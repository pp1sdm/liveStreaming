//! 一道题的轮次状态机。
//!
//! ```text
//!   open()                deadline 到 / lock()          reveal()
//!   ───────► Collecting ──────────────────────► Locked ────────► Revealed
//!                │                                              ▲
//!                └──────────────────────────────────────────────┘
//!                        直接公布（超时自动锁定的常见路径）
//! ```
//!
//! 时间从外面传进来（`now: Instant`），这个类型自己不读时钟，所以测试里
//! 可以把时间捏在手上。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use livestream_core::{Answer, Choice, Danmaku, Question};
use serde::Serialize;

use crate::judge::{Rejection, Verdict, judge};

/// 「疑似」日志最多保留多少条。热闹的直播间一秒钟能有几十条噪声，
/// 不设上限会一路涨上去。
const MAX_REJECTIONS: usize = 200;

/// 公布时最多列出多少位答对者。
const MAX_WINNERS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 题已经亮出来了，但**还没开始计时**——主持人在念题干。
    Presenting,
    /// 正在收答。
    Collecting,
    /// 已封盘，等主播公布。
    Locked,
    /// 已公布。
    Revealed,
}

/// 一条弹幕进到轮次里之后的去向。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 不在收答窗口内——封盘了，或者已经公布了。弹幕源照推不误，这里不看。
    Ignored,
    /// 计为一次作答。
    Counted(Choice),
    /// 这个人已经答过了。同人只取第一次，改口不算。
    Duplicate,
    /// 不是有效作答，已进「疑似」日志。
    Rejected(Rejection),
}

/// 公布时给画面的统计。
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub correct: Choice,
    /// 四个选项各自的票数。
    pub tally: [u32; 4],
    /// 参与人数 = 有效作答去重后的人数。
    pub participants: u32,
    pub correct_count: u32,
    /// 正确率，0.0–1.0。没人作答时为 0。
    pub accuracy: f32,
    /// 按到达顺序的答对者。
    pub winners: Vec<Answer>,
}

/// 一道题的完整生命周期。
#[derive(Debug, Clone)]
pub struct Round {
    pub question: Question,
    pub phase: Phase,
    pub opened_at: Instant,
    pub deadline: Instant,
    /// 收答窗口总长。存下来是为了 [`Round::remaining`] 在还没开始计时时
    /// 能报出满格。
    pub window: Duration,
    pub tally: [u32; 4],
    /// 按到达顺序排列的有效作答，用于排名次。
    pub answers: Vec<Answer>,
    /// 被丢掉的弹幕：`(昵称, 原文, 原因)`。控制台用它调优判定。
    rejections: Vec<(String, String, Rejection)>,
    /// 已经答过的人。同人只取第一次。
    seen: HashSet<String>,
}

impl Round {
    /// 开一道题，立刻开始收答。
    pub fn open(question: Question, window: Duration, now: Instant) -> Self {
        let mut round = Self::present(question, window, now);
        round.start_collecting(now);
        round
    }

    /// 开一道题，但**先不计时**。
    ///
    /// 这是 AI 主持的默认开法。它要先把题干念一遍（七八秒），这段时间
    /// 计时器不该跑，原因有两个，都不是小事：
    ///
    /// 1. **公平。** 念题干的同时计时，手速加分就发给了"没在听讲解、
    ///    自己扫一眼就作答"的人。这个玩法奖励的是懂韩语，不是跳读。
    /// 2. **穿帮。** 画面上会出现"AI 还在念题，已经有三个人答完了"。
    ///    一场号称全 AI 主持的直播，这种场面一眼就假。
    pub fn present(question: Question, window: Duration, now: Instant) -> Self {
        Self {
            question,
            phase: Phase::Presenting,
            opened_at: now,
            // 占位。真的截止时刻在 `start_collecting` 里才定。
            deadline: now,
            window,
            tally: [0; 4],
            answers: Vec::new(),
            rejections: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// 旁白念完了，开始计时收答。
    ///
    /// 只有 `Presenting` 能转过来。重复调用是安全的空操作——编排层可能
    /// 因为语音回报、超时兜底两条路都走到这里。
    pub fn start_collecting(&mut self, now: Instant) {
        if self.phase == Phase::Presenting {
            self.phase = Phase::Collecting;
            self.opened_at = now;
            self.deadline = now + self.window;
        }
    }

    /// 收答窗口是否还在。
    pub fn is_collecting(&self) -> bool {
        self.phase == Phase::Collecting
    }

    /// 题亮出来了但还没开始计时。
    pub fn is_presenting(&self) -> bool {
        self.phase == Phase::Presenting
    }

    /// 倒计时还剩多久。已过期为 0。
    pub fn remaining(&self, now: Instant) -> Duration {
        // 还没开始计时，倒计时环显示满格——不是 0，那会看着像已经结束了。
        if self.phase == Phase::Presenting {
            return self.window;
        }
        self.deadline.saturating_duration_since(now)
    }

    /// 收答时间是否已到。
    ///
    /// 还没开始计时时**永远是 false**。`deadline` 在 `Presenting` 期间是个
    /// 占位值（等于开题时刻），照直比会立刻返回 true——任何一处
    /// "先看 expired 再看 phase"的代码都会当场把题封掉。
    /// 让这个函数自己说真话，比要求每个调用点都记得先判 phase 可靠。
    pub fn expired(&self, now: Instant) -> bool {
        if self.phase == Phase::Presenting {
            return false;
        }
        now >= self.deadline
    }

    /// 收下一条弹幕。
    pub fn submit(&mut self, d: &Danmaku, now: Instant) -> Outcome {
        if !self.is_collecting() || self.expired(now) {
            return Outcome::Ignored;
        }

        match judge(&d.content) {
            Verdict::Chosen(choice) => {
                if !self.seen.insert(d.user_id.clone()) {
                    return Outcome::Duplicate;
                }
                self.tally[choice.index()] += 1;
                self.answers.push(Answer {
                    user_id: d.user_id.clone(),
                    nickname: d.nickname.clone(),
                    choice,
                    seq: d.seq,
                });
                Outcome::Counted(choice)
            }
            Verdict::Rejected(reason) => {
                if self.rejections.len() < MAX_REJECTIONS {
                    self.rejections
                        .push((d.nickname.clone(), d.content.clone(), reason.clone()));
                }
                Outcome::Rejected(reason)
            }
        }
    }

    /// 封盘：停止收答，但先不公布。主播这时候可以讲一讲。
    pub fn lock(&mut self) {
        if self.phase == Phase::Collecting {
            self.phase = Phase::Locked;
        }
    }

    /// 公布答案并给出统计。从 Collecting 直接调用也行（超时自动公布）。
    pub fn reveal(&mut self) -> Stats {
        self.phase = Phase::Revealed;

        let correct = self.question.correct;
        let winners: Vec<Answer> = self
            .answers
            .iter()
            .filter(|a| a.is_correct(&self.question))
            .take(MAX_WINNERS)
            .cloned()
            .collect();

        let participants = self.answers.len() as u32;
        let correct_count = self.tally[correct.index()];

        Stats {
            correct,
            tally: self.tally,
            participants,
            correct_count,
            accuracy: if participants == 0 {
                0.0
            } else {
                correct_count as f32 / participants as f32
            },
            winners,
        }
    }

    /// 「疑似」日志，最新的在后。
    pub fn rejections(&self) -> &[(String, String, Rejection)] {
        &self.rejections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> Question {
        Question {
            id: 1,
            prompt: "학교".into(),
            options: ["医院".into(), "学校".into(), "公司".into(), "家".into()],
            correct: Choice::B,
        }
    }

    fn danmaku(user: &str, content: &str, seq: u64) -> Danmaku {
        Danmaku {
            seq,
            user_id: user.into(),
            nickname: user.into(),
            content: content.into(),
        }
    }

    #[test]
    fn presenting_does_not_start_the_clock() {
        // **回归测试。** 这是 AI 主持的公平性基础：念题干的那七八秒里
        // 计时器不能跑。跑了的话，手速加分就发给了没在听讲解、
        // 自己扫一眼就作答的人——而这个玩法奖励的应该是懂韩语。
        let t0 = Instant::now();
        let r = Round::present(question(), Duration::from_secs(60), t0);

        assert!(r.is_presenting());
        assert!(!r.is_collecting(), "还没开始收答");
        assert_eq!(
            r.remaining(t0),
            Duration::from_secs(60),
            "倒计时该显示满格，不是 0——0 看着像已经结束了"
        );

        // 就算过了很久，也不该"过期"。
        let later = t0 + Duration::from_secs(120);
        assert!(!r.expired(later), "还没开始计时，谈不上过期");
        assert_eq!(r.remaining(later), Duration::from_secs(60), "一直满格");
    }

    #[test]
    fn presenting_rejects_answers() {
        // 念题期间作答不计分，也不该进「疑似」日志——那是有效弹幕，
        // 只是时机没到。
        let t0 = Instant::now();
        let mut r = Round::present(question(), Duration::from_secs(60), t0);

        assert_eq!(r.submit(&danmaku("u1", "B", 1), t0), Outcome::Ignored);
        assert!(r.answers.is_empty());
        assert_eq!(r.tally, [0; 4]);
        assert!(r.rejections().is_empty(), "时机没到不算判定失败");
    }

    #[test]
    fn start_collecting_opens_the_full_window_from_that_moment() {
        // 窗口从"开始收答"那一刻算，不从"题目亮出来"算。
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(8); // 念了 8 秒题干
        let mut r = Round::present(question(), Duration::from_secs(60), t0);
        r.start_collecting(t1);

        assert!(r.is_collecting());
        assert_eq!(r.opened_at, t1, "开始时刻该是念完那一刻");
        assert_eq!(r.deadline, t1 + Duration::from_secs(60), "整整 60 秒");
        assert_eq!(r.remaining(t1), Duration::from_secs(60));
        assert!(!r.expired(t1 + Duration::from_secs(59)));
        assert!(r.expired(t1 + Duration::from_secs(60)));
    }

    #[test]
    fn start_collecting_is_idempotent() {
        // 编排层会因为"语音播完了"和"兜底超时"两条路都走到这里，
        // 重复调用不能把窗口往后推——那等于凭空延长收答时间。
        let t0 = Instant::now();
        let mut r = Round::present(question(), Duration::from_secs(60), t0);
        r.start_collecting(t0);
        let deadline = r.deadline;
        r.start_collecting(t0 + Duration::from_secs(30));

        assert_eq!(r.deadline, deadline, "重复调用不该重置截止时刻");
    }

    #[test]
    fn answers_count_normally_after_the_presentation() {
        // 转成收答之后，一切照旧。
        let t0 = Instant::now();
        let mut r = Round::present(question(), Duration::from_secs(60), t0);
        r.start_collecting(t0);

        assert_eq!(r.submit(&danmaku("u1", "B", 1), t0), Outcome::Counted(Choice::B));
        assert_eq!(r.tally, [0, 1, 0, 0]);
    }

    #[test]
    fn open_still_starts_collecting_immediately() {
        // `open` 的语义没变——手动主持那条路还走它。
        let t0 = Instant::now();
        let r = Round::open(question(), Duration::from_secs(60), t0);
        assert!(r.is_collecting(), "open 应该立刻开始收答");
        assert!(!r.is_presenting());
    }

    #[test]
    fn counts_answers_and_tallies() {
        let t0 = Instant::now();
        let mut r = Round::open(question(), Duration::from_secs(60), t0);

        assert_eq!(r.submit(&danmaku("u1", "B", 0), t0), Outcome::Counted(Choice::B));
        assert_eq!(r.submit(&danmaku("u2", "b", 1), t0), Outcome::Counted(Choice::B));
        assert_eq!(r.submit(&danmaku("u3", "我选C", 2), t0), Outcome::Counted(Choice::C));
        assert_eq!(r.submit(&danmaku("u4", "Ａ", 3), t0), Outcome::Counted(Choice::A));

        assert_eq!(r.tally, [1, 2, 1, 0]);
        assert_eq!(r.answers.len(), 4);
    }

    #[test]
    fn same_user_only_counts_once() {
        let t0 = Instant::now();
        let mut r = Round::open(question(), Duration::from_secs(60), t0);

        assert_eq!(r.submit(&danmaku("u1", "A", 0), t0), Outcome::Counted(Choice::A));
        // 改口不算，第一次说了算。
        assert_eq!(r.submit(&danmaku("u1", "B", 1), t0), Outcome::Duplicate);
        assert_eq!(r.tally, [1, 0, 0, 0]);
    }

    #[test]
    fn noise_goes_to_rejection_log() {
        let t0 = Instant::now();
        let mut r = Round::open(question(), Duration::from_secs(60), t0);

        assert_eq!(
            r.submit(&danmaku("u1", "haha", 0), t0),
            Outcome::Rejected(Rejection::Ambiguous { letters: "AA".into() })
        );
        assert_eq!(
            r.submit(&danmaku("u2", "打卡", 1), t0),
            Outcome::Rejected(Rejection::NoChoice)
        );

        assert!(r.tally.iter().all(|&n| n == 0), "噪声不该进票数");
        assert_eq!(r.rejections().len(), 2);
    }

    #[test]
    fn nothing_counts_after_deadline_or_lock() {
        let t0 = Instant::now();
        let window = Duration::from_secs(60);
        let mut r = Round::open(question(), window, t0);

        let after = t0 + window + Duration::from_millis(1);
        assert_eq!(r.submit(&danmaku("u1", "B", 0), after), Outcome::Ignored);

        // 窗口内封盘之后同样不再收。
        let mut r = Round::open(question(), window, t0);
        r.lock();
        assert_eq!(r.submit(&danmaku("u1", "B", 0), t0), Outcome::Ignored);
    }

    #[test]
    fn reveal_reports_accuracy_and_winners_in_arrival_order() {
        let t0 = Instant::now();
        let mut r = Round::open(question(), Duration::from_secs(60), t0);

        r.submit(&danmaku("wrong", "A", 0), t0);
        r.submit(&danmaku("first", "B", 1), t0);
        r.submit(&danmaku("second", "b", 2), t0);
        r.submit(&danmaku("third", "B", 3), t0);

        let stats = r.reveal();
        assert_eq!(stats.correct, Choice::B);
        assert_eq!(stats.tally, [1, 3, 0, 0]);
        assert_eq!(stats.participants, 4);
        assert_eq!(stats.correct_count, 3);
        assert!((stats.accuracy - 0.75).abs() < f32::EPSILON);
        assert_eq!(
            stats.winners.iter().map(|w| w.nickname.as_str()).collect::<Vec<_>>(),
            ["first", "second", "third"],
        );
        assert_eq!(r.phase, Phase::Revealed);
    }

    #[test]
    fn reveal_with_no_answers_does_not_divide_by_zero() {
        let t0 = Instant::now();
        let mut r = Round::open(question(), Duration::from_secs(60), t0);
        let stats = r.reveal();
        assert_eq!(stats.participants, 0);
        assert_eq!(stats.accuracy, 0.0);
        assert!(stats.winners.is_empty());
    }

    #[test]
    fn rejection_log_is_bounded() {
        let t0 = Instant::now();
        let mut r = Round::open(question(), Duration::from_secs(60), t0);
        for i in 0..MAX_REJECTIONS as u64 + 50 {
            r.submit(&danmaku(&format!("u{i}"), "打卡", i), t0);
        }
        assert_eq!(r.rejections().len(), MAX_REJECTIONS);
    }
}
