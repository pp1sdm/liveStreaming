//! 假弹幕源。用于在没有抖音、没有网络的情况下把整条链路跑通。
//!
//! 剧本刻意包含了几种真实直播间一定会出现的边界样本：全角字母、小写、
//! 「我选C」这种带前缀的、以及 `haha` / `打卡` / `666` 这类会被判定层
//! 丢掉的噪声。

use std::time::Duration;

use async_trait::async_trait;
use livestream_core::Danmaku;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

use crate::{DanmakuSource, SeqCounter};

/// 一轮剧本：(昵称, 弹幕内容)。
///
/// 用户 id 直接用昵称，方便在 ticker 里肉眼对。真实源会有独立的 uid。
const SCRIPT: &[(&str, &str)] = &[
    ("小鹿", "B"),
    ("阿泽", "b"),
    ("Mina", "Ｂ"),      // 全角，中文输入法常出
    ("老王", "我选C"),
    ("韩梅梅", "A吧"),
    ("路人甲", "haha"),   // 提取出 AA，丢弃
    ("默默", "打卡"),      // 提取不到，丢弃
    ("666", "666"),       // 提取不到，丢弃
    ("小鹿", "D"),        // 同一人改口，应该只算第一次
    ("新来的", "选a"),
    ("보라", "정답은 D"),
    ("阿泽", "我觉得是 B 吧"),
    ("夜猫子", "c"),
    ("路人乙", "Ａ"),
    ("潜水员", "答案是 C"),
];

/// 按固定节奏吐剧本的假源。循环播放，永不停止。
pub struct MockSource {
    /// 每两条弹幕之间的间隔。
    pub tick: Duration,
}

impl MockSource {
    pub fn new(tick: Duration) -> Self {
        Self { tick }
    }

    /// 演示用的默认节奏。
    pub fn demo() -> Self {
        Self::new(Duration::from_millis(700))
    }
}

#[async_trait]
impl DanmakuSource for MockSource {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn run(&self, tx: mpsc::Sender<Danmaku>) -> anyhow::Result<()> {
        let seq = SeqCounter::default();
        let mut ticker = interval(self.tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        for (nickname, content) in SCRIPT.iter().cycle() {
            ticker.tick().await;

            let danmaku = Danmaku {
                seq: seq.next(),
                user_id: (*nickname).to_string(),
                nickname: (*nickname).to_string(),
                content: (*content).to_string(),
            };

            // 接收端消失说明程序在退出，正常收摊。
            if tx.send(danmaku).await.is_err() {
                return Ok(());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn mock_emits_script_in_order_and_keeps_going() {
        let (tx, mut rx) = mpsc::channel(64);
        let src = MockSource::new(Duration::from_millis(10));

        let handle = tokio::spawn(async move { src.run(tx).await });

        // 取满一轮剧本多一点，确认它是循环而不是跑完就停。
        let mut got = Vec::new();
        for _ in 0..SCRIPT.len() + 3 {
            got.push(rx.recv().await.expect("source closed early"));
        }

        assert_eq!(got[0].content, "B");
        assert_eq!(got[0].seq, 0);
        assert_eq!(got[SCRIPT.len()].content, "B", "应该从头循环");
        assert!(got.windows(2).all(|w| w[0].seq < w[1].seq), "seq 必须递增");

        drop(rx);
        assert!(handle.await.unwrap().is_ok(), "接收端关闭时应干净退出");
    }
}
