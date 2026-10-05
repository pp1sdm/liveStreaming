//! 一场直播的会话状态。
//!
//! 把 [`Round`](livestream_engine::Round)（一道题）和直播层面的东西（题库游标、
//! 作答滚动条、上一题统计）合到一起，并且负责把它翻译成画面要的
//! [`OverlayState`]。
//!
//! 有一条纪律：**答案在公布之前不下发到页面。** 画面不需要它，提前给出去
//! 只会多一个泄题的口子。

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use livestream_core::{Choice, Danmaku, Question};
use livestream_engine::{Award, Leaderboard, Outcome, Phase, RankRow, Round, Stats};
use serde::Serialize;

/// 抢答榜留几个位置。
///
/// **只留最先答的那几个，不留最近的。** 原来是「最近 8 条」，谁刚答谁在
/// 上面滚；现在是「最先 10 名」，答满就封榜。两个理由：
///
/// 一是画面：公布那一刻，榜上这几张卡片要飞到上面自己选的选项里去
/// （见 `overlay.html` 的 `flyPicks`）。这要求榜上的人是**确定的一批**——
/// 飞的过程中榜单还在滚的话，卡片飞到一半换了主人。
/// 二是直播：抢答榜得有抢的意味。人人有份的榜不叫榜。
///
/// 代价是收答后期榜单不再动。这是故意的，不是卡住了——标题从
/// 「正在作答」变成「抢答榜 · 선착순」就是在说这件事。
const TICKER_LEN: usize = 10;

/// 到这个作答人数，分布就改用百分比显示，不再报绝对人数。
///
/// 人数少的时候「3 人答对」比「30% 答对」有用——分母太小，百分比是噪声；
/// 人数一多就反过来，「137 人答对」没人读得出比例。100 是用户定的线。
const PERCENT_AT: u32 = 100;

/// 画面上摆几名。OBS 那块地方只够这个数，再多就糊了。
pub const BOARD_LEN: usize = 5;

/// 抢答榜上的一条。
#[derive(Debug, Clone, Serialize)]
pub struct TickerItem {
    /// 抢答名次，从 1 起。榜上就十个位置，所以它同时是「第几个答的」。
    pub rank: u32,
    pub nickname: String,
    /// 观众选了什么。收答期间是 `None`——先亮出来等于给后来的人抄。
    pub choice: Option<char>,
    pub correct: Option<bool>,
}

/// 全场答题分布。**只在公布之后才有**，理由同 `OverlayState::tally`。
///
/// 三档：答对、答错、没答。前两档是本题作答的人，第三档是「来过直播间
/// 但这一题没出手的人」——用累计玩过的人减去本题作答人数算的。
///
/// 第三档要知道它的分寸：分母是**整场累计**玩过的人，中途走掉的人也留在
/// 里面。所以这个数只会涨不会跌，它是「越来越多人只看不答」的长期指标，
/// 不是「此刻有多少人在潜水」。画面上的标签就写「未作答」，不写「在线」——
/// 后者是我们根本不知道的数，写出来就是编。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Distribution {
    /// 答对人数。
    pub correct: u32,
    /// 答错人数。
    pub wrong: u32,
    /// 没作答的人数。
    pub absent: u32,
    /// 用百分比还是用绝对人数。见 [`PERCENT_AT`]。
    pub percent: bool,
}

/// 下发给画面的题目。公布之前不带答案。
#[derive(Debug, Clone, Serialize)]
pub struct QuestionView {
    pub id: u64,
    pub prompt: String,
    pub options: [String; 4],
    pub correct: Option<Choice>,
}

/// 一条要念出来的语音，附字幕。
///
/// **只带一个 url，不带音频字节。** 音频单独走 `GET /voice/{id}`：一段三秒的
/// MP3 有二十来 KB，塞进"每次变化整包推"的状态里，等于每次推送都复制几十 KB，
/// 还得 base64 才能过 JSON。更要命的是讲解词里含正确答案——它一旦进了状态，
/// 就等于在收答期间把答案推给了画面。
#[derive(Debug, Clone, Serialize)]
pub struct SpeechView {
    /// 取件码，同时也是 `/voice/{id}` 的路径。页面靠它去重，避免重连时重播。
    pub id: String,
    /// `intro` / `reveal` / `chatter` / `narration`。页面按它决定字幕样式。
    pub kind: String,
    /// 音频地址，相对路径。
    pub url: String,
    /// 字幕文本。
    pub text: String,
    /// 服务端按字节估的时长。页面只拿它做兜底超时——真正播完靠页面回报。
    pub duration_ms: u64,
}

/// 画面的完整状态。每次变化整包推给浏览器，前端只管重画。
#[derive(Debug, Clone, Serialize)]
pub struct OverlayState {
    pub phase: Phase,
    pub question: QuestionView,
    /// 本场第几题，从 1 起。
    pub index: u32,
    /// 本场一共多少题。画面用它显示「第 3 / 30 题」。
    pub total: u32,
    /// 四个选项的票数。**收答期间是 `None`。**
    ///
    /// 这条和上面 `TickerItem::choice` 是同一个理由，但更要紧：票数分布
    /// 是最直接的跟风燃料——「B 已经 12 票了」一露出来，后面的人就不用懂
    /// 韩语了，跟着按 B 就行，答对率会虚高到没有意义。
    ///
    /// 之前这里是 `[u32; 4]` 裸着推的。两个页面碰巧都在用公布后的
    /// `stats.tally`，所以画面上看不出来——但数据是**真的推给了每一个
    /// 连着画面的客户端**的，改天谁加个实时条形图、或者有人开 devtools
    /// 看一眼 SSE，就全漏了。不能靠"现在没人用"来保证安全。
    pub tally: Option<[u32; 4]>,
    pub participants: u32,
    /// 收答截止时刻，Unix 毫秒。前端拿它自己跑倒计时，比每秒推一次顺滑。
    pub ends_at_ms: u64,
    /// 收答窗口总长，毫秒。倒计时环用它算进度。
    pub window_ms: u64,
    pub ticker: Vec<TickerItem>,
    /// 全场答题分布。**收答期间是 `None`。**
    ///
    /// 和 `tally` 一样是防跟风的：答对率本身就是答案的强提示——一题里
    /// 八成人都答对，那多数人选的那个就是对的。所以它和票数分布同一时刻
    /// 出现，早一毫秒都不行。
    pub distribution: Option<Distribution>,
    /// 公布之后才有。
    pub stats: Option<Stats>,
    /// 累计榜前几名。只反映已经公布过的题，所以收答期间不会泄题。
    pub leaderboard: Vec<RankRow>,
    /// 本题的得分明细，公布那一刻的反馈。公布前是空的。
    pub awards: Vec<Award>,
    /// 上榜人数（至少答对过一题的人）。
    pub ranked_count: u32,
    /// 当前该念的那句话。没有就是 `None`，页面保持安静。
    ///
    /// **公布前这里绝不能是讲解词**——`kind: "reveal"` 的语音含正确答案，
    /// 只有阶段真的变成 `revealed` 才允许下发。这条由 director 保证，
    /// 并由 `session` 的测试守着。
    pub speech: Option<SpeechView>,
}

/// 主播按一次「继续」，实际推动了什么。
///
/// 回给控制台，让它知道那一下有没有真的走成——按钮文案也跟着换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// 报题 → 收答。旁白念完（或手动跳过）开始计时。
    Started,
    /// 收答 → 封盘。
    Locked,
    /// 封盘 → 公布。
    Revealed,
    /// 公布 → 下一题。
    Advanced,
}

/// 一场直播。
pub struct Session {
    bank: Vec<Question>,
    cursor: usize,
    window: Duration,
    round: Round,
    ticker: VecDeque<TickerItem>,
    stats: Option<Stats>,
    /// 整场的累计积分，跨题累加。
    leaderboard: Leaderboard,
    /// 本题的得分明细。换题时清空。
    awards: Vec<Award>,
    /// 把 `Instant` 换算成 Unix 毫秒用的基准。
    epoch: (Instant, u64),
    /// 当前该念的那句话。
    speech: Option<SpeechView>,
}

impl Session {
    /// 开一场直播，立刻出第一道题。`bank` 不能为空。
    pub fn new(bank: Vec<Question>, window: Duration, now: Instant) -> anyhow::Result<Self> {
        let first = bank
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("题库是空的，至少需要一道题"))?;

        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Ok(Self {
            bank,
            cursor: 0,
            window,
            round: Round::open(first, window, now),
            ticker: VecDeque::with_capacity(TICKER_LEN),
            stats: None,
            leaderboard: Leaderboard::new(),
            awards: Vec::new(),
            epoch: (now, unix_now),
            speech: None,
        })
    }

    /// 换到下一题，但**先不计时**——等 AI 把题干念完再开表。
    ///
    /// 和 [`Self::advance`] 的区别只在这里：那个立刻开始收答，是给手动主持
    /// 用的；AI 主持走这条。理由见 [`Round::present`]。
    pub fn prepare_next(&mut self, now: Instant) {
        self.cursor = (self.cursor + 1) % self.bank.len();
        self.round = Round::present(self.bank[self.cursor].clone(), self.window, now);
        self.ticker.clear();
        self.stats = None;
        // 榜单留着，这是整场的累计；只清掉上一题的得分明细。
        self.awards.clear();
        self.speech = None;
    }

    /// 旁白念完了，开始计时。返回是否真的开了表。
    pub fn begin(&mut self, now: Instant) -> bool {
        if !self.round.is_presenting() {
            return false;
        }
        self.round.start_collecting(now);
        true
    }

    /// 把**当前这道题**退回「报题中」，等旁白念完再开表。
    ///
    /// 开场专用。[`Self::new`] 出来是直接收答的（手动主持的路径），
    /// 但 AI 主持的第一题也得先念题——不然观众还没听清就开始倒计时了，
    /// 计分不公平。只有开场该用它；换题走 [`Self::prepare_next`]。
    pub fn present_current(&mut self, now: Instant) {
        self.round = Round::present(self.round.question.clone(), self.window, now);
        self.ticker.clear();
        self.stats = None;
        self.awards.clear();
        self.speech = None;
    }

    /// 往题库里追加一道题（AI 现出的）。
    ///
    /// 题号在这里统一分配，而不是让调用方给——调用方（director）不知道
    /// 题库里已经有哪些号，撞号的话画面上的「第 N 题」和榜单的归属都会错。
    pub fn push_question(&mut self, mut question: Question) {
        let next = self.bank.iter().map(|q| q.id).max().unwrap_or(0) + 1;
        question.id = next;
        self.bank.push(question);
    }

    /// 题库是不是快见底了。director 用它决定要不要提前再出一道。
    ///
    /// 看的是「从当前这道往后还剩几道」——循环播放的题不算新内容，
    /// 观众第二次看到同一道题就换台了。
    pub fn bank_running_low(&self, low_water: usize) -> bool {
        self.bank.len().saturating_sub(self.cursor + 1) < low_water
    }

    /// 换一句要念的话。传 `None` 就是让页面安静下来。
    pub fn set_speech(&mut self, speech: Option<SpeechView>) {
        self.speech = speech;
    }

    /// 现在该念的那句话。
    pub fn speech(&self) -> Option<&SpeechView> {
        self.speech.as_ref()
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    pub fn phase(&self) -> Phase {
        self.round.phase
    }

    /// 当前这道题。出题和讲解都要拿它。
    pub fn round_question(&self) -> Question {
        self.round.question.clone()
    }

    /// 离封盘还有多久。`Presenting` 期间恒等于整个窗口（还没开始计时）。
    pub fn remaining(&self) -> Duration {
        self.round.remaining(Instant::now())
    }

    /// 本题已作答人数。
    pub fn participants(&self) -> u32 {
        self.round.answers.len() as u32
    }

    /// 本题的票数分布。**只在公布后调用**——它含答案信息。
    pub fn stats(&self) -> Option<Stats> {
        self.stats.clone()
    }

    /// 收下一条弹幕。窗口关着的话会被 [`Outcome::Ignored`] 掉——
    /// 这就是「封盘」，弹幕源照推，这里不看。
    pub fn handle(&mut self, d: &Danmaku, now: Instant) -> Outcome {
        let outcome = self.round.submit(d, now);

        if let Outcome::Counted(choice) = outcome {
            let correct = choice == self.round.question.correct;
            // **答满就封榜。** 上面的人数判断不能省：`push_back` 之后
            // 无条件截断的话，被砍掉的是**最后**一个，等于榜上永远是最新
            // 十条——正好是这次要改掉的那套「滚动条」语义。
            if self.ticker.len() < TICKER_LEN {
                self.ticker.push_back(TickerItem {
                    // 名次按到达顺序发，和 `push_back` 是同一次推入，
                    // 所以它就是「第几个答的」。服务端的 `answers` 已经是
                    // 去重的（一人一票），跟着它走不会重号。
                    rank: self.ticker.len() as u32 + 1,
                    nickname: d.nickname.clone(),
                    choice: Some(choice.letter()),
                    correct: Some(correct),
                });
            }
        }

        outcome
    }

    /// 换下一道题。题库循环使用。
    pub fn advance(&mut self, now: Instant) {
        self.cursor = (self.cursor + 1) % self.bank.len();
        self.round = Round::open(self.bank[self.cursor].clone(), self.window, now);
        self.ticker.clear();
        self.stats = None;
        // 榜单留着，这是整场的累计；只清掉上一题的得分明细。
        self.awards.clear();
        // 上一题的语音不能留在状态里——换题了还在念旧题，听起来像卡带。
        self.speech = None;
    }

    /// 收答时间到了就封盘。返回是否发生了封盘。
    pub fn auto_lock_if_expired(&mut self, now: Instant) -> bool {
        if self.round.is_collecting() && self.round.expired(now) {
            self.round.lock();
            return true;
        }
        false
    }

    pub fn lock(&mut self) {
        self.round.lock();
    }

    /// 公布答案，同时结算本题得分。重复调用只返回第一次的统计。
    ///
    /// **计分挂在这里而不是 [`Self::handle`]**：收答期间就算分的话，榜上冒出来
    /// 的人名本身就是答案。
    pub fn reveal(&mut self) -> Stats {
        // 已经结算过就直接给上次那份。写成提前返回而不是「先判断、末尾
        // expect 一定写入过」——那个 expect 虽然到不了，但它是个持锁期间
        // 的 panic 点，而持锁 panic 会把会话锁弄中毒。这里干脆让它不存在。
        if let Some(done) = &self.stats {
            return done.clone();
        }

        let stats = self.round.reveal();
        self.awards = self
            .leaderboard
            .record_round(&self.round.answers, stats.correct);
        self.stats = Some(stats.clone());
        stats
    }

    /// 按当前阶段往前走一步。这是主播唯一需要按的键。
    ///
    /// 以前是三个按钮各绑一个阶段，封盘之后只剩「公布」能动，而它排在最后、
    /// 也没有样式——看上去整个页面就卡死了。合成一个键之后不存在走不出去的
    /// 岔路：每个阶段恰好只有一个正确的下一步。
    ///
    /// 收答 → 封盘 → 公布 → 下一题，转一圈回到收答。
    pub fn step(&mut self, now: Instant) -> Step {
        match self.round.phase {
            // 念题期间按「继续」= 跳过剩下的旁白、直接开表。
            // 语音那边卡住时靠它救场，主播不必干等。
            Phase::Presenting => {
                self.round.start_collecting(now);
                Step::Started
            }
            Phase::Collecting => {
                self.round.lock();
                Step::Locked
            }
            Phase::Locked => {
                self.reveal();
                Step::Revealed
            }
            Phase::Revealed => {
                self.advance(now);
                Step::Advanced
            }
        }
    }

    /// 整场累计榜的前几名。
    pub fn top_players(&self, n: usize) -> Vec<RankRow> {
        self.leaderboard.top(n)
    }

    /// 上榜人数：至少答对过一题的人。
    pub fn ranked_count(&self) -> usize {
        self.leaderboard.ranked_count()
    }

    /// 进来答过题的总人数，包括一题没对的。
    pub fn player_count(&self) -> usize {
        self.leaderboard.player_count()
    }

    /// 被丢掉的弹幕，供控制台调优判定。
    pub fn rejections(&self) -> &[(String, String, livestream_engine::Rejection)] {
        self.round.rejections()
    }

    pub fn question_index(&self) -> usize {
        self.cursor
    }

    pub fn bank_len(&self) -> usize {
        self.bank.len()
    }

    /// 产出画面的状态。
    ///
    /// 刻意不带「还剩多少毫秒」——只给截止时刻，倒计时由前端自己跑。
    /// 每秒从服务端推一次倒计时，画面会一跳一跳的。
    pub fn snapshot(&self) -> OverlayState {
        let revealed = self.round.phase == Phase::Revealed;

        OverlayState {
            phase: self.round.phase,
            question: QuestionView {
                id: self.round.question.id,
                prompt: self.round.question.prompt.clone(),
                options: self.round.question.options.clone(),
                // 公布之前不给答案。
                correct: revealed.then_some(self.round.question.correct),
            },
            index: self.cursor as u32 + 1,
            total: self.bank.len() as u32,
            // 公布之前不发票数分布，理由见字段注释。
            tally: revealed.then_some(self.round.tally),
            participants: self.round.answers.len() as u32,
            ends_at_ms: self.to_epoch_ms(self.round.deadline),
            window_ms: self.window.as_millis() as u64,
            ticker: self
                .ticker
                .iter()
                .map(|t| {
                    if revealed {
                        t.clone()
                    } else {
                        // 收答期间只报「谁答了」，不报选了什么。
                        TickerItem {
                            rank: t.rank,
                            nickname: t.nickname.clone(),
                            choice: None,
                            correct: None,
                        }
                    }
                })
                .collect(),
            // 和 tally 同一时刻出现、同一时刻消失，理由见字段注释。
            distribution: revealed.then(|| self.distribution()),
            stats: self.stats.clone(),
            leaderboard: self.leaderboard.top(BOARD_LEN),
            // awards 只在公布时产生，所以收答期间天然是空的。
            awards: self.awards.clone(),
            ranked_count: self.leaderboard.ranked_count() as u32,
            speech: self.speech.clone(),
        }
    }

    /// 全场答题分布。**含答案信息，只在公布后调用。**
    ///
    /// 三档加起来是「整场累计玩过的人」：本题作答的记进答对/答错，
    /// 剩下的记进未作答。所以三档相加恒等于 `player_count()`——
    /// 画面上的三条永远不会加起来超过 100%，那不是巧合，是这里保证的。
    fn distribution(&self) -> Distribution {
        let participants = self.round.answers.len() as u32;
        // 用 `Choice::index()`，**不要**写 `as usize - 'A' as usize`：
        // `Choice` 是 0..3 的无字段枚举，`as usize` 出来就是 0，减去 'A'
        // 直接下溢。debug 构建下这是当场 panic，而且 panic 发生在持锁期间
        // 会把会话锁弄中毒——一题公布就把整场直播带走。
        let correct = self.round.tally[self.round.question.correct.index()];
        Distribution {
            correct,
            // 作答人数减去答对人数，不是拿 tally 里其余三项相加——两者相等，
            // 但这样写少一次遍历，而且以后 `Round` 若允许改票，也不会忽然
            // 算出「答错的人比答过的人还多」。
            wrong: participants.saturating_sub(correct),
            absent: (self.leaderboard.player_count() as u32).saturating_sub(participants),
            // 按本题作答人数决定，不按累计人数——题目问的是「这一题大家答得
            // 怎么样」，分母小的时候「3 人答对」比「30%」有用得多。
            percent: participants >= PERCENT_AT,
        }
    }

    /// `Instant` 不能直接变成 Unix 时间，用启动时记下的基准换算。
    fn to_epoch_ms(&self, t: Instant) -> u64 {
        let (base, unix) = self.epoch;
        if t >= base {
            unix + (t - base).as_millis() as u64
        } else {
            unix.saturating_sub((base - t).as_millis() as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank() -> Vec<Question> {
        vec![
            Question {
                id: 1,
                prompt: "Q1".into(),
                options: ["가".into(), "나".into(), "다".into(), "라".into()],
                correct: Choice::A,
            },
            Question {
                id: 2,
                prompt: "Q2".into(),
                options: ["가".into(), "나".into(), "다".into(), "라".into()],
                correct: Choice::B,
            },
        ]
    }

    fn danmaku(user: &str, content: &str) -> Danmaku {
        Danmaku {
            seq: 0,
            user_id: user.into(),
            nickname: user.into(),
            content: content.into(),
        }
    }

    fn session() -> Session {
        Session::new(bank(), Duration::from_secs(60), Instant::now()).unwrap()
    }

    #[test]
    fn empty_bank_is_rejected() {
        assert!(Session::new(vec![], Duration::from_secs(60), Instant::now()).is_err());
    }

    #[test]
    fn answer_is_hidden_until_revealed() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);

        assert!(s.snapshot().question.correct.is_none());
        s.reveal();
        assert_eq!(s.snapshot().question.correct, Some(Choice::A));
    }

    #[test]
    fn tally_is_hidden_until_revealed() {
        // **回归测试。** 票数分布是最直接的跟风燃料——收答期间一旦露出
        // 「B 已经 12 票了」，后面的人不用懂韩语，跟着按 B 就行，
        // 答对率会虚高到没有意义。
        //
        // 原来 `OverlayState.tally` 是裸着 `[u32; 4]` 推给每一个连着画面的
        // 客户端的。两个页面碰巧都在用公布后的 `stats.tally`，所以画面上
        // 看不出来——但数据是真的在线上跑的，谁都收得到。
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);
        s.handle(&danmaku("u2", "B"), t0);
        s.handle(&danmaku("u3", "B"), t0);

        let live = s.snapshot();
        assert!(live.tally.is_none(), "收答期间绝不能推送票数分布");
        assert_eq!(live.participants, 3, "人数可以报——那是气氛，不是答案");

        s.reveal();
        assert_eq!(
            s.snapshot().tally,
            Some([1, 2, 0, 0]),
            "公布之后要给出真实分布，讲解词靠它"
        );
    }

    #[test]
    fn ticker_keeps_the_first_ten_and_then_closes() {
        // **回归测试。** 这里原来是 `push_front` + `truncate(8)`，
        // 榜上留的是**最近**答的几条，谁刚答谁在最前面滚。
        //
        // 现在要的是「最先 10 名」：公布那一刻榜上这几张卡片要飞到上面
        // 自己选的选项里去（见 overlay.html 的 `flyPicks`）。飞行途中榜还在
        // 滚的话，卡片会飞到一半换主人——所以榜必须是确定的一批人。
        let t0 = Instant::now();
        let mut s = session();
        for i in 1..=15 {
            s.handle(&danmaku(&format!("u{i}"), "A"), t0);
        }

        let live = s.snapshot();
        assert_eq!(live.ticker.len(), TICKER_LEN, "榜上就该正好十个位置");
        assert_eq!(live.participants, 15, "答题人数照数，封榜只影响展示");

        let names: Vec<&str> = live.ticker.iter().map(|t| t.nickname.as_str()).collect();
        assert_eq!(
            names,
            (1..=10).map(|i| format!("u{i}")).collect::<Vec<_>>(),
            "留在榜上的必须是**最先**答的十个，不是最后十个"
        );
        let ranks: Vec<u32> = live.ticker.iter().map(|t| t.rank).collect();
        assert_eq!(ranks, (1..=10).collect::<Vec<u32>>(), "名次该是 1..10");
    }

    #[test]
    fn distribution_hides_until_revealed() {
        // 和票数分布同一个道理，而且更直接：一题里八成人都答对，
        // 「多数人选的那个」就是答案。收答期间露出去，等于把答案念了一遍。
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);
        s.handle(&danmaku("u2", "B"), t0);

        assert!(
            s.snapshot().distribution.is_none(),
            "收答期间绝不能推送答题分布"
        );

        s.reveal();
        assert!(
            s.snapshot().distribution.is_some(),
            "公布之后要给出分布，画面靠它画那条三段的条"
        );
    }

    #[test]
    fn distribution_buckets_add_up_to_the_player_count() {
        // 三条加起来必须正好是「累计玩过的人」。多出来或少了，
        // 画面上就会出现「答对 40% + 答错 40% + 未作答 30%」这种加起来
        // 超过 100% 的东西——那是错的，而且错得很显眼。
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0); // 对
        s.handle(&danmaku("u2", "B"), t0); // 错
        s.handle(&danmaku("u3", "C"), t0); // 错
        s.reveal();

        let d = s.snapshot().distribution.expect("公布后该有分布");
        assert_eq!(d.correct, 1);
        assert_eq!(d.wrong, 2);
        assert_eq!(d.absent, 0, "三个人都答了，没有未作答的");
        assert_eq!(d.correct + d.wrong + d.absent, 3);

        // 换一题，上一题答过的三个人都没出手 → 全部落进未作答。
        s.advance(t0);
        s.reveal();
        let d = s.snapshot().distribution.expect("公布后该有分布");
        assert_eq!((d.correct, d.wrong), (0, 0));
        assert_eq!(d.absent, 3, "老观众这一题没答，要算进未作答");
    }

    #[test]
    fn distribution_switches_to_percent_at_a_hundred_answers() {
        // 人数少的时候「3 人答对」比「30% 答对」有用——分母太小，百分比是
        // 噪声；人数一多就反过来，「137 人答对」没人读得出比例。
        let t0 = Instant::now();

        let mut small = session();
        for i in 0..(PERCENT_AT - 1) {
            small.handle(&danmaku(&format!("u{i}"), "A"), t0);
        }
        small.reveal();
        assert!(
            !small.snapshot().distribution.unwrap().percent,
            "{} 人还没到线，该报人数",
            PERCENT_AT - 1
        );

        let mut big = session();
        for i in 0..PERCENT_AT {
            big.handle(&danmaku(&format!("u{i}"), "A"), t0);
        }
        big.reveal();
        assert!(
            big.snapshot().distribution.unwrap().percent,
            "到了 {PERCENT_AT} 人就该换成百分比"
        );
    }

    #[test]
    fn ticker_hides_choice_while_collecting() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);

        let live = s.snapshot();
        assert_eq!(live.ticker.len(), 1);
        assert!(live.ticker[0].choice.is_none(), "收答期间不该暴露选项");

        s.reveal();
        let done = s.snapshot();
        assert_eq!(done.ticker[0].choice, Some('A'));
        assert_eq!(done.ticker[0].correct, Some(true));
    }

    #[test]
    fn advance_cycles_bank_and_resets_round() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);
        assert_eq!(s.snapshot().participants, 1);

        s.advance(t0);
        let st = s.snapshot();
        assert_eq!(st.question.id, 2);
        assert_eq!(st.participants, 0);
        assert!(st.ticker.is_empty());
        assert!(st.stats.is_none());

        s.advance(t0);
        assert_eq!(s.snapshot().question.id, 1, "应该循环回第一题");
    }

    #[test]
    fn auto_lock_closes_the_window() {
        let t0 = Instant::now();
        let window = Duration::from_secs(60);
        let mut s = Session::new(bank(), window, t0).unwrap();

        assert!(!s.auto_lock_if_expired(t0));
        assert!(s.auto_lock_if_expired(t0 + window));
        assert_eq!(s.phase(), Phase::Locked);

        // 封盘之后弹幕进不来。
        assert_eq!(s.handle(&danmaku("u1", "A"), t0 + window), Outcome::Ignored);
    }

    #[test]
    fn scoring_waits_for_the_reveal() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);

        // 收答期间榜上冒出人名，等于把答案说出去了。
        let live = s.snapshot();
        assert!(live.leaderboard.is_empty(), "公布前不该有人上榜");
        assert!(live.awards.is_empty());
        assert_eq!(live.ranked_count, 0);

        s.reveal();
        let done = s.snapshot();
        assert_eq!(done.leaderboard.len(), 1);
        assert_eq!(done.awards.len(), 1);
        assert_eq!(done.awards[0].nickname, "u1");
        assert_eq!(done.awards[0].rank, 1);
    }

    #[test]
    fn leaderboard_accumulates_across_rounds() {
        let t0 = Instant::now();
        let mut s = session();

        s.handle(&danmaku("u1", "A"), t0); // 第 1 题正确答案是 A
        s.reveal();
        let after_first = s.top_players(1)[0].clone();

        s.advance(t0);
        s.handle(&danmaku("u1", "B"), t0); // 第 2 题正确答案是 B
        s.reveal();
        let after_second = s.top_players(1)[0].clone();

        assert_eq!(after_second.correct, 2, "跨题累计");
        assert!(after_second.score > after_first.score);
        assert_eq!(after_second.streak, 2);
    }

    #[test]
    fn advancing_clears_awards_but_keeps_the_board() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);
        s.reveal();
        assert_eq!(s.snapshot().awards.len(), 1);

        s.advance(t0);
        let st = s.snapshot();
        assert!(st.awards.is_empty(), "换题后得分明细要清掉");
        assert_eq!(st.leaderboard.len(), 1, "累计榜要留着");
    }

    #[test]
    fn revealing_twice_does_not_double_score() {
        // 控制台的「公布」按钮和超时自动公布可能撞在一起。
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0);

        s.reveal();
        let once = s.snapshot().leaderboard[0].score;
        s.reveal();

        assert_eq!(s.snapshot().leaderboard[0].score, once, "重复公布不该重复计分");
        assert_eq!(s.snapshot().awards.len(), 1);
    }

    #[test]
    fn ends_at_ms_stays_in_the_future() {
        let t0 = Instant::now();
        let s = session();
        let _ = t0;
        let state = s.snapshot();
        // 基准是真实时钟，所以截止时刻应该是「现在 + 60 秒」量级。
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(state.ends_at_ms > now_ms, "截止时刻应该在未来");
        assert!(state.ends_at_ms < now_ms + 120_000);
    }

    #[test]
    fn step_walks_the_whole_cycle_without_dead_ends() {
        // 「继续」键的价值就在这里：每个阶段都有且只有一个下一步，
        // 连按四次必然回到起点，中途不会停在某个按不动的状态上。
        let t0 = Instant::now();
        let mut s = session();

        assert_eq!(s.phase(), Phase::Collecting);
        assert_eq!(s.step(t0), Step::Locked);
        assert_eq!(s.phase(), Phase::Locked);
        assert_eq!(s.step(t0), Step::Revealed);
        assert_eq!(s.phase(), Phase::Revealed);
        assert_eq!(s.step(t0), Step::Advanced);
        assert_eq!(s.phase(), Phase::Collecting, "转一圈应该回到收答");
        assert_eq!(s.step(t0), Step::Locked, "第二圈照走不误");
    }

    #[test]
    fn stepping_from_collecting_still_settles_scores() {
        // 主播不封盘、直接连按两下也应该正常结算，不能跳过计分。
        let t0 = Instant::now();
        let mut s = session();
        s.handle(&danmaku("u1", "A"), t0); // 第一题的正确答案是 A

        s.step(t0); // 封盘
        s.step(t0); // 公布

        let st = s.snapshot();
        assert_eq!(st.awards.len(), 1, "走「继续」也要结算得分");
        assert_eq!(st.leaderboard[0].score, 15, "基础 10 + 手速 5");
    }
}
