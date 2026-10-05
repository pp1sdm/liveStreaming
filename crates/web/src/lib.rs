//! HTTP 层：给 OBS 的画面页、给主播的控制台，以及它们共用的推送通道。
//!
//! 推送用 [`tokio::sync::watch`] 而不是广播——画面要的永远是「现在是什么样」，
//! 中间漏掉几帧无所谓，重连之后拿到的也必须是最新状态。watch 正好是这个语义。

mod audio;
mod session;

pub use audio::{AudioId, AudioStore};
pub use session::{
    BOARD_LEN, OverlayState, QuestionView, Session, SpeechView, Step, TickerItem,
};

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use livestream_core::{Danmaku, Question};
use livestream_engine::{Outcome, Phase, Rejection};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::WatchStream;

const OVERLAY_HTML: &str = include_str!("../assets/overlay.html");
const CONSOLE_HTML: &str = include_str!("../assets/console.html");

/// 服务端状态。克隆起来很便宜，axum 的 handler 随便拿。
#[derive(Clone)]
pub struct App {
    session: Arc<Mutex<Session>>,
    state: watch::Sender<OverlayState>,
    /// 合成好的音频。**不进 `OverlayState`**，页面凭 id 单独来取。
    audio: Arc<AudioStore>,
    /// 页面回报「这句念完了」。编排层收它来推进节奏。
    spoken: mpsc::UnboundedSender<AudioId>,
}

impl App {
    /// 建一个 `App`，同时给出「页面回报语音念完了」的接收端。
    ///
    /// 接收端交回给调用方（编排层），而不是让 `App` 自己消费——因为
    /// 「念完了该怎么办」是编排逻辑，不该塞进 HTTP 层。
    pub fn new(session: Session) -> (Self, mpsc::UnboundedReceiver<AudioId>) {
        let initial = session.snapshot();
        let (state, _rx) = watch::channel(initial);
        let (spoken, spoken_rx) = mpsc::unbounded_channel();
        (
            Self {
                session: Arc::new(Mutex::new(session)),
                state,
                audio: Arc::new(AudioStore::default()),
                spoken,
            },
            spoken_rx,
        )
    }

    /// 存一段合成好的音频，返回页面取件用的 id。
    pub fn publish_audio(&self, bytes: Vec<u8>) -> AudioId {
        self.audio.put(bytes)
    }

    /// 页面回报「这句念完了」。
    ///
    /// 和 `POST /api/spoken` 走的是同一条路——测试直接调它，
    /// 就能不用起 HTTP 服务地把「念完了 → 下一步」这段节奏跑通。
    pub fn report_spoken(&self, id: AudioId) {
        let _ = self.spoken.send(id);
    }

    /// 换一句要念的话，并推给画面。传 `None` 让页面安静下来。
    pub fn set_speech(&self, speech: Option<SpeechView>) {
        self.lock().set_speech(speech);
        self.publish();
    }

    /// 换到下一题，但先不计时（等旁白念完）。
    pub fn prepare_next(&self) {
        self.lock().prepare_next(Instant::now());
        self.publish();
    }

    /// 旁白念完了，开始计时。返回是否真的开了表。
    pub fn begin(&self) -> bool {
        let started = self.lock().begin(Instant::now());
        if started {
            self.publish();
        }
        started
    }

    pub fn phase(&self) -> Phase {
        self.lock().phase()
    }

    /// 当前这道题。出题/讲解要拿它。
    pub fn question(&self) -> Question {
        self.lock().round_question()
    }

    /// 离封盘还有多久。`Presenting` 期间恒等于整个窗口。
    pub fn remaining(&self) -> Duration {
        self.lock().remaining()
    }

    /// 现在该念的那句话。
    pub fn speech(&self) -> Option<SpeechView> {
        self.lock().speech().cloned()
    }

    /// 本题作答人数。编排层用它决定要不要说点什么。
    pub fn participants(&self) -> u32 {
        self.lock().participants()
    }

    /// 本题的票数分布。**只在公布后才有值**——之前是 `None`，
    /// 这是防跟风的硬约束，不是「调用方注意一下」。
    pub fn stats(&self) -> Option<livestream_engine::Stats> {
        self.lock().stats()
    }

    /// 开场让第一题也先报题再计时。
    pub fn present_current(&self) {
        self.lock().present_current(Instant::now());
        self.publish();
    }

    /// 往里加一道 AI 现出的题。
    pub fn push_question(&self, question: Question) {
        self.lock().push_question(question);
    }

    /// 题库是不是快见底了。
    pub fn bank_running_low(&self, low_water: usize) -> bool {
        self.lock().bank_running_low(low_water)
    }

    /// 现在播到第几题（从 0 数）。
    pub fn question_index(&self) -> usize {
        self.lock().question_index()
    }

    /// 题库里一共几道题。
    pub fn bank_len(&self) -> usize {
        self.lock().bank_len()
    }

    /// 收一条弹幕。只有在真的计入作答时才推送给画面——
    /// 噪声弹幕每秒几十条，每条都推会把画面刷爆。
    pub fn handle_danmaku(&self, d: &Danmaku) -> Outcome {
        let outcome = self.lock().handle(d, Instant::now());
        if matches!(outcome, Outcome::Counted(_)) {
            self.publish();
        }
        outcome
    }

    /// 走一步：收答 → 封盘 → 公布 → 下一题。
    ///
    /// 返回做了什么，以及走完之后的状态。控制台直接拿这份去重画，不用再等
    /// SSE 绕一圈回来——那一圈的时间里按钮还是活的，连点两下就会连走两步
    /// （实测过：两次「下一题」直接跳过一整道题）。
    pub fn step(&self) -> (Step, OverlayState) {
        // 动作和快照必须在同一个锁里取，否则中间夹进来一条弹幕，
        // 回给控制台的就是旧数据。
        let (step, snapshot) = {
            let mut session = self.lock();
            let step = session.step(Instant::now());
            (step, session.snapshot())
        };
        self.state.send_replace(snapshot.clone());
        (step, snapshot)
    }

    /// 定时调用：到点自动封盘。返回是否发生了封盘。
    pub fn tick(&self) -> bool {
        let locked = self.lock().auto_lock_if_expired(Instant::now());
        if locked {
            tracing::info!("收答时间到，自动封盘");
            self.publish();
        }
        locked
    }

    /// 把当前状态推给所有连着的画面。
    ///
    /// 用 [`watch::Sender::send_replace`] 而不是 `send`。**`send` 在没有订阅者
    /// 的时候直接返回 `Err`，而且不写共享值**——原来这里是 `let _ = send(..)`，
    /// 于是"没人看的时候"发生的每一次状态变化都被丢掉了。OBS 还没开、或者
    /// 画面正好断线重连的那几十秒里，主播在控制台一路点到"公布"，通道里存的
    /// 还是启动时那份 `collecting`；浏览器一连上就先收到这份旧的（`WatchStream`
    /// 首帧给的就是共享值），画面停在收答中，答案其实早公布了，音效还会在
    /// 下一次真推送到达时莫名其妙补一声。
    ///
    /// 这里要的语义是"**最后一个状态永远是对的**，谁连上谁就拿到它"，
    /// `send_replace` 正好是：无条件写入，有订阅者就顺带叫醒他们。
    pub fn publish(&self) {
        let snapshot = self.lock().snapshot();
        self.state.send_replace(snapshot);
    }

    /// 拿会话锁。
    ///
    /// **中毒了也照常往下走，不跟着 panic。** 锁中毒只说明之前某次持锁期间
    /// panic 过，不代表 [`Session`] 的数据坏了——出事的那些操作要么已经改完，
    /// 要么根本没开始改。原来这里是 `.expect()`，于是任何一次 panic 都会把
    /// 整个服务变成永久黑屏：之后每个请求都在这一行再 panic 一次，永远缓不
    /// 过来。直播间正在播，恢复比维持中毒状态重要得多。
    fn lock(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|poisoned| {
            tracing::error!("会话锁之前被一次 panic 弄中毒了，按当前状态继续");
            poisoned.into_inner()
        })
    }
}

/// 组装路由。
pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::permanent("/overlay") }))
        .route("/overlay", get(overlay))
        .route("/console", get(console))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/rejections", get(api_rejections))
        .route("/api/leaderboard", get(api_leaderboard))
        .route("/api/continue", post(api_continue))
        .route("/voice/{id}", get(voice))
        .route("/api/spoken", post(api_spoken))
        .with_state(app)
}

/// 取一段合成好的语音。
///
/// 找不到就 404，页面安静跳过——音频可能因为容量淘汰已经没了，
/// 那不是错误，不需要页面报错刷屏。
async fn voice(State(app): State<App>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(bytes) = app.audio.get(&AudioId::new(id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // 同样不许缓存：OBS 的 CEF 会把音频缓存住，改一句词重启服务，
    // 那边放的还是旧的。
    (
        [
            (header::CONTENT_TYPE, "audio/mpeg"),
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
        ],
        bytes.to_vec(),
    )
        .into_response()
}

/// 页面回报「这句念完了」。
///
/// 编排层靠它推进节奏：讲解念完了才换下一题。页面挂了或回报丢了，
/// 编排层那边有兜底超时，所以这里的失败不必让页面重试。
#[derive(Debug, Deserialize)]
struct SpokenReport {
    id: String,
}

async fn api_spoken(State(app): State<App>, Json(report): Json<SpokenReport>) -> impl IntoResponse {
    app.report_spoken(AudioId::new(report.id));
    Json(json!({ "ok": true }))
}

/// 页面一律不许缓存。
///
/// OBS 的浏览器源是 CEF，缓存很激进：不加这个头，改完页面重新编译、甚至重启服务，
/// OBS 那边还在放几十轮之前的旧页面。实测踩过一次——OBS 里显示的是上一版配色的
/// 绿块，浏览器里是新的青白，两边对不上，看着像样式写错了，其实是缓存在作怪。
/// 页面本身只有二十几 KB，每次开播拉一次，不值得为省这点流量留缓存。
const NO_STORE: [(axum::http::HeaderName, &str); 1] = [(
    axum::http::header::CACHE_CONTROL,
    "no-store, no-cache, must-revalidate",
)];

async fn overlay() -> impl IntoResponse {
    (NO_STORE, Html(OVERLAY_HTML))
}

async fn console() -> impl IntoResponse {
    (NO_STORE, Html(CONSOLE_HTML))
}

async fn api_state(State(app): State<App>) -> impl IntoResponse {
    // 快照在锁里取完就放锁，序列化在锁外做。
    let snapshot = app.lock().snapshot();
    Json(snapshot)
}

/// 画面订阅这里。连上就先收到一份当前状态，之后每次变化再收一份。
async fn api_events(
    State(app): State<App>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let rx = app.state.subscribe();
    let stream = WatchStream::new(rx).map(|state| {
        let data = serde_json::to_string(&state).unwrap_or_else(|_| "{}".to_owned());
        Ok(Event::default().event("state").data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// 被判定层丢掉的弹幕。主播用它判断判定规则是不是太严或太松。
async fn api_rejections(State(app): State<App>) -> impl IntoResponse {
    // 先把要发的数据拷出来、放掉锁，再拼 JSON。
    //
    // 拼 JSON 是这里唯一可能 panic 的地方（判定原因曾经有个变体序列化不了），
    // 而 panic 一旦发生在持锁期间就会把 Mutex 弄脏，之后每个请求都跟着完蛋。
    // 拷贝 200 条字符串的开销，换的是「序列化出问题也炸不到会话状态」。
    let rows: Vec<(String, String, Rejection)> = {
        let session = app.lock();
        session.rejections().iter().rev().cloned().collect()
    };

    let items: Vec<_> = rows
        .into_iter()
        .map(|(nickname, content, reason)| {
            json!({
                "nickname": nickname,
                "content": content,
                // 界面直接显示这句。
                "reason": reason.describe(),
                // 结构化的那份留着，以后要做统计不用再改接口。
                "detail": reason,
            })
        })
        .collect();
    Json(items)
}

/// 控制台看的完整榜。画面那块地方只够摆 [`BOARD_LEN`] 名，这里可以看全。
async fn api_leaderboard(State(app): State<App>) -> impl IntoResponse {
    /// 控制台最多列这么多名，再多就得翻页了。
    const CONSOLE_ROWS: usize = 50;

    // 同样：锁里只取数据，拼 JSON 放到锁外面。
    let (rows, ranked, players) = {
        let session = app.lock();
        (
            session.top_players(CONSOLE_ROWS),
            session.ranked_count(),
            session.player_count(),
        )
    };
    Json(json!({ "rows": rows, "ranked": ranked, "players": players }))
}

/// 主播的「继续」。按当前阶段做该做的事，并把走完的状态一并回过去。
async fn api_continue(State(app): State<App>) -> impl IntoResponse {
    let (did, state) = app.step();
    Json(json!({ "ok": true, "did": did, "state": state }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use livestream_core::{Choice, Question};
    use std::time::Duration;

    fn bank() -> Vec<Question> {
        vec![Question {
            id: 1,
            prompt: "Q1".into(),
            options: ["가".into(), "나".into(), "다".into(), "라".into()],
            correct: Choice::A,
        }]
    }

    fn app() -> App {
        let session = Session::new(bank(), Duration::from_secs(60), Instant::now()).unwrap();
        // 测试不关心"念完了"的回报，接收端直接丢掉。
        App::new(session).0
    }

    #[test]
    fn state_stays_current_while_nobody_is_watching() {
        // 回归测试。`watch::Sender::send` 在**没有订阅者**时返回 Err 并且
        // 不写共享值，而原来是 `let _ = send(..)`——没人看的时候状态全丢。
        // 实际表现：OBS 还没开的那段时间，主播在控制台一路点到「公布」，
        // 浏览器一连上收到的却是启动时那份「收答中」，画面和答案对不上，
        // 音效还会在下一帧突然补一声。
        let app = app();

        // 一个订阅者都没有的时候走两步：收答 → 封盘 → 公布。
        assert_eq!(app.step().0, Step::Locked);
        assert_eq!(app.step().0, Step::Revealed);

        // 现在才连上来看。WatchStream 首帧给的就是共享值，所以直接读它。
        let rx = app.state.subscribe();
        assert_eq!(
            rx.borrow().phase,
            livestream_engine::Phase::Revealed,
            "没有订阅者时发生的状态变化也必须留在通道里"
        );
    }

    #[test]
    fn a_poisoned_session_lock_does_not_brick_the_app() {
        // 回归测试。以前 lock() 是 `.expect("session mutex poisoned")`，
        // 于是任何一次持锁期间的 panic 都会让之后**每一个**请求都在同一行
        // 再 panic 一次——服务再也缓不过来，画面永久黑屏。实测过一次：
        // /api/rejections 拼 JSON 时炸掉，整个直播间就死了。
        let app = app();

        // 模拟一次「持锁期间 panic」。
        let session = Arc::clone(&app.session);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = session.lock().unwrap();
            panic!("模拟持锁期间的 panic");
        }));

        assert!(app.session.is_poisoned(), "前提：这次 panic 确实把锁弄中毒了");

        // 中毒之后还得能用。
        let (step, state) = app.step();
        assert_eq!(step, Step::Locked, "中毒之后「继续」照走不误");
        assert_eq!(state.phase, livestream_engine::Phase::Locked);
    }
}
