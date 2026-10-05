//! 答题引擎：判定一条弹幕是不是作答，以及管理一道题的轮次。
//!
//! 纯逻辑，没有 IO、没有时间源（窗口用调用方传进来的 `Instant`），所以全部
//! 可以同步单测。

pub mod judge;
pub mod round;
pub mod score;

pub use judge::{Rejection, Verdict, judge};
pub use round::{Outcome, Phase, Round, Stats};
pub use score::{Award, Leaderboard, Player, RankRow};
