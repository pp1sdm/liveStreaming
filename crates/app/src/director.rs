//! 导演：替掉那个盯着后台的人。
//!
//! # 它做的事
//!
//! 按阶段自动推进整场直播——报题、收答、封盘、公布、下一题，循环往复——
//! 并且在每个节点安排该说的那句话。人不在场。
//!
//! ```text
//!   Presenting ──报题语音念完──► Collecting ──到点──► Locked ──► Revealed
//!        ▲                                                          │
//!        └──────────── 讲解念完 → 串场念完 → 换题 ────────────────────┘
//! ```
//!
//! # 四条纪律
//!
//! 1. **绝不持锁 await。** DeepSeek 要一两秒，TTS 也要一两秒。会话锁一旦跨
//!    await 持有，整个 HTTP 层和画面推送全卡住。所以所有生成都在后台任务里做，
//!    结果通过 channel 送回这个循环，循环本身只做「锁内取数据、放锁、决策」。
//!
//! 2. **讲解稿在公布前不进画面。** 它含正确答案。生成好之后压在
//!    `pending_reveal` 里，只有阶段真的变成 `Revealed` 才排进播放队列。
//!
//! 3. **任何一环挂掉都不能让直播停。** 没 key、API 报错、TTS 失败、页面不回
//!    回报——每一条都有兜底，最差也能用本地台词把整场走完。
//!
//! 4. **过期的产物一律丢掉。** 后台任务可能在换题之后才返回。语音认题号，
//!    对不上就扔——否则观众会看到上一题的讲解配这一题的答案。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use livestream_ai::script::{self, GeneratedQuestion, RevealFacts, RoomDigest};
use livestream_ai::{DeepSeek, Segment, Tts, Voices};
use livestream_core::Question;
use livestream_engine::Phase;
use livestream_web::{App, AudioId, SpeechView};
use tokio::sync::mpsc;

/// 报题阶段最长等多久。语音一直没回来（TTS 挂了、页面没连上）也得开表，
/// 否则整场直播就停在这儿了。
const PRESENT_TIMEOUT: Duration = Duration::from_secs(25);

/// 等页面回报「念完了」时，比估算时长多等这么久就兜底放行。
///
/// 页面可能崩了、OBS 可能被关了、回报可能丢在路上。等不到就自己往下走，
/// 顶多是两句话叠在一起，比整场卡死强得多。
const SPEECH_SLACK: Duration = Duration::from_secs(3);

/// 等公布语音合成出来最长等多久。
///
/// 这段是现场合成的（票数分布封盘那一刻才存在），正常一两秒。等太久说明
/// TTS 出问题了，不能再干等——画面停在公布状态不动，观众会以为卡了。
const REVEAL_SYNTH_TIMEOUT: Duration = Duration::from_secs(12);

/// 串场最长等多久。生成失败时靠它把流程推走，不至于一直停在公布画面上。
const INTERLUDE_TIMEOUT: Duration = Duration::from_secs(10);

/// 两条弹幕互动之间至少隔这么久。
///
/// 这是「控场」，不是「聊天机器人」。说太密会盖过题目本身，
/// 而且观众很快就会发现它只是在没话找话。
const CHATTER_INTERVAL: Duration = Duration::from_secs(25);

/// 题库剩多少道就开始提前生成下一道。
const BANK_LOW_WATER: usize = 2;

/// 出过的题最多记这么多，用于让模型别重复。
const AVOID_KEEP: usize = 30;

/// 后台送来的一条消息：合成好了，或者合成挂了。
///
/// **失败也要报回来。** 只报成功的话，「等报题语音念完」就永远等不到——
/// TTS 一挂，每道题都要硬等满 [`PRESENT_TIMEOUT`]，直播变成一分钟一步。
/// 报回来才能立刻决定「这句没有，跳过」。
struct Ready {
    kind: &'static str,
    audio: Vec<u8>,
    text: String,
    /// 这条属于哪道题。换题之后才回来的就作废。
    for_question: u64,
    /// 合成失败——没有可播的东西，别等它。
    failed: bool,
}

impl Ready {
    fn new(kind: &'static str, audio: Vec<u8>, text: String, for_question: u64) -> Self {
        Self { kind, audio, text, for_question, failed: false }
    }

    /// 合成失败。只带「哪句话没了」，让编排层跳过等待。
    fn failed(kind: &'static str, for_question: u64) -> Self {
        Self { kind, audio: Vec::new(), text: String::new(), for_question, failed: true }
    }

    fn duration(&self) -> Duration {
        Duration::from_millis(livestream_ai::tts::estimate_ms(&self.audio))
    }
}

/// 正在播的那条。
struct Playing {
    id: AudioId,
    kind: &'static str,
    /// 到这个时刻还没等到回报就兜底放行。
    give_up_at: Instant,
}

/// 公布之后要走的两步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Curtain {
    /// 讲解在播（事实部分 + 提前生成的那段拼在一起）。
    Explaining,
    /// 讲解念完了，串场在生成/播放。
    Interlude,
}

/// 导演。
pub struct Director {
    app: App,
    /// 没有 key 就是 `None`，整场走本地台词。
    ///
    /// 用 `Arc` 是因为每个后台任务都要自己拿一份：客户端里就是几个 `String`
    /// 加一个自带连接池的 `reqwest::Client`，克隆引用比每次重建省得多。
    ai: Option<Arc<DeepSeek>>,
    tts: Arc<dyn Tts>,
    voices: Voices,

    /// 页面回报「念完了」。
    spoken: mpsc::UnboundedReceiver<AudioId>,
    /// 后台任务合成好的语音送回来。
    ready_rx: mpsc::UnboundedReceiver<Ready>,
    ready_tx: mpsc::UnboundedSender<Ready>,
    /// 后台出好的题送回来；`None` 表示这次没出成，仅用于解除「正在出题」标记。
    question_rx: mpsc::UnboundedReceiver<Option<Question>>,
    question_tx: mpsc::UnboundedSender<Option<Question>>,

    /// 等着播的队列，先进先出。
    queue: VecDeque<Ready>,
    playing: Option<Playing>,

    /// 本题讲解的音频，提前合成好压在这里，公布时才放出去。
    pending_reveal: Option<Ready>,
    /// 已经为哪道题排过讲解生成，避免重复调用。
    reveal_for: Option<u64>,
    /// 报题语音是否已经播完。
    intro_played: bool,
    /// 进入报题阶段的时刻，用来算兜底超时。
    presenting_since: Option<(u64, Instant)>,

    /// 公布之后走到哪一步了，以及是哪道题的。
    curtain: Curtain,
    curtain_for: Option<u64>,
    /// 讲解真的开播过。
    ///
    /// 不能只看「队列空不空」——`step_locked` 刚 spawn 完合成任务，
    /// 下一个 tick 队列本来就是空的，会被误判成「已经念完了」，
    /// 于是串场排到讲解前面去。必须等它**真的播过一轮**。
    spoke_reveal: bool,
    /// 等讲解合成的硬截止。合成一直不回来也不能干等。
    curtain_until: Instant,
    /// 串场念完了（或判定它不会来了）。
    narration_done: bool,
    /// 串场阶段的硬截止。
    interlude_until: Instant,

    /// 下一次可以互动的时刻。
    next_chatter: Instant,
    /// 收答期攒下的、已消毒的弹幕样本。
    chatter_buf: Vec<String>,
    /// 正在后台生成互动，避免同时排好几个。
    chatter_pending: bool,

    /// 已经出过的题干，用来让模型别重复。
    asked: Vec<String>,
    /// 正在后台出题。
    question_pending: bool,
}

impl Director {
    pub fn new(
        app: App,
        ai: Option<Arc<DeepSeek>>,
        tts: Arc<dyn Tts>,
        voices: Voices,
        spoken: mpsc::UnboundedReceiver<AudioId>,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::unbounded_channel();
        let (question_tx, question_rx) = mpsc::unbounded_channel();
        let now = Instant::now();
        Self {
            app,
            ai,
            tts,
            voices,
            spoken,
            ready_rx,
            ready_tx,
            question_rx,
            question_tx,
            queue: VecDeque::new(),
            playing: None,
            pending_reveal: None,
            reveal_for: None,
            intro_played: false,
            presenting_since: None,
            curtain: Curtain::Explaining,
            curtain_for: None,
            spoke_reveal: false,
            curtain_until: now,
            narration_done: false,
            interlude_until: now,
            next_chatter: now,
            chatter_buf: Vec::new(),
            chatter_pending: false,
            asked: Vec::new(),
            question_pending: false,
        }
    }

    /// 开场：让第一题也走「先报题、后计时」，跟后面每一题一样。
    pub fn start(&mut self) {
        self.app.present_current();
    }

    /// 收一条弹幕，攒着给互动用。
    ///
    /// 攒的是**消毒过**的样本，不是原文。含答案痕迹、广告链接的在
    /// [`script::sanitize_danmaku`] 那里就被丢掉了——模型拿不到危险的东西，
    /// 就不可能把它说出来。
    pub fn observe_danmaku(&mut self, content: &str) {
        if !matches!(self.app.phase(), Phase::Collecting) {
            return;
        }
        if let Some(clean) = script::sanitize_danmaku(content)
            && self.chatter_buf.len() < 40
        {
            self.chatter_buf.push(clean);
        }
    }

    /// 每 250ms 走一次。
    ///
    /// **刻意不是 `async`。** 这里一个 `.await` 都没有——所有网络和合成都在
    /// `tokio::spawn` 出去的任务里。保持同步，调用方才敢用普通 `Mutex` 把它
    /// 和收弹幕的任务共起来，而不必担心持锁跨 await。
    pub fn tick(&mut self) {
        self.drain_reports();
        self.drain_ready();
        self.drain_questions();
        self.expire_playing();

        // 到点自动封盘。原有逻辑，保留。
        self.app.tick();

        // 阶段在 `app.tick()` 里可能刚变过，所以要在这之后读。
        match self.app.phase() {
            Phase::Presenting => self.step_presenting(),
            Phase::Collecting => self.step_collecting(),
            Phase::Locked => self.step_locked(),
            Phase::Revealed => self.step_revealed(),
        }

        self.pump();
    }

    /// 收页面回报的「念完了」。
    fn drain_reports(&mut self) {
        while let Ok(id) = self.spoken.try_recv() {
            let Some(playing) = &self.playing else { continue };
            if playing.id != id {
                // 过期的回报（上一句的）——丢掉，别拿它推进当前这句。
                continue;
            }
            let kind = playing.kind;
            self.playing = None;
            self.app.set_speech(None);
            self.finish(kind);
        }
    }

    /// 收后台合成好的语音。
    fn drain_ready(&mut self) {
        // 先记下当前题号，避免在循环里反复取锁。
        let current = self.app.question().id;
        while let Ok(ready) = self.ready_rx.try_recv() {
            // 换题之后才回来的产物一律作废。
            if ready.for_question != current {
                // 带上两个题号：光说「丢了一条」看不出是慢了一拍还是慢了整整一轮。
                tracing::debug!(
                    kind = ready.kind,
                    ready_for = ready.for_question,
                    current,
                    "丢掉过期语音（题已经换了）"
                );
                continue;
            }
            if ready.failed {
                // 这句没合成出来。当作「已经念过」放行，别让流程等一个
                // 永远不会到的回报。
                tracing::warn!(kind = ready.kind, "这句没合成出来，跳过");
                self.finish(ready.kind);
                continue;
            }
            if ready.kind == "explain" {
                // 收答期预生成的讲解**片段**。**压在手里，不排进队列**——
                // 它含正确答案，公布之前不能播。
                //
                // 注意它和 `"reveal"` 是两回事：公布的整句是
                // 「现场拼的事实 + 这段讲解」，由 `step_locked` 合成后再送回来，
                // 那条是 `"reveal"`，要正常排队。两者混用同一个 kind 的话，
                // 公布的语音会被这里当成片段又扣下来，永远播不出去。
                self.pending_reveal = Some(ready);
            } else {
                self.queue.push_back(ready);
            }
        }
    }

    /// 收后台出好的题。
    fn drain_questions(&mut self) {
        while let Ok(made) = self.question_rx.try_recv() {
            self.question_pending = false;
            let Some(question) = made else { continue };
            // 注意：**这里不记 `asked`**。记在上屏那一刻（`step_presenting`），
            // 出好的题可能一直排在题库里没轮到，过早记进去会让模型误以为
            // 已经出过——那等于凭白缩小了可用的题面。
            self.app.push_question(question);
        }
    }

    /// 记下「这道题上屏了」，供出题时避开。
    fn remember_asked(&mut self, question: &Question) {
        // 题库是循环的：跑久了同一道题会第二次上屏，别记两遍。
        if self.asked.iter().any(|p| p == &question.prompt) {
            return;
        }
        // 只留最近这些。模型那边也只取前 20 条。
        if self.asked.len() >= AVOID_KEEP {
            self.asked.remove(0);
        }
        self.asked.push(question.prompt.clone());
    }

    /// 等太久了就兜底往下走。
    fn expire_playing(&mut self) {
        let Some(playing) = &self.playing else { return };
        if Instant::now() < playing.give_up_at {
            return;
        }
        tracing::warn!(
            kind = playing.kind,
            "等页面回报念完超时，兜底继续——画面可能断了，直播不能停"
        );
        let kind = playing.kind;
        self.playing = None;
        self.app.set_speech(None);
        self.finish(kind);
    }

    /// 一条语音播完了（真回报或兜底超时都走这里）。
    fn finish(&mut self, kind: &'static str) {
        match kind {
            "intro" => self.intro_played = true,
            "reveal" => self.spoke_reveal = true,
            "narration" => self.narration_done = true,
            _ => {}
        }
    }

    /// 把队列里的下一条推给画面。
    fn pump(&mut self) {
        if self.playing.is_some() {
            return;
        }
        let Some(ready) = self.queue.pop_front() else {
            return;
        };

        let duration = ready.duration();
        let id = self.app.publish_audio(ready.audio);
        self.app.set_speech(Some(SpeechView {
            id: id.to_string(),
            kind: ready.kind.to_owned(),
            url: format!("/voice/{id}"),
            text: ready.text,
            duration_ms: duration.as_millis() as u64,
        }));
        self.playing = Some(Playing {
            id,
            kind: ready.kind,
            give_up_at: Instant::now() + duration + SPEECH_SLACK,
        });
    }

    // ── 各阶段 ────────────────────────────────────────────────

    /// 报题中：排报题语音，同时**提前生成讲解**，等语音念完就开表。
    fn step_presenting(&mut self) {
        let question = self.app.question();

        if self.presenting_since.map(|(id, _)| id) != Some(question.id) {
            // 刚进入这道题的报题阶段。
            self.presenting_since = Some((question.id, Instant::now()));
            self.intro_played = false;
            self.chatter_buf.clear();
            self.next_chatter = Instant::now() + CHATTER_INTERVAL;
            // 上一题可能留下了没播完的（比如串场生成太慢被跳过了）。
            self.queue.clear();
            self.playing = None;
            self.app.set_speech(None);

            // 记在**上屏这一刻**，而不是「AI 出的那一刻」。
            //
            // 原来只在 `drain_questions` 里记，于是 `asked` 里永远只有 AI
            // 出的题——而第一次出题恰恰发生在题库见底的时候，那时 `asked`
            // 是空的，模型拿不到任何「别重复」的线索。演示题库那三十道
            // 观众刚看了半小时，模型完全可能原样再出一道。
            // 上屏是唯一一个「这题真的被看过了」的时刻，记在这里最准。
            self.remember_asked(&question);

            self.spawn_intro(&question);
            self.spawn_explanation(&question);
            self.maybe_generate_question();
            return;
        }

        // 语音念完了，或者压根没合成出来 → 开表。
        let (_, since) = self.presenting_since.expect("上面刚确认过是 Some");
        let done = self.intro_played && self.playing.is_none() && self.queue.is_empty();
        // **超时只管「还没开播」，不管「还在播」。** 报题这句本身就有七八秒，
        // 合成又可能抖动到十几秒（实测有 2.5s 也有 13s）。用「距离进入报题
        // 阶段过了多久」去卡它，就会在旁白念到一半时开始计时——而这一阶段
        // 存在的全部意义，就是别让手快的人赢过在听讲解的人。
        // 一旦开播，交给「播完」的判定，那个的时间尺度是跟着音频长度走的。
        let idle = !self.intro_played && self.playing.is_none() && self.queue.is_empty();
        let timed_out = idle && since.elapsed() > PRESENT_TIMEOUT;

        if done || timed_out {
            if timed_out {
                tracing::warn!("报题语音一直没合成出来，直接开表");
            }
            self.app.begin();
            self.presenting_since = None;
            self.intro_played = false;
        }
    }

    /// 收答中：偶尔说句话控场。封盘由 `app.tick()` 负责。
    fn step_collecting(&mut self) {
        if self.playing.is_some() || !self.queue.is_empty() {
            return;
        }
        if self.ai.is_none() {
            // 没 key 就别每 25 秒空转一次。
            return;
        }
        if Instant::now() < self.next_chatter || self.chatter_pending {
            return;
        }
        self.next_chatter = Instant::now() + CHATTER_INTERVAL;
        self.spawn_chatter();
    }

    /// 封盘：立刻公布，把事实和讲解拼成一句排上。
    ///
    /// 事实部分（人数、票数分布）只能在封盘这一刻拼——它之前根本不存在。
    /// 所以这一段是现场合成，讲解那部分则早在收答期就合成好了。
    /// 公布音效正好盖住这一两秒的合成时间。
    fn step_locked(&mut self) {
        self.app.step(); // Locked → Revealed

        let Some(stats) = self.app.stats() else {
            tracing::error!("公布之后拿不到统计，跳过讲解");
            return;
        };
        let question = self.app.question();

        let facts = RevealFacts {
            prompt: &question.prompt,
            // 题干的中文意思，就是正确选项那一个——`GeneratedQuestion::validate`
            // 专门校验过这两者必须对得上，演示题库的选项本身也就是中文释义。
            //
            // **这里原来写的是空串**，注释还写着「`segments()` 也不用它」——
            // 可 `segments()` 明明用了。于是每一题公布的时候，主持人都要
            // 念一遍「意思是「」。」，引号里空的。线上真跑才看得见。
            meaning_zh: question.option(question.correct),
            correct: question.correct.letter(),
            correct_text: question.option(question.correct),
            tally: stats.tally,
            correct_count: stats.correct_count,
            total: stats.participants,
        };
        let segments = facts.segments();

        // 讲解已经在收答期备好了；万一没好（API 慢或挂了），事实部分自己也能站住。
        let explanation = self.pending_reveal.take();
        let tts = Arc::clone(&self.tts);
        let voices = self.voices.clone();
        let tx = self.ready_tx.clone();
        let qid = question.id;
        let facts_text: String = segments.iter().map(|s| s.text.as_str()).collect();

        tokio::spawn(async move {
            let audio = match tts.say_all(&segments, &voices).await {
                Ok(audio) => audio,
                Err(err) => {
                    tracing::error!("公布语音合成失败：{err:#}");
                    return;
                }
            };

            let (audio, text) = match explanation {
                Some(expl) => {
                    // 事实 + 讲解拼成**一条** MP3，而不是排两条。
                    // 分开播的话两句之间会有一道缝，中韩切换的地方尤其明显。
                    let mut joined = audio;
                    joined.extend_from_slice(&expl.audio);
                    (joined, format!("{facts_text}{}", expl.text))
                }
                None => (audio, facts_text),
            };

            let _ = tx.send(Ready::new("reveal", audio, text, qid));
        });

        self.curtain = Curtain::Explaining;
        self.curtain_for = Some(qid);
        self.spoke_reveal = false;
        self.curtain_until = Instant::now() + REVEAL_SYNTH_TIMEOUT;
    }

    /// 已公布：讲解念完 → 串场念完 → 换下一题。
    fn step_revealed(&mut self) {
        let qid = self.app.question().id;
        if self.curtain_for != Some(qid) {
            // 走到这儿说明是人工在控制台插了一手，重新对表。
            self.curtain = Curtain::Explaining;
            self.curtain_for = Some(qid);
        }

        match self.curtain {
            Curtain::Explaining => {
                // 播完了。
                let spoken = self.spoke_reveal && self.playing.is_none() && self.queue.is_empty();
                // 还没轮到它——合成没回来，队列空着也没在播。
                //
                // **超时只管合成，不能管播放。** 这条语音本身就有十几秒长，
                // 用「距离开播过了多久」去卡它，会在它念到一半时把串场插进来。
                // 一旦开播，就交给「播完」的判定（页面回报，或时长兜底），
                // 那个的时间尺度是跟着音频长度走的。
                let idle = !self.spoke_reveal && self.playing.is_none() && self.queue.is_empty();
                let gave_up = idle && Instant::now() >= self.curtain_until;
                if spoken || gave_up {
                    if gave_up {
                        tracing::warn!("公布语音一直没合成出来，直接接串场");
                    }
                    self.narration_done = false;
                    self.interlude_until = Instant::now() + INTERLUDE_TIMEOUT;
                    self.spawn_narration();
                    self.curtain = Curtain::Interlude;
                }
            }
            Curtain::Interlude => {
                // 串场念完了。
                let quiet = self.narration_done && self.playing.is_none() && self.queue.is_empty();
                // 没生成出来（超时兜底）。同样只管合成，不管播放。
                let idle = !self.narration_done && self.playing.is_none() && self.queue.is_empty();
                if quiet || (idle && Instant::now() >= self.interlude_until) {
                    self.app.prepare_next();
                    self.curtain = Curtain::Explaining;
                    self.curtain_for = None;
                    self.reveal_for = None;
                    self.pending_reveal = None;
                    self.narration_done = false;
                }
            }
        }
    }

    // ── 后台任务 ──────────────────────────────────────────────

    /// 排报题语音。纯本地拼句，不经过模型——题目信息本来就全都有。
    fn spawn_intro(&self, question: &Question) {
        // 分段数直接等于握手次数，所以中文字尽量并成一段。
        let segments = vec![
            Segment::zh(format!("第 {} 题，请听题——", self.app.question_index() + 1)),
            Segment::ko(question.prompt.clone()),
            Segment::zh("。四个选项在屏幕上，打字母作答。"),
        ];
        self.synthesize("intro", segments, question.id);
    }

    /// 提前生成讲解稿。**这是收答期那几十秒白送的算力。**
    ///
    /// 生成好之后压在 `pending_reveal` 里，公布之前绝不进画面。
    fn spawn_explanation(&mut self, question: &Question) {
        if self.reveal_for == Some(question.id) {
            return;
        }
        self.reveal_for = Some(question.id);

        let Some(ai) = self.ai.as_ref() else {
            // 没 key：讲解就只有事实部分，由 `step_locked` 现场拼。
            return;
        };

        let user = format!(
            "请为这道题写一句面向初学者的讲解（中文，40 字以内，说清为什么）。\
             只输出这句话本身，不要任何前缀。\n题干：{}\n正确选项：{}",
            question.prompt,
            question.option(question.correct)
        );

        let ai = Arc::clone(ai);
        let tts = Arc::clone(&self.tts);
        let voices = self.voices.clone();
        let tx = self.ready_tx.clone();
        let qid = question.id;

        tokio::spawn(async move {
            // 调用放在任务**里面**，不是外面先建好 future 再 move 进来——
            // `chat` 借的是 `&self` 和 `&str`，在外面建出来的 future 带着
            // 对栈上数据的借用，根本 move 不进 'static 的任务。
            // **失败原因必须打出来。** 原来这里是 `let Ok(text) = .. else { warn!(...) }`，
            // 错误被就地丢掉，日志里只剩下「讲解生成失败」五个字。线上真出现过
            // 连着十几分钟每题都失败，而日志完全看不出是 401、429、超时还是网络不通——
            // 排查时只能靠猜。丢掉错误的那一行，就是丢掉唯一的线索。
            let text = match ai.chat(script::HOST_PERSONA, &user).await {
                Ok(text) => text,
                Err(err) => {
                    // `err` 里不含密钥：`chat_inner` 只带状态码和 200 字以内的响应片段，
                    // reqwest 也不会把 Authorization 头写进错误里。
                    tracing::warn!(error = %format!("{err:#}"), "讲解生成失败，公布时只用本地事实部分");
                    return;
                }
            };
            let text = text.trim().to_owned();
            if text.is_empty() {
                return;
            }
            match tts.say_all(&[Segment::zh(text.clone())], &voices).await {
                // 攒着不给画面。`step_locked` 会在公布那一刻把它接上。
                Ok(audio) => {
                    tracing::debug!(qid, bytes = audio.len(), "讲解已合成，送进管道等公布");
                    let _ = tx.send(Ready::new("explain", audio, text, qid));
                }
                // 讲解没合成出来不是致命的——公布时还有本地的事实部分。
                Err(err) => tracing::warn!("讲解语音合成失败：{err:#}"),
            }
        });
    }

    /// 收答期的控场旁白。
    ///
    /// 递过去的是**摘要**，不是弹幕原文——「多少人、多少条、有几条在聊韩语、
    /// 几条消毒过的样本」。要的是判断，不是逐条回应。
    fn spawn_chatter(&mut self) {
        let Some(ai) = self.ai.as_ref() else { return };
        self.chatter_pending = true;

        let digest = RoomDigest {
            index: self.app.question_index() + 1,
            participants: self.app.participants(),
            seconds_left: self.app.remaining().as_secs(),
            danmaku_count: self.chatter_buf.len(),
            learning_count: self
                .chatter_buf
                .iter()
                .filter(|s| script::looks_like_learning(s))
                .count(),
            // 样本已经消毒过，见 `observe_danmaku`。
            samples: self.chatter_buf.iter().rev().take(5).cloned().collect(),
        };
        self.chatter_buf.clear();

        let ai = Arc::clone(ai);
        let user = digest.prompt();
        let tts = Arc::clone(&self.tts);
        let voices = self.voices.clone();
        let tx = self.ready_tx.clone();
        let qid = self.app.question().id;

        tokio::spawn(async move {
            // 和讲解那边一样，**别把错误丢掉**。这里原来是 `let Ok(text) = .. else { return }`，
            // 连一行日志都没有——于是「AI 一整场没说过一句话」在日志里是完全隐形的。
            // 排查那次故障时，这条线索本该最早暴露问题。
            let text = match ai.chat(script::CHATTER_SYSTEM, &user).await {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!(error = %format!("{err:#}"), "互动台词没生成出来，这轮不说话了");
                    return;
                }
            };
            let text = text.trim().to_owned();
            if text.is_empty() {
                return;
            }
            if let Ok(audio) = tts.say_all(&[Segment::zh(text.clone())], &voices).await {
                let _ = tx.send(Ready::new("chatter", audio, text, qid));
            }
        });
    }

    /// 两题之间的串场。
    fn spawn_narration(&mut self) {
        let Some(ai) = self.ai.as_ref() else {
            // 没有 AI 就没有串场，但不能让公布画面为此多停十秒。
            self.narration_done = true;
            return;
        };
        let ai = Arc::clone(ai);
        let tts = Arc::clone(&self.tts);
        let voices = self.voices.clone();
        let tx = self.ready_tx.clone();
        let qid = self.app.question().id;

        tokio::spawn(async move {
            let Ok(text) = ai.chat(script::NARRATION_SYSTEM, "继续。").await else {
                return;
            };
            let text = text.trim().to_owned();
            if text.is_empty() {
                return;
            }
            if let Ok(audio) = tts.say_all(&[Segment::zh(text.clone())], &voices).await {
                let _ = tx.send(Ready::new("narration", audio, text, qid));
            }
        });
    }

    /// 题库快见底了，提前让模型出一道塞进去。
    ///
    /// 提前做，是因为出题要好几秒——等到没题可出的时候再要，
    /// 直播就会出现一段空白。
    fn maybe_generate_question(&mut self) {
        if self.question_pending || !self.app.bank_running_low(BANK_LOW_WATER) {
            return;
        }
        let Some(ai) = self.ai.as_ref() else { return };
        self.question_pending = true;

        let ai = Arc::clone(ai);
        let user = script::question_prompt(&self.asked, self.app.bank_len());
        let tx = self.question_tx.clone();

        tokio::spawn(async move {
            // 任何一步失败都要能把「正在出题」的标记解开，好让下次还能再试。
            // 丢题不致命：题库里的旧题会循环。
            //
            // 但**三步失败要分开报**。「调用挂了」「回来了但不是 JSON」
            // 「是 JSON 但没过校验」是三个完全不同的问题：分别是网络/密钥、
            // 提示词、模型能力。原来这三条被 `.ok()?` 拧成一句
            // 「这次没出成题」，等于把线索又丢了一次。
            let made = match ai.chat_json(script::QUESTION_SYSTEM, &user).await {
                Err(err) => {
                    tracing::warn!(error = %format!("{err:#}"), "出题调用失败，留待下次");
                    None
                }
                Ok(raw) => match serde_json::from_str::<GeneratedQuestion>(&raw) {
                    Err(err) => {
                        tracing::warn!(error = %err, raw = %raw.chars().take(200).collect::<String>(),
                                       "出题的返回不是能解的 JSON，留待下次");
                        None
                    }
                    // 选项在这里被打乱（见 `into_question`）——否则模型偏爱把
                    // 正确答案放在 A，观众摸出规律就会跟风。
                    Ok(parsed) => match parsed.into_question(0, seed()) {
                        Ok(q) => Some(q),
                        Err(err) => {
                            tracing::warn!(error = %format!("{err:#}"), "出的题没过校验，留待下次");
                            None
                        }
                    },
                },
            };
            let _ = tx.send(made);
        });
    }

    /// 本地合成一段话并排进队列。
    fn synthesize(&self, kind: &'static str, segments: Vec<Segment>, qid: u64) {
        let tts = Arc::clone(&self.tts);
        let voices = self.voices.clone();
        let tx = self.ready_tx.clone();
        let text: String = segments.iter().map(|s| s.text.as_str()).collect();
        tokio::spawn(async move {
            match tts.say_all(&segments, &voices).await {
                Ok(audio) => {
                    let _ = tx.send(Ready::new(kind, audio, text, qid));
                }
                Err(err) => {
                    tracing::error!("语音合成失败（{kind}）：{err:#}");
                    // 必须报回去。否则「等报题语音念完」会一直等，
                    // TTS 一挂每道题就多卡 25 秒。
                    let _ = tx.send(Ready::failed(kind, qid));
                }
            }
        });
    }
}

/// 打乱选项用的种子。
///
/// 用时间而不是固定值：同一道题重播时也该换个位置，
/// 否则「答案总在第三个」又会变成新的跟风信号。
fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use livestream_core::Choice;
    use livestream_web::Session;

    /// 不出声的合成器。给一段固定的短音频，估算时长十几毫秒——
    /// 测试不想为「等它播完」真坐几秒。
    struct FakeTts;

    #[async_trait::async_trait]
    impl Tts for FakeTts {
        async fn say(&self, _text: &str, _voice: &str) -> anyhow::Result<Vec<u8>> {
            Ok(vec![0u8; 60])
        }
    }

    fn bank(n: u64) -> Vec<Question> {
        (1..=n)
            .map(|i| Question {
                id: i,
                prompt: format!("문제{i}"),
                options: ["가".into(), "나".into(), "다".into(), "라".into()],
                correct: Choice::B,
            })
            .collect()
    }

    /// 一个跑得起来的导演：有假合成器、没有 AI（走本地台词这条降级路径）。
    fn fixture(questions: u64) -> (Director, App) {
        let session =
            Session::new(bank(questions), Duration::from_millis(120), Instant::now()).unwrap();
        let (app, spoken) = App::new(session);
        let mut director = Director::new(
            app.clone(),
            None,
            Arc::new(FakeTts),
            Voices::default(),
            spoken,
        );
        director.start();
        (director, app)
    }

    #[test]
    fn the_explanation_is_held_back_until_reveal() {
        // 这是防泄题的核心不变量：收答期预生成的讲解含正确答案，
        // 只能压在 `pending_reveal` 里，绝不能排进播放队列。
        let (mut d, _app) = fixture(2);
        let qid = d.app.question().id;
        d.ready_tx
            .send(Ready::new("explain", vec![0; 32], "正确答案是B，因为…".into(), qid))
            .unwrap();

        d.drain_ready();

        assert!(d.pending_reveal.is_some(), "讲解该被压在手里");
        assert!(d.queue.is_empty(), "公布之前，讲解绝不能进播放队列");
    }

    #[test]
    fn an_explanation_that_arrives_after_the_question_changed_is_dropped() {
        // 后台任务可能在换题之后才返回。用错题的讲解配这一题的答案，
        // 观众一眼就看出来了。
        let (mut d, _app) = fixture(2);
        let stale = d.app.question().id + 99;
        d.ready_tx
            .send(Ready::new("explain", vec![0; 32], "上一题的讲解".into(), stale))
            .unwrap();

        d.drain_ready();

        assert!(d.pending_reveal.is_none(), "换了题才回来的讲解必须丢掉");
        assert!(d.queue.is_empty());
    }

    #[test]
    fn the_reveal_line_is_queued_not_held() {
        // 回归测试。公布的整句和预生成的片段**不能共用 kind**：
        // 都用 "reveal" 的话，`drain_ready` 会把公布的整句也当成片段扣下来，
        // 于是公布时一声不吭——而且不报错，最难查的那种。
        let (mut d, _app) = fixture(2);
        let qid = d.app.question().id;
        d.ready_tx
            .send(Ready::new("reveal", vec![0; 32], "正确答案是B。".into(), qid))
            .unwrap();

        d.drain_ready();

        assert_eq!(d.queue.len(), 1, "公布的整句要排队去播，不能被扣下");
        assert!(d.pending_reveal.is_none());
    }

    #[test]
    fn a_failed_synthesis_does_not_stall_the_show() {
        // TTS 挂了没人报信的话，「等报题语音念完」会一直等，
        // 每道题白等满 25 秒。失败也必须报回来。
        let (mut d, _app) = fixture(2);
        let qid = d.app.question().id;
        d.ready_tx.send(Ready::failed("intro", qid)).unwrap();

        d.drain_ready();

        assert!(d.intro_played, "合成挂了就该当作念过，不能一直等下去");
    }

    #[test]
    fn danmaku_is_only_collected_while_answers_are_open() {
        let (mut d, app) = fixture(2);

        // 报题阶段还在念题，这时候的弹幕不该被攒起来。
        assert_eq!(app.phase(), Phase::Presenting);
        d.observe_danmaku("我在学韩语");
        assert!(d.chatter_buf.is_empty(), "没开表就别攒弹幕");

        // 开表之后才开始攒。
        app.begin();
        d.observe_danmaku("我在学韩语");
        assert_eq!(d.chatter_buf.len(), 1);

        // 而含答案痕迹的根本进不来——这是第一道防线，不指望模型自己拒绝。
        d.observe_danmaku("我觉得答案是B");
        d.observe_danmaku("选B");
        assert_eq!(d.chatter_buf.len(), 1, "带答案痕迹的弹幕必须在进门时就被丢掉");
    }

    /// 把导演摆成「报题阶段已经超时」的样子。`playing` 传 `Some` 表示
    /// 语音正在播，`None` 表示还没播上。
    fn aged_presenting(d: &mut Director, app: &App, playing: bool) {
        let qid = app.question().id;
        d.presenting_since = Some((
            qid,
            Instant::now()
                .checked_sub(PRESENT_TIMEOUT + Duration::from_secs(30))
                .expect("测试机的时间没往回跳过"),
        ));
        d.intro_played = false;
        d.playing = playing.then(|| Playing {
            id: AudioId::new("v1"),
            kind: "intro",
            // 页面回报的兜底时限还没到，所以它不会被 `expire_playing` 清掉。
            give_up_at: Instant::now() + Duration::from_secs(60),
        });
    }

    #[test]
    fn a_playing_intro_is_never_cut_short_by_the_synthesis_deadline() {
        // **回归测试。** 这个超时是给「合成还没回来」兜底的，不是给
        // 「正在播」兜底的。报题这句本身就有七八秒，合成还实测抖动过
        // 2.5s～13s——拿「进入报题阶段多久了」去卡它，就会在旁白念到
        // 一半时开表。而 Presenting 这一阶段存在的全部意义，就是别让
        // 手快的人赢过在听讲解的人；在念到一半时开表，正好把它毁掉。
        let (mut d, app) = fixture(2);
        aged_presenting(&mut d, &app, true);

        d.tick();

        assert_eq!(
            app.phase(),
            Phase::Presenting,
            "语音还在念，计时就不能开始——哪怕早就过了合成时限"
        );
    }

    #[test]
    fn a_synthesis_that_never_arrives_still_starts_the_clock() {
        // 另一头也得钉住：兜底不能因为上面那层守卫而变成「永远不触发」。
        // 合成彻底挂了（没人报信、队列也空），到点就得开表，
        // 否则整场直播停在报题阶段，再也走不下去。
        let (mut d, app) = fixture(2);
        aged_presenting(&mut d, &app, false);

        d.tick();

        assert_eq!(
            app.phase(),
            Phase::Collecting,
            "压根没合成出来，到点必须开表，不能干等"
        );
    }

    #[tokio::test]
    async fn the_explanation_reaches_the_reveal() {
        // **排查用。** 线上出现过讲解明明生成好了，公布时却只有本地事实部分——
        // 而日志里既没有「生成失败」也没有「合成失败」，等于无声无息地丢了。
        // 这条盯住整条接线：后台送进来的 explain → 压住 → 公布时接上。
        let (mut d, app) = fixture(2);
        let mut sent = false;
        let mut reveal_text = None;

        for _ in 0..800 {
            d.tick();

            // 一进报题阶段，就假装「后台已经把讲解合成好了」送进来。
            // （fixture 里没有 AI，真正那条 chat 调用在这里跑不了。）
            if !sent && d.presenting_since.is_some() {
                let qid = app.question().id;
                d.ready_tx
                    .send(Ready::new("explain", vec![0u8; 60], "这是讲解正文".into(), qid))
                    .unwrap();
                sent = true;
            }

            if let Some(speech) = app.speech() {
                if speech.kind == "reveal" {
                    reveal_text = Some(speech.text.clone());
                }
                app.report_spoken(AudioId::new(speech.id));
            }
            if reveal_text.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let text = reveal_text.expect("公布语音压根没推给画面");
        assert!(
            text.contains("这是讲解正文"),
            "讲解备好了却没接进公布语音：{text}"
        );
    }

    /// 真打 DeepSeek，验「题库见底 → 自动出题 → 补回题库」这条闭环。
    ///
    /// ```bash
    /// cargo test -p livestream-app -- --ignored --nocapture refills_itself
    /// ```
    ///
    /// **这条单独拎出来测是有原因的。** 演示题库有三十来道，`BANK_LOW_WATER`
    /// 是 2，所以线上跑要三十多分钟才会碰到一次；光靠「盯着直播间看」是验不到
    /// 的，而它偏偏是「AI 出题」这个卖点的全部实现。这里只放一道题，
    /// 让它在第一秒就触发，走的是和线上**同一份**代码。
    #[tokio::test]
    #[ignore = "要联网 + 花钱，手动跑"]
    async fn the_bank_refills_itself_when_it_runs_low() {
        let _ = dotenvy::dotenv();
        let Some(ai) = livestream_ai::DeepSeek::from_env().expect("建客户端失败") else {
            eprintln!("跳过：没有 DEEPSEEK_API_KEY（.env 里没写或为空）");
            return;
        };

        let session = Session::new(bank(1), Duration::from_millis(120), Instant::now()).unwrap();
        let (app, spoken) = App::new(session);
        let mut d = Director::new(
            app.clone(),
            Some(Arc::new(ai)),
            Arc::new(FakeTts),
            Voices::default(),
            spoken,
        );
        d.start();

        assert!(
            app.bank_running_low(BANK_LOW_WATER),
            "前提：只有一道题，开局就该是见底状态"
        );
        assert_eq!(app.bank_len(), 1);

        let mut tried = 0u32;
        for _ in 0..600 {
            d.tick();
            tried += 1;
            if app.bank_len() > 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        println!("第 {tried} 次 tick 之后，题库 {} 道", app.bank_len());
        assert!(
            app.bank_len() > 1,
            "题库见底了却没补上——AI 出题这条路是断的（线上要跑半小时才看得出来）"
        );
    }

    #[tokio::test]
    async fn every_question_that_goes_on_screen_is_remembered() {
        // **回归测试。** `asked` 原来只在收到 AI 出的题时记一笔，题库里的题
        // 一道都不记。而第一次出题恰恰发生在题库见底那一刻——那时 `asked`
        // 是空的，提示词里连一条「别重复」都没有，模型完全可能原样再出
        // 一道观众刚看了半小时的题。
        let (mut d, app) = fixture(3);
        let mut shown = std::collections::HashSet::new();

        for _ in 0..600 {
            d.tick();
            if let Some(speech) = app.speech() {
                app.report_spoken(AudioId::new(speech.id));
            }
            // 数「报题阶段真的开始了」的题，不是「`app.question()` 换成了谁」。
            // `prepare_next` 一跑，`app.question()` 立刻就是下一题了，
            // 但那一题的报题阶段要下一次 tick 才进——拿前者当进度会数早了，
            // 测出来的就是「记漏了」，而其实是还没轮到记。
            if let Some((id, _)) = d.presenting_since {
                shown.insert(id);
            }
            if shown.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(shown.len(), 3, "前提：三道题的报题阶段都走过");
        assert_eq!(d.asked.len(), 3, "上过屏的题都该记住，实际是 {:?}", d.asked);
        // 记的得是题干（提示词里给模型看的就是它），不是 id。
        assert!(
            d.asked.iter().all(|p| p.starts_with("문제")),
            "记错东西了：{:?}",
            d.asked
        );
    }

    #[tokio::test]
    async fn the_show_advances_on_its_own() {
        // 整条链路：报题 → 开表 → 封盘 → 公布 → 换题，全程没有人按按钮。
        // 这同时盯着「自动推进不卡死」和「不持锁 await」——真卡住的话
        // 这个循环走不完，断言会直接失败，而不是挂在那儿。
        let (mut d, app) = fixture(4);
        let mut seen = std::collections::HashSet::new();

        for _ in 0..600 {
            d.tick();
            // 扮演页面：凡是看到新的一句，立刻回报「念完了」。
            if let Some(speech) = app.speech() {
                app.report_spoken(AudioId::new(speech.id));
            }
            seen.insert(app.question().id);
            if seen.len() >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            seen.len(),
            4,
            "没人管也得自己把题换下去，实际只走到 {seen:?}"
        );
    }
}
