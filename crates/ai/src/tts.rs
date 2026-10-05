//! 语音合成：把一句台词变成 MP3 字节。
//!
//! 为什么是服务端合成而不是页面里 `speechSynthesis`——OBS 的浏览器源是 CEF，
//! 而 CEF 没有实现 Web Speech 后端：`getVoices()` 返回空**且永远是空**，
//! 不是常见的异步时序问题，等多久都不会有。就算响了，OBS 那边也录不进去
//! （OBS issue #404：勾了 "Control audio via OBS" 后 `start` 事件都不触发）。
//! 所以语音只能在这里合成好，当普通音频文件推给页面，和音效走同一条路。
//!
//! 用的是微软 Edge 的在线朗读接口。它不要钱、不要 key，音色也够用，
//! 代价是**协议随时可能变**，而且它靠时间戳防重放（见 [`sec_ms_gec`]）。
//! 所以这一切藏在 [`Tts`] 后面：将来要换成云厂商 TTS，只改这一个文件。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// 台词的语言。决定用哪个音色读。
///
/// 这不是过度设计：中文音色读韩语会把「학교」按汉字音读成 xiào，
/// 观众一听就出戏。讲稿天然是中韩混着的（「这次的词是 학교」），
/// 所以必须一段一段按语言分开发音色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    Ko,
}

/// 一段同语言的台词。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub lang: Lang,
    pub text: String,
}

impl Segment {
    pub fn zh(text: impl Into<String>) -> Self {
        Self { lang: Lang::Zh, text: text.into() }
    }

    pub fn ko(text: impl Into<String>) -> Self {
        Self { lang: Lang::Ko, text: text.into() }
    }
}

/// 用哪两个音色。放在配置里而不是写死，是想换个声音时不用重编译。
#[derive(Debug, Clone)]
pub struct Voices {
    pub zh: String,
    pub ko: String,
}

impl Default for Voices {
    fn default() -> Self {
        Self {
            zh: "zh-CN-XiaoxiaoNeural".to_owned(),
            ko: "ko-KR-SunHiNeural".to_owned(),
        }
    }
}

impl Voices {
    pub fn pick(&self, lang: Lang) -> &str {
        match lang {
            Lang::Zh => &self.zh,
            Lang::Ko => &self.ko,
        }
    }
}

/// 合成器。藏在这个接口后面，是为了让上层的编排逻辑能用假的实现来测——
/// 单元测试不该真的去打微软的服务器。
#[async_trait]
pub trait Tts: Send + Sync {
    /// 把一段台词合成成 MP3 字节。
    async fn say(&self, text: &str, voice: &str) -> Result<Vec<u8>>;

    /// 把一串分段合成，拼成一个 MP3。
    ///
    /// **拼成一个**而不是让页面一段一段播：分段播在每段之间会有一道
    /// 几十毫秒的缝，中韩切换的地方尤其明显，听起来像卡了一下。
    /// MP3 是分帧的，解码器能自己从任意帧边界重新同步，直接接起来就能放。
    /// 代价是时长不能从文件头读——所以时长由我们这边按字节数估（见 [`estimate_ms`]）。
    ///
    /// **并发合成，但不是不限量地并发。** 每段都要单独握一次 WebSocket + TLS，
    /// 实测一次往返一秒多。串行的话报题那几段加起来要十一秒——观众对着空屏幕
    /// 干等十几秒，直播就废了。
    ///
    /// 但也不能全放出去：一次开出四五个连接，微软那边会回 503
    /// （实测踩到过，那一句讲解就没了）。所以用 [`buffered`](StreamExt::buffered)
    /// 把并发卡在 [`MAX_CONCURRENT_SAY`] ——它同时保证**按输入顺序**取结果，
    /// 拼接顺序不受完成先后影响。
    async fn say_all(&self, segments: &[Segment], voices: &Voices) -> Result<Vec<u8>> {
        use futures_util::StreamExt;

        let wanted: Vec<&Segment> = segments
            .iter()
            .filter(|seg| !seg.text.trim().is_empty())
            .collect();

        // 先把 future 收进 Vec 再交给 `stream::iter`。直接传 map 出来的
        // 迭代器过不了借用检查（迭代器产出的是 `&&Segment`，闭包对两个
        // 生命周期的要求对不上）；收成 Vec 之后借用关系就具体了。
        let jobs: Vec<_> = wanted
            .iter()
            .map(|seg| self.say(&seg.text, voices.pick(seg.lang)))
            .collect();

        let parts: Vec<Result<Vec<u8>>> = futures_util::stream::iter(jobs)
            .buffered(MAX_CONCURRENT_SAY)
            .collect()
            .await;

        let mut out = Vec::new();
        for (seg, part) in wanted.iter().zip(parts) {
            let part = part.with_context(|| format!("合成失败：{:?} {:?}", seg.lang, seg.text))?;
            out.extend_from_slice(&part);
        }
        if out.is_empty() {
            bail!("所有分段都是空的，没有可合成的音频");
        }
        Ok(out)
    }
}

/// 同时最多开几条合成连接。
///
/// 一台机器一场直播就一个客户端，2 条已经能把报题那几段从十一秒压到两三秒；
/// 再往上加，微软那边会开始回 503——实测四条并发就中过一次，
/// 代价是那一句讲解整个没了。
const MAX_CONCURRENT_SAY: usize = 2;

/// 按字节数估时长。48kbps 单声道 = 6000 字节/秒。
///
/// 只用来做「播完了吗」的兜底超时，不需要精确：真正播完是靠页面回报
/// （`POST /api/spoken`），这个估算只是在页面挂了的时候别把直播卡死。
pub fn estimate_ms(mp3: &[u8]) -> u64 {
    const BYTES_PER_SEC: u64 = 6_000;
    (mp3.len() as u64) * 1000 / BYTES_PER_SEC
}

/// 受托客户端令牌。这是 Edge 浏览器扩展里写死的公开常量，不是密钥。
const TRUSTED_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";

const HOST: &str = "speech.platform.bing.com";
const PATH: &str = "/consumer/speech/synthesize/readaloud/edge/v1";

/// 输出格式。24kHz / 48kbps 单声道是实测能拿到的，语音够用，体积也小。
const OUTPUT_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";

/// `Sec-MS-GEC-Version`。
///
/// **这个值其实无关紧要**——实测任意版本号（130 / 131 / 140 / 143）都能连上。
/// 留着它是因为协议里有这个参数，不是因为它挑版本。曾经我以为是版本号太旧
/// 导致的 403，那是误诊，真正的原因是 [`sec_ms_gec`] 里的时间戳。
const GEC_VERSION: &str = "1-130.0.2849.68";

/// 算 `Sec-MS-GEC`：一个防重放的令牌，**掺了当前系统时间**。
///
/// 算法：取 Windows 文件时间（1601 年起的 100 纳秒数），向下取整到 5 分钟，
/// 拼上受托令牌做 SHA-256。微软那边拿同样的算法算一遍比对。
///
/// 这个函数是整条链路里最容易出事的地方，因为它依赖本机时钟：
///
/// - **每次连接都必须重算，绝不能缓存。** 有效期只有 5 分钟（取整窗口），
///   缓存下来的 token 会在窗口翻页的那一刻失效。我一开始调试时在一个长循环里
///   复用了同一个 token，结果整轮全是 403，白白怀疑了版本号和请求头半天。
/// - **时钟偏了就直接 403，而且看起来和"被墙了"一模一样。** 实测故意偏移
///   ±1 小时或 ±1 天都是 403，只有当前时间才放行。这在 WSL2 上是真实风险：
///   宿主机休眠再唤醒之后，WSL 的时钟经常停在休眠前不动，
///   偏移一下就超过 5 分钟，语音从此永久失败。所以 [`EdgeTts::say`] 见到 403
///   会重算一次重试，还不行就报一句能照着修的错（提示校准时钟），
///   而不是把一个光秃秃的 403 抛上去让人猜。
fn sec_ms_gec(now: SystemTime) -> String {
    /// 1601-01-01 到 1970-01-01 之间的秒数。
    const WIN_EPOCH: f64 = 11_644_473_600.0;
    /// 5 分钟，换算成 100 纳秒的格数。
    const ROUND_TO: f64 = 3_000_000_000.0;

    let secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let mut ticks = (secs + WIN_EPOCH) * 1e9 / 100.0;
    ticks -= ticks % ROUND_TO;

    let mut hasher = Sha256::new();
    hasher.update(format!("{ticks:.0}{TRUSTED_TOKEN}").as_bytes());
    format!("{:X}", hasher.finalize())
}

/// 生成 `ConnectionId`：32 位十六进制的随机串。
///
/// 不为这个引 `uuid` crate——只需要"这次连接不重样"，用标准库的随机种子
/// 凑够 128 位就够了，省一个依赖。
fn connection_id() -> String {
    use std::hash::{BuildHasher, Hasher};

    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

/// Edge 要的时间戳，形如 `Wed Oct 01 2026 12:00:00 GMT+0000 (Coordinated Universal Time)`。
///
/// 手写而不是引 `chrono`：只用这一次，格式还是固定的英文。
fn timestamp(now: SystemTime) -> String {
    /// 星期名，1970-01-01 是星期四。
    const WEEKDAY: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTH: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, tod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);

    format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        WEEKDAY[days.rem_euclid(7) as usize],
        MONTH[(m - 1) as usize],
        d,
        y,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
    )
}

/// 把「1970-01-01 起的天数」换成公历年月日。
///
/// Howard Hinnant 的 `civil_from_days`。用它是因为它对负数（1970 年以前）
/// 也是对的，不用特判，也就不会有那种"闰年那天差一天"的隐蔽错误。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// SSML 是 XML，台词里的 `&` `<` 之类必须转义，否则整个请求会被判为格式错。
///
/// 台词里有 AI 生成的文本和观众昵称，出现 `<` 是完全可能的——
/// 尤其是弹幕互动那条路，不转义就等于让别人往我们的请求里塞标签。
fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Edge 在线语音的实现。
pub struct EdgeTts {
    client: reqwest::Client,
}

impl EdgeTts {
    pub fn new() -> Result<Self> {
        // 这个 client 只用来拉音色表（方便排查），合成走 WebSocket。
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("建 HTTP client 失败")?;
        Ok(Self { client })
    }

    /// 列出所有可用音色。排障用：怀疑音色名写错时打一下看看。
    pub async fn voices(&self) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(rename = "ShortName")]
            short_name: String,
        }

        let list: Vec<Raw> = self
            .client
            .get(format!("https://{HOST}/consumer/speech/synthesize/readaloud/voices/list"))
            .query(&[("trustedclienttoken", TRUSTED_TOKEN)])
            .send()
            .await
            .context("拉音色表失败")?
            .error_for_status()
            .context("音色表返回了错误状态")?
            .json()
            .await
            .context("音色表不是合法 JSON")?;

        Ok(list.into_iter().map(|v| v.short_name).collect())
    }

    /// 连一次、合成一段、断开。
    async fn synth_once(&self, text: &str, voice: &str) -> Result<Vec<u8>> {
        let url = format!(
            "wss://{HOST}{PATH}?TrustedClientToken={TRUSTED_TOKEN}\
             &ConnectionId={}&Sec-MS-GEC={}&Sec-MS-GEC-Version={GEC_VERSION}",
            connection_id(),
            sec_ms_gec(SystemTime::now()),
        );

        let mut req = url.into_client_request().context("WebSocket 请求建不起来")?;
        {
            let h = req.headers_mut();
            // Origin 和 UA 都照抄 Edge 扩展。实测这两个值其实不参与校验
            // （换任何值都能连上），但保持和真浏览器一致没坏处。
            h.insert("Origin", "chrome-extension://jdiccldimpdaibmpdkjnckianbfold".parse()?);
            h.insert(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0"
                    .parse()?,
            );
            h.insert("Pragma", "no-cache".parse()?);
            h.insert("Cache-Control", "no-cache".parse()?);
        }

        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .context("连 Edge 语音服务失败")?;
        let (mut write, mut read) = ws.split();

        let now = timestamp(SystemTime::now());
        let cfg = format!(
            "X-Timestamp:{now}\r\n\
             Content-Type:application/json; charset=utf-8\r\n\
             Path:speech.config\r\n\r\n\
             {{\"context\":{{\"synthesis\":{{\"audio\":{{\
             \"metadataoptions\":{{\"sentenceBoundaryEnabled\":\"false\",\
             \"wordBoundaryEnabled\":\"false\"}},\
             \"outputFormat\":\"{OUTPUT_FORMAT}\"}}}}}}}}"
        );
        write.send(Message::Text(cfg.into())).await.context("发 speech.config 失败")?;

        // 请求 id 是 32 位十六进制，和 ConnectionId 同款格式。
        let ssml = format!(
            "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
             <voice name='{voice}'><prosody pitch='+0Hz' rate='+0%' volume='+0%'>{}</prosody></voice>\
             </speak>",
            escape_xml(text)
        );
        let msg = format!(
            "X-RequestId:{}\r\n\
             Content-Type:application/ssml+xml\r\n\
             X-Timestamp:{now}Z\r\n\
             Path:ssml\r\n\r\n{ssml}",
            connection_id(),
        );
        write.send(Message::Text(msg.into())).await.context("发 ssml 失败")?;

        let mut audio = Vec::new();
        while let Some(frame) = read.next().await {
            match frame.context("收帧失败")? {
                Message::Text(text) => {
                    // 文本帧是控制消息，`Path:turn.end` 表示这段合成完了。
                    if text.as_str().contains("Path:turn.end") {
                        break;
                    }
                }
                Message::Binary(data) => {
                    // 二进制帧的格式：2 字节大端 header 长度 + header + 音频。
                    // header 里有 `Path:audio` 才是音频，别的（比如
                    // `Path:response`）要丢掉。
                    if data.len() < 2 {
                        continue;
                    }
                    let hlen = u16::from_be_bytes([data[0], data[1]]) as usize;
                    if data.len() < 2 + hlen {
                        continue;
                    }
                    let head = String::from_utf8_lossy(&data[2..2 + hlen]);
                    if head.contains("Path:audio") {
                        audio.extend_from_slice(&data[2 + hlen..]);
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }

        let _ = write.close().await;
        if audio.is_empty() {
            bail!("合成结束但一个音频字节都没收到");
        }
        Ok(audio)
    }
}

#[async_trait]
impl Tts for EdgeTts {
    async fn say(&self, text: &str, voice: &str) -> Result<Vec<u8>> {
        match self.synth_once(text, voice).await {
            Ok(mp3) => Ok(mp3),
            Err(err) if is_unavailable(&err) => {
                // 503：服务端一时忙不过来（并发开太多时会碰到）。
                // 等一小会儿再来一次，通常就过了。这场直播还等着念这句。
                tracing::warn!("Edge 语音返回 503，等一下重试");
                tokio::time::sleep(RETRY_AFTER_503).await;
                self.synth_once(text, voice).await
            }
            Err(err) => {
                // 403 只有一个现实成因：`Sec-MS-GEC` 里的时间戳和服务器对不上。
                // 重算一次能救的只有"刚好卡在 5 分钟窗口边界上"这一种情况；
                // 时钟真的偏移了，重试也会照样失败，那就得说人话。
                if !is_forbidden(&err) {
                    return Err(err);
                }
                tracing::warn!("Edge 语音返回 403，重算时间戳令牌后重试一次");
                match self.synth_once(text, voice).await {
                    Ok(mp3) => Ok(mp3),
                    Err(_) => bail!(
                        "Edge 语音连续两次返回 403。这几乎总是**本机系统时钟偏了**\
                         （令牌里掺了时间，服务器允许的偏差只有几分钟）。\
                         在 WSL2 上宿主机休眠唤醒后时钟会停住，很容易发生。\
                         跑 `sudo hwclock -s` 或 `wsl --shutdown` 重启 WSL 校准后再试。"
                    ),
                }
            }
        }
    }
}

/// 503 之后隔多久重试。
const RETRY_AFTER_503: std::time::Duration = std::time::Duration::from_millis(700);

/// 判断这个错误是不是握手阶段的 503（服务端忙）。
fn is_unavailable(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_tungstenite::tungstenite::Error>()
            .is_some_and(|e| {
                matches!(
                    e,
                    tokio_tungstenite::tungstenite::Error::Http(resp)
                        if resp.status() == 503
                )
            })
    })
}

/// 判断这个错误是不是握手阶段的 403。
fn is_forbidden(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_tungstenite::tungstenite::Error>()
            .is_some_and(|e| {
                matches!(
                    e,
                    tokio_tungstenite::tungstenite::Error::Http(resp)
                        if resp.status() == 403
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 第 n 段故意拖得越久，用来验证「先完成的不会插到前面去」。
    struct SlowTts;

    #[async_trait]
    impl Tts for SlowTts {
        async fn say(&self, text: &str, _voice: &str) -> Result<Vec<u8>> {
            let n: u64 = text.parse().unwrap_or(0);
            tokio::time::sleep(std::time::Duration::from_millis(40 - n * 10)).await;
            Ok(vec![n as u8])
        }
    }

    #[tokio::test]
    async fn segments_are_joined_in_order_even_when_they_finish_out_of_order() {
        // 合成是并发的，第 1 段比第 3 段先回来是常态。要是用了
        // `buffer_unordered`，拼出来的音频顺序就跟着完成先后乱掉——
        // 「第 3 题」配着第 3 题的选项，念出来是另一道题。
        // 句子层面听不出错，但中韩分段会直接错位。
        let segs: Vec<Segment> = ["1", "2", "3"].iter().map(|s| Segment::zh(*s)).collect();
        let out = SlowTts.say_all(&segs, &Voices::default()).await.unwrap();

        assert_eq!(out, vec![1, 2, 3], "拼接顺序必须按输入，不按完成先后");
    }

    #[tokio::test]
    async fn empty_segments_are_skipped_without_calling_the_service() {
        let segs = vec![Segment::zh("1"), Segment::zh("   "), Segment::zh("2")];
        let out = SlowTts.say_all(&segs, &Voices::default()).await.unwrap();
        assert_eq!(out, vec![1, 2], "空白段不该占一次合成，也不该留个空档");
    }

    #[test]
    fn gec_token_is_stable_within_the_same_five_minute_window() {
        // 同一个 5 分钟窗口里的两个时刻，算出来的令牌必须一样——
        // 否则说明取整没做对，每个请求都会是新的，白白浪费连接。
        let base = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let a = sec_ms_gec(base);
        let b = sec_ms_gec(base + std::time::Duration::from_secs(60));
        assert_eq!(a, b, "窗口内应该稳定");

        // 而跨窗口必须变。这个断言守着"有效期只有 5 分钟"这个前提：
        // 如果哪天它不成立，说明取整写错了，缓存 token 就会变成"看起来能行"
        // 的假优化，然后在窗口翻页时集体 403。
        let c = sec_ms_gec(base + std::time::Duration::from_secs(400));
        assert_ne!(a, c, "跨窗口必须换令牌");
    }

    #[test]
    fn gec_token_follows_the_clock() {
        // 令牌确实掺了时间：偏移一小时就该完全不同。这条和上一条一起，
        // 说明"403 = 时钟偏了"这个诊断是有依据的，不是猜的。
        let base = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_ne!(
            sec_ms_gec(base),
            sec_ms_gec(base + std::time::Duration::from_secs(3600))
        );
    }

    #[test]
    fn gec_token_is_uppercase_hex_sha256() {
        let token = sec_ms_gec(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000));
        assert_eq!(token.len(), 64, "SHA-256 的十六进制是 64 位");
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
            "必须是大写十六进制，小写会被拒: {token}"
        );
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 手写的日历换算，拿几个已知的日子钉住。闰年那天最容易写错。
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29), "2024 是闰年");
        assert_eq!(civil_from_days(-1), (1969, 12, 31), "1970 以前也要对");
    }

    #[test]
    fn timestamp_is_the_shape_edge_expects() {
        // 2026-10-01 12:00:00 UTC
        let t = timestamp(UNIX_EPOCH + std::time::Duration::from_secs(1_790_856_000));
        assert_eq!(t, "Thu Oct 01 2026 12:00:00 GMT+0000 (Coordinated Universal Time)");
    }

    #[test]
    fn xml_is_escaped() {
        // 弹幕互动那条路会把观众昵称送进来，不转义就是让人往我们请求里塞标签。
        assert_eq!(escape_xml("a & b"), "a &amp; b");
        assert_eq!(escape_xml("<script>"), "&lt;script&gt;");
        assert_eq!(escape_xml("他说\"你好\""), "他说&quot;你好&quot;");
    }

    #[test]
    fn estimate_ms_is_roughly_seconds() {
        // 6000 字节/秒。只用来做兜底超时，所以宽松地钉一下量级。
        assert_eq!(estimate_ms(&vec![0u8; 6_000]), 1_000);
        assert_eq!(estimate_ms(&vec![0u8; 18_000]), 3_000);
    }

    /// 真的去打微软的服务器。默认跳过——单元测试不该依赖外网。
    ///
    /// `cargo test -p livestream-ai -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "要联网，手动跑"]
    async fn real_edge_tts_returns_mp3() {
        let tts = EdgeTts::new().unwrap();
        let mp3 = tts.say("안녕하세요", "ko-KR-SunHiNeural").await.unwrap();
        println!("拿到 {} 字节，约 {} ms", mp3.len(), estimate_ms(&mp3));
        assert!(mp3.len() > 1_000, "太短了，不像真的音频");
        // MP3 要么有 ID3 头，要么直接是帧同步字 0xFF 0xEx/0xFx。
        let looks_like_mp3 = mp3.starts_with(b"ID3") || (mp3[0] == 0xFF && (mp3[1] & 0xE0) == 0xE0);
        assert!(looks_like_mp3, "开头不像 MP3: {:02X?}", &mp3[..mp3.len().min(8)]);
    }
}
