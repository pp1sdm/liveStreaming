//! 弹幕来源抽象。
//!
//! 整个项目里唯一认识「弹幕从哪来」的地方。上面所有逻辑（判定、计分、画面）
//! 都只跟 [`livestream_core::Danmaku`] 打交道，所以换源不动上层。
//!
//! 现在有两个实现：
//!
//! * [`BarrageGrabSource`] —— 连本地转发工具的 WebSocket，开播用这个。
//! * [`MockSource`] —— 自己造弹幕，离线开发和演示用。

mod barrage;
mod mock;

pub use barrage::{BarrageGrabSource, Chat, DEFAULT_URL, parse_chat};
pub use mock::MockSource;

use async_trait::async_trait;
use livestream_core::Danmaku;
use tokio::sync::mpsc;

/// 弹幕源。
///
/// 实现者负责把自家协议翻译成 [`Danmaku`]，并在**收到的那一刻**分配 `seq`。
/// `seq` 是全局单调递增的到达序号，用来排「谁先答对」——刻意不用墙钟时间，
/// 因为推送延迟对每个观众都不一样，用序号至少对所有人在同一把尺子上。
#[async_trait]
pub trait DanmakuSource: Send + Sync + 'static {
    /// 人类可读的名字，用于日志和开播自检。
    fn name(&self) -> &'static str;

    /// 持续把弹幕推进 `tx`，直到 `tx` 被关闭或发生不可恢复的错误。
    ///
    /// 注意：这里应该**一直推**，不要因为「现在不在收答」就停。封盘是消费端
    /// 的事（engine 只在自己的窗口内消费），不是生产端的事。
    async fn run(&self, tx: mpsc::Sender<Danmaku>) -> anyhow::Result<()>;
}

/// 递增序号分配器。真实实现应该持有一个，在收到弹幕时调用。
#[derive(Debug, Default)]
pub struct SeqCounter(std::sync::atomic::AtomicU64);

impl SeqCounter {
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}
