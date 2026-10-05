//! DeepSeek 客户端：出题、讲解、点评都从这里走。
//!
//! 接口是 OpenAI 兼容的，所以只有 `/chat/completions` 一个端点。
//! 官方**没有语音接口**——合成语音是 [`crate::tts`] 的事，两件事不要混。
//!
//! 关于密钥：值只从环境变量 `DEEPSEEK_API_KEY` 读（由 `dotenvy` 从 `.env`
//! 载入），**任何日志、错误信息、`Debug` 输出里都不出现**。
//! [`DeepSeek`] 手写了 `Debug` 就是为了这个：万一哪天有人 `{:?}` 打了一下
//! 配置，也不会把密钥打到日志里。直播是长时间无人值守跑着的，
//! 日志被翻出来或者被投屏的概率都不低。

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// 默认模型。`deepseek-flash` 便宜快，出题这种任务够用；
/// 想换 `deepseek-v4-pro` 就设 `DEEPSEEK_MODEL`。
const DEFAULT_MODEL: &str = "deepseek-flash";
const DEFAULT_BASE: &str = "https://api.deepseek.com";

/// 单次请求超时。出题/讲解都在收答期里提前做，这个时间是够的；
/// 真超了就该走降级台词，而不是让直播等。
const TIMEOUT: Duration = Duration::from_secs(20);

/// 一次 `chat` 最多打几次网络。2 = 原始那次 + 一次重试。
///
/// 为什么要有重试：这是一场**无人值守**的直播，连续三次不走运就等于
/// 两分钟降级。而实测里这个接口绝大多数时候是 0.6～1.7 秒、稳稳的 200，
/// 偶尔抽一下恰恰是重试能吃掉的东西。
///
/// 为什么只重试一次：每一次重试都要占收答期的时间预算。收答期就 30 秒，
/// 重试链拉长了，讲解还没生成出来就该公布了。
const ATTEMPTS: u32 = 2;

/// 重试前等多久。不搞指数退避——只重试一次，等 400 毫秒躲开瞬时抖动
/// 就够了，退避是给「一直重试到成功」那类场景用的。
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

/// 连续失败到这个次数就熔断，之后一段时间直接走降级，不再打网络。
const BREAK_AFTER: u32 = 3;

/// 熔断后隔多久再试一次。
const BREAK_FOR: Duration = Duration::from_secs(120);

/// 客户端。
pub struct DeepSeek {
    http: reqwest::Client,
    /// 密钥。**不要**把它放进任何日志、错误或 Debug 输出里。
    key: String,
    model: String,
    base: String,
    /// 熔断状态：(连续失败次数, 熔断到什么时候)。
    health: Mutex<(u32, Option<Instant>)>,
}

/// 手写 Debug，把密钥换成固定占位符。
///
/// `#[derive(Debug)]` 会把 `key` 原样打出来——这个结构体迟早会有人
/// 顺手 `{:?}` 一下（比如塞进某个 `#[derive(Debug)]` 的上层配置里），
/// 那就是密钥泄漏。这里让它即使被打印也是安全的。
impl fmt::Debug for DeepSeek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeepSeek")
            .field("model", &self.model)
            .field("base", &self.base)
            .field("key", &"<已设置，不显示>")
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    /// 出题要结构化输出，靠这个让模型只吐 JSON。
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    temperature: f32,
    /// 显式关掉流式：我们要的是一整块 JSON，不是逐字吐。
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

/// 一次尝试的结果。
///
/// **只分「要不要再来一次」两种**，具体原因写在字符串里进日志。多分几类
/// 看着精确，其实没用——重试策略只认这一件事，而分类越细，越容易在某次
/// 改动里悄悄把一个「该重试」挪进「不该重试」，然后没人发现。
enum Attempt {
    /// 拿到正文了。
    Ok(String),
    /// 值得再来一次：网络层就没出去（连不上、DNS、TLS）、429 和 5xx
    /// （服务端说「等会儿」）、响应体坏了。共同的性子是「这一次不走运」，
    /// 而不是「这个请求本身不行」。
    Again(String),
    /// 再来一次也是一样的结果，别浪费时间：
    /// - 401/403/400 这类 4xx，原样重发只会拿到同样的答复；
    /// - **超时**。归到这一类是故意的：一次 20 秒，重试就是 40 秒起步，
    ///   而这 40 秒里直播是停着的。真超时说明这条链路现在不行，
    ///   该做的是走降级台词，不是接着等。
    Fatal(String),
}

/// 这个状态码值不值得重试。
///
/// 单独抽出来是为了能单测：这条判断写错，要么在 401 上空转 400 毫秒，
/// 要么在 429 上白白放弃一次本来能成的机会。两种都不会报错，只会让
/// 直播悄悄变差。
fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

impl DeepSeek {
    /// 从环境变量建。**没有密钥时返回 `Ok(None)`**，不是错误。
    ///
    /// 这是刻意的：没有 key 也必须能起服务、必须能跑完整场直播（走本地
    /// 模板台词）。开发和调试不该被一个密钥卡住，开播时忘记设 key 也不该
    /// 让整个直播间黑屏。
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(key) = std::env::var("DEEPSEEK_API_KEY") else {
            return Ok(None);
        };
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Ok(None);
        }

        let base = std::env::var("DEEPSEEK_BASE").unwrap_or_else(|_| DEFAULT_BASE.to_owned());
        let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());

        let mut builder = reqwest::Client::builder().timeout(TIMEOUT);

        // **默认直连，不走代理。**
        //
        // reqwest 会自动读 `HTTP_PROXY` / `HTTPS_PROXY` 环境变量。这两个变量
        // 一旦被设上（开发机的 Clash、容器里的透明代理都会设），每一次
        // DeepSeek 调用就悄悄从那个代理绕一圈——一个没人打算让它参与直播、
        // 也没人盯着的单点。出故障时最难查的正是这种：代码里看不见、
        // 日志里没有、`curl` 直连又是通的，只能靠猜。
        //
        // 要挂代理就明说：设 `DEEPSEEK_PROXY`，启动时日志里会写出来。
        match std::env::var("DEEPSEEK_PROXY") {
            Ok(url) if !url.trim().is_empty() => {
                let url = url.trim().to_owned();
                let proxy = reqwest::Proxy::all(&url)
                    .with_context(|| format!("DEEPSEEK_PROXY 不是合法的代理地址：{url}"))?;
                builder = builder.proxy(proxy);
                tracing::info!("DeepSeek 走代理 {url}");
            }
            _ => {
                builder = builder.no_proxy();
                tracing::info!("DeepSeek 直连（忽略 HTTP_PROXY / HTTPS_PROXY，要挂代理请设 DEEPSEEK_PROXY）");
            }
        }

        let http = builder.build().context("建 HTTP client 失败")?;

        Ok(Some(Self {
            http,
            key,
            model,
            base,
            health: Mutex::new((0, None)),
        }))
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// 现在是不是处于熔断状态。
    fn is_open(&self) -> bool {
        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
        match health.1 {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                // 熔断时间过了，放行一次探探路。
                health.1 = None;
                health.0 = 0;
                false
            }
            None => false,
        }
    }

    fn note_success(&self) {
        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
        *health = (0, None);
    }

    fn note_failure(&self) {
        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
        health.0 += 1;
        if health.0 >= BREAK_AFTER {
            health.1 = Some(Instant::now() + BREAK_FOR);
            tracing::warn!(
                "DeepSeek 连续失败 {} 次，熔断 {} 秒，期间用本地台词顶上",
                health.0,
                BREAK_FOR.as_secs()
            );
        }
    }

    /// 发一次对话请求，返回正文。
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        self.chat_inner(system, user, false).await
    }

    /// 要一段 JSON。用 `response_format: json_object` 约束模型只吐 JSON。
    ///
    /// 注意：DeepSeek 只支持 `json_object`，**不支持 `json_schema`**，
    /// 所以字段结构不会被服务端强制。结构正确性必须我们自己校验
    /// （见 `script.rs` 里的解析和上层 `bank` 的校验），不能指望模型。
    pub async fn chat_json(&self, system: &str, user: &str) -> Result<String> {
        self.chat_inner(system, user, true).await
    }

    async fn chat_inner(&self, system: &str, user: &str, json: bool) -> Result<String> {
        if self.is_open() {
            bail!("DeepSeek 处于熔断状态，先用本地台词");
        }

        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                Message { role: "system", content: system },
                Message { role: "user", content: user },
            ],
            response_format: json.then_some(ResponseFormat { kind: "json_object" }),
            // 出题和讲解都要稳定、可复现，不要它发挥。
            temperature: if json { 1.0 } else { 0.8 },
            stream: false,
        };

        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tracing::debug!("DeepSeek 第 {attempt} 次重试：{last}");
                tokio::time::sleep(RETRY_BACKOFF).await;
            }

            match self.send_once(&body).await {
                Attempt::Ok(text) => {
                    self.note_success();
                    return Ok(text);
                }
                Attempt::Again(why) => last = why,
                Attempt::Fatal(why) => {
                    self.note_failure();
                    bail!("{why}");
                }
            }
        }

        // 打满了次数还是没成。**只记一次失败**——熔断器数的是「几次请求
        // 没成」，不是「几次网络调用没成」。按后者数的话，三次抖动的请求
        // 会记成六次，熔断提前一半触发；而抖动恰恰是这个重试要吃掉的东西，
        // 结果反而更容易熔断，等于白做。
        self.note_failure();
        bail!("DeepSeek 连续 {ATTEMPTS} 次都没成：{last}");
    }

    /// 打一次网络。
    ///
    /// 把「打一次」和「打几次」分开，是为了让重试那层只看得懂
    /// [`Attempt::Again`] 和 [`Attempt::Fatal`]，不必知道 HTTP 细节。
    async fn send_once(&self, body: &ChatRequest<'_>) -> Attempt {
        let resp = match self
            .http
            .post(format!("{}/chat/completions", self.base))
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) if err.is_timeout() => {
                return Attempt::Fatal(format!("DeepSeek 请求超过 {} 秒没回", TIMEOUT.as_secs()));
            }
            // `{err:#}` 是 anyhow 的「带上整条 error source 链」格式，
            // 比 `{}` 只打最外面一层有用得多。reqwest 不会把
            // Authorization 头写进错误里，密钥不会从这里漏出去。
            Err(err) => return Attempt::Again(format!("请求 DeepSeek 失败：{err:#}")),
        };

        let status = resp.status();
        if !status.is_success() {
            // 只取状态码和一小段正文。**不要**把响应体整个打出来——
            // 虽然正常不会含密钥，但错误回显里什么都可能有。
            let snippet = resp.text().await.unwrap_or_default();
            let snippet: String = snippet.chars().take(200).collect();
            let msg = format!("DeepSeek 返回 {status}：{snippet}");
            return if retryable_status(status) {
                Attempt::Again(msg)
            } else {
                Attempt::Fatal(msg)
            };
        }

        let parsed: ChatResponse = match resp.json().await {
            Ok(p) => p,
            // 截断的响应体也会走到这里，所以值得再来一次。
            Err(err) => return Attempt::Again(format!("DeepSeek 的响应不是预期结构：{err:#}")),
        };

        match parsed.choices.into_iter().next().and_then(|c| c.message.content) {
            Some(text) if !text.trim().is_empty() => Attempt::Ok(text),
            // 空正文和「没有 choices」是同一件事的两张脸：模型这次什么都没
            // 说出来。重试一次有可能拿到东西——但如果重试也是空的，那就
            // 会被记成一次失败，这是对的。
            _ => Attempt::Again("DeepSeek 返回了空的正文".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_key() {
        // 回归测试。这个结构体迟早会被人顺手 `{:?}` 一下，
        // 一旦打出来就是密钥泄漏——直播日志会被翻、会被投屏。
        let ds = DeepSeek {
            http: reqwest::Client::new(),
            key: "sk-绝密-不应该出现".to_owned(),
            model: "deepseek-flash".to_owned(),
            base: DEFAULT_BASE.to_owned(),
            health: Mutex::new((0, None)),
        };
        let printed = format!("{ds:?}");
        assert!(!printed.contains("sk-绝密"), "密钥被 Debug 打出来了: {printed}");
        assert!(printed.contains("不显示"), "应该明确标出密钥已设置");
    }

    #[test]
    fn circuit_opens_after_repeated_failures() {
        let ds = DeepSeek {
            http: reqwest::Client::new(),
            key: "x".to_owned(),
            model: "m".to_owned(),
            base: DEFAULT_BASE.to_owned(),
            health: Mutex::new((0, None)),
        };
        assert!(!ds.is_open(), "一开始应该是通的");

        for _ in 0..BREAK_AFTER {
            ds.note_failure();
        }
        assert!(ds.is_open(), "连续失败后应该熔断，不再打网络");

        // 成功一次就该恢复。
        ds.note_success();
        assert!(!ds.is_open(), "成功后应该恢复");
    }

    #[test]
    fn retries_when_the_server_says_come_back_later() {
        use reqwest::StatusCode;
        // 「现在忙」——等一下再来是有可能成的。
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS), "429 应该重试");
        assert!(retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(retryable_status(StatusCode::BAD_GATEWAY));
        assert!(retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(retryable_status(StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn does_not_retry_requests_that_will_fail_again() {
        use reqwest::StatusCode;
        // 密钥不对、没权限、请求写错了：原样重发只会拿到同样的答复，
        // 白等 400 毫秒，还占掉收答期的时间预算。
        assert!(!retryable_status(StatusCode::UNAUTHORIZED), "401 重试没有意义");
        assert!(!retryable_status(StatusCode::FORBIDDEN));
        assert!(!retryable_status(StatusCode::BAD_REQUEST));
        assert!(!retryable_status(StatusCode::NOT_FOUND));
        // 成功更不该重试——这条要是写反了，每次调用都会打两遍，
        // 账单翻倍而且直播照常，谁都不会发现。
        assert!(!retryable_status(StatusCode::OK));
    }
}
