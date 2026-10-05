//! 本地弹幕转发工具的 WebSocket 源。
//!
//! 抖音没有给个人开发者「读自己直播间弹幕」的公开接口，官方小玩法那条路要认证
//! 加软著，周期按月算。所以走成熟的第三条路：在开播这台机器上跑一个转发工具，
//! 它用登录态去拉抖音的弹幕，再在本地用 WebSocket 吐出来。
//!
//! 默认对接 [BarrageGrab](https://github.com/OrdinaryRoad-Atlas/BarrageGrab)
//! （`ws://127.0.0.1:8888`），它把各种消息统一成一份 JSON，用 `Type` 区分：
//!
//! ```text
//!   1 进场   2 关注   3 弹幕   4 点赞   5 礼物   7 统计   8 状态变更   9 粉丝团
//! ```
//!
//! 我们只关心 3，其余的直接丢掉——引擎不需要知道有人点了赞。
//!
//! **解析刻意写得很宽容。** 这些工具的字段名在不同版本之间飘（`Nickname` /
//! `nickname` / `NickName`，`Id` 时而是数字时而是字符串），上游一改我们的直播
//! 就哑了。所以这里按「找得到就用，找不到就退回默认」来解，而不是用强类型
//! 反序列化——宁可多收一条噪声，也不能一条都收不到。

use async_trait::async_trait;
use futures::StreamExt;
use livestream_core::Danmaku;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::{DanmakuSource, SeqCounter};

/// BarrageGrab 默认监听这里。
pub const DEFAULT_URL: &str = "ws://127.0.0.1:8888";

/// 消息类型码：弹幕。
const TYPE_CHAT: i64 = 3;

/// 断线后第一次重连等多久。之后翻倍。
const INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

/// 重连间隔上限。转发工具重启通常几秒就好，等太久反而错过开播。
const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

/// 解出来的一条弹幕，还没发号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub user_id: String,
    pub nickname: String,
    pub content: String,
}

impl Chat {
    fn into_danmaku(self, seq: u64) -> Danmaku {
        Danmaku {
            seq,
            user_id: self.user_id,
            nickname: self.nickname,
            content: self.content,
        }
    }
}

/// 把一条推送原文解成弹幕。不是弹幕（礼物、点赞、进场……）返回 `None`。
pub fn parse_chat(raw: &str) -> Option<Chat> {
    let v: Value = serde_json::from_str(raw).ok()?;

    if type_code(&v).is_some_and(|code| code != TYPE_CHAT) {
        return None;
    }
    // 没有 Type 字段就往下解——有些转发工具不带这个。真正兜底的是下面
    // 「必须有非空 Content」，点赞和统计消息过不了这一关。

    let data = v.get("Data").or_else(|| v.get("data")).unwrap_or(&v);

    let content = pick_str(data, &["Content", "content", "Msg", "Message", "message"])?;
    if content.trim().is_empty() {
        return None;
    }

    let user = data.get("User").or_else(|| data.get("user"));

    let nickname = user
        .and_then(|u| pick_str(u, &["Nickname", "nickname", "NickName", "UserName", "user_name"]))
        .unwrap_or_else(|| "观众".to_owned());

    // 拿不到 id 就用昵称顶着。同人只取第一次作答是靠 id 去重的，退化成昵称
    // 之后改了名会被当成两个人，但总比整条弹幕丢掉强。
    let user_id = user
        .and_then(pick_id)
        .unwrap_or_else(|| nickname.clone());

    Some(Chat {
        user_id,
        nickname,
        content,
    })
}

fn type_code(v: &Value) -> Option<i64> {
    let t = v.get("Type").or_else(|| v.get("type"))?;
    // 有的版本给数字，有的给字符串。
    t.as_i64().or_else(|| t.as_str()?.parse().ok())
}

fn pick_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::to_owned)
}

fn pick_id(user: &Value) -> Option<String> {
    const KEYS: [&str; 6] = ["Id", "id", "UserId", "user_id", "SecUid", "sec_uid"];

    KEYS.iter().find_map(|k| {
        let v = user.get(*k)?;
        // 抖音的 id 在部分版本里是数字，超过 2^53 会丢精度，所以字符串优先。
        if let Some(s) = v.as_str() {
            (!s.is_empty()).then(|| s.to_owned())
        } else {
            v.as_i64().map(|n| n.to_string())
        }
    })
}

/// 连本地转发工具的弹幕源。
pub struct BarrageGrabSource {
    url: String,
    /// 到达序号。重连之后接着往下发，不会从零开始——上层靠它排先后。
    seq: SeqCounter,
}

impl BarrageGrabSource {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            seq: SeqCounter::default(),
        }
    }

    /// 连本机默认端口，也就是转发工具装在开播这台机器上的情况。
    pub fn local() -> Self {
        Self::new(DEFAULT_URL)
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// 连上并一直读，直到连接断了。
    async fn pump(&self, tx: &mpsc::Sender<Danmaku>) -> anyhow::Result<()> {
        let (mut ws, _) = tokio_tungstenite::connect_async(&self.url).await?;
        tracing::info!(url = %self.url, "弹幕源已连上");

        while let Some(frame) = ws.next().await {
            let text = match frame? {
                Message::Text(t) => t.to_string(),
                // 个别版本会把 JSON 塞进二进制帧。
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Close(_) => break,
                // Ping/Pong 由 tungstenite 自己回，这里不用管。
                _ => continue,
            };

            let Some(chat) = parse_chat(&text) else {
                continue;
            };

            if tx.send(chat.into_danmaku(self.seq.next())).await.is_err() {
                // 接收端没了，正常收摊。
                return Ok(());
            }
        }

        Ok(())
    }
}

#[async_trait]
impl DanmakuSource for BarrageGrabSource {
    fn name(&self) -> &'static str {
        "barragegrab"
    }

    async fn run(&self, tx: mpsc::Sender<Danmaku>) -> anyhow::Result<()> {
        let mut backoff = INITIAL_BACKOFF;

        loop {
            match self.pump(&tx).await {
                // 连上过又断了：转发工具多半是重启，退避从头来。
                Ok(()) => backoff = INITIAL_BACKOFF,
                Err(err) => {
                    tracing::warn!(url = %self.url, error = %err, "弹幕源连接失败");
                }
            }

            if tx.is_closed() {
                tracing::info!("弹幕队列已关闭，弹幕源退出");
                return Ok(());
            }

            tracing::warn!(secs = backoff.as_secs_f32(), "弹幕源重连中");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_out_a_chat_message() {
        let raw = r#"{"Type":3,"Data":{"User":{"Id":123,"Nickname":"小鹿"},"Content":"B"}}"#;
        assert_eq!(
            parse_chat(raw),
            Some(Chat {
                user_id: "123".into(),
                nickname: "小鹿".into(),
                content: "B".into(),
            })
        );
    }

    #[test]
    fn everything_that_is_not_chat_is_dropped() {
        // 礼物、点赞、进场。这些进了判定层只会变成噪声日志。
        for kind in [1, 2, 4, 5, 7, 8, 9] {
            let raw = format!(
                r#"{{"Type":{kind},"Data":{{"User":{{"Id":1,"Nickname":"x"}},"Content":"A"}}}}"#
            );
            assert_eq!(parse_chat(&raw), None, "Type={kind} 不该被当成弹幕");
        }
    }

    #[test]
    fn lowercase_keys_work_too() {
        // douyinLive 那类工具用小写字段。
        let raw = r#"{"type":3,"data":{"user":{"id":"u9","nickname":"Mina"},"content":"C"}}"#;
        assert_eq!(
            parse_chat(raw),
            Some(Chat {
                user_id: "u9".into(),
                nickname: "Mina".into(),
                content: "C".into(),
            })
        );
    }

    #[test]
    fn numeric_and_string_ids_both_land_as_strings() {
        let num = r#"{"Type":3,"Data":{"User":{"Id":7001234567890123456,"Nickname":"a"},"Content":"A"}}"#;
        let s = r#"{"Type":3,"Data":{"User":{"Id":"sec_abc","Nickname":"b"},"Content":"A"}}"#;
        assert_eq!(parse_chat(num).unwrap().user_id, "7001234567890123456");
        assert_eq!(parse_chat(s).unwrap().user_id, "sec_abc");
    }

    #[test]
    fn missing_user_still_yields_a_usable_danmaku() {
        let raw = r#"{"Type":3,"Data":{"Content":"A"}}"#;
        let chat = parse_chat(raw).unwrap();
        assert_eq!(chat.nickname, "观众");
        assert_eq!(
            chat.user_id, "观众",
            "拿不到 id 时退回昵称，至少还能靠它去重"
        );
    }

    #[test]
    fn message_without_type_is_still_tried() {
        // 有些转发工具根本不带 Type，只看 Content。
        let raw = r#"{"Data":{"User":{"Id":1,"Nickname":"a"},"Content":"D"}}"#;
        assert_eq!(parse_chat(raw).unwrap().content, "D");
    }

    #[test]
    fn stats_message_without_type_is_filtered_by_content() {
        // 统计消息没有 Content，靠这一条挡下来。
        let raw = r#"{"Data":{"OnlineCount":1234}}"#;
        assert_eq!(parse_chat(raw), None);
    }

    #[test]
    fn blank_content_is_not_a_danmaku() {
        let raw = r#"{"Type":3,"Data":{"User":{"Id":1,"Nickname":"a"},"Content":"   "}}"#;
        assert_eq!(parse_chat(raw), None);
    }

    #[test]
    fn garbage_does_not_panic() {
        for raw in ["", "not json", "[]", "null", "{}", r#"{"Type":"三"}"#] {
            assert_eq!(parse_chat(raw), None, "{raw:?} 应该安静地解不出来");
        }
    }

    #[test]
    fn full_width_answer_survives_parsing() {
        // 判定层要 NFKC 归一化，这里先把原文完整传下去。
        let raw = r#"{"Type":3,"Data":{"User":{"Id":1,"Nickname":"a"},"Content":"Ｂ"}}"#;
        assert_eq!(parse_chat(raw).unwrap().content, "Ｂ");
    }

    #[tokio::test]
    async fn source_reports_a_bad_url_instead_of_hanging() {
        // 端口上什么都没有的时候要快速失败并进重连，而不是把进程挂住。
        let source = BarrageGrabSource::new("ws://127.0.0.1:1");
        assert_eq!(source.name(), "barragegrab");
        assert_eq!(source.url(), "ws://127.0.0.1:1");

        let (tx, rx) = mpsc::channel(4);
        drop(rx); // 接收端已经没了，run 应该立刻收摊
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), source.run(tx)).await;
    }
}
