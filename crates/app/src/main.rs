//! 主程序：把弹幕源、引擎和 HTTP 层接起来。
//!
//! ```text
//!   DanmakuSource ──mpsc──► App::handle_danmaku ──► Session ──watch──► 画面
//!                                                              └──────► 控制台
//! ```

mod bank;
mod director;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use livestream_ai::{DeepSeek, EdgeTts, Tts, Voices};
use livestream_danmaku::{BarrageGrabSource, DanmakuSource, MockSource};
use livestream_web::{App, Session};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::director::Director;

/// 打不开语音合成时的替身：什么都不说，但流程照走。
///
/// 有它才不会因为一个网络问题就让整个直播间起不来——没有声音是可惜，
/// 但字幕和答题都还在，直播能继续。
struct MuteTts;

#[async_trait::async_trait]
impl Tts for MuteTts {
    async fn say(&self, _text: &str, _voice: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("这台机器没能连上语音合成服务")
    }
}

/// 只监听本机。画面和控制台都不需要从外面访问。
const BIND: &str = "127.0.0.1:7331";

/// 每题收答时长。
///
/// 从 60 秒收到 30 秒。一题一轮的总时长 = 收答 + 公布（十几秒语音）+ 串场
/// （几秒）+ 报题（七八秒），60 秒那版一轮要一百多秒，对短视频直播的节奏
/// 来说太拖了。30 秒仍然比「听清题干、想一下、打字」需要的时间宽裕得多，
/// 而且**计时是从旁白念完之后才开始的**（见 `Phase::Presenting`），
/// 念题那几秒不占观众的思考时间。
const WINDOW: Duration = Duration::from_secs(30);

/// 挑弹幕源。
///
/// 设了 `DANMAKU_WS` 就连本地转发工具的 WebSocket（比如 BarrageGrab 的
/// `ws://127.0.0.1:8888`），否则用自己造弹幕的演示源。
fn pick_source() -> Box<dyn DanmakuSource> {
    match std::env::var("DANMAKU_WS") {
        Ok(url) if !url.trim().is_empty() => {
            tracing::info!(%url, "连本地弹幕转发工具");
            Box::new(BarrageGrabSource::new(url))
        }
        _ => {
            tracing::warn!(
                "没有设置 DANMAKU_WS，跑的是演示弹幕源——屏幕上的人都是假的，\
                 收不到真实弹幕。开播前先起转发工具，再 DANMAKU_WS=ws://127.0.0.1:8888 启动。"
            );
            Box::new(MockSource::demo())
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // 从 `.env` 载入环境变量。**必须在任何 `std::env::var` 之前**。
    //
    // 密钥只存在 `.env` 里（已加进 .gitignore），不进代码、不进命令行参数
    // （参数会出现在 `ps` 里，同机器的任何进程都看得到）、不进日志。
    // 找不到 `.env` 不是错误：没有密钥也要能跑完整场直播。
    match dotenvy::dotenv() {
        Ok(path) => tracing::info!("已载入 {}", path.display()),
        Err(_) => tracing::info!("没有 .env，只用已有的环境变量"),
    }

    // 只说有没有、用哪个模型，**绝不打印密钥本身**。
    let ai = match DeepSeek::from_env()? {
        Some(ds) => {
            tracing::info!(model = ds.model(), "DeepSeek 就绪，AI 接管出题和讲解");
            Some(Arc::new(ds))
        }
        None => {
            tracing::warn!(
                "没有 DEEPSEEK_API_KEY，AI 出题/讲解不可用，改用题库循环和本地台词。\
                 要开 AI 就在 .env 里写 DEEPSEEK_API_KEY=sk-..."
            );
            None
        }
    };

    // 语音合成。**打不开也要能开播**——退化成静音，字幕和答题照常。
    let tts: Arc<dyn Tts> = match EdgeTts::new() {
        Ok(engine) => {
            tracing::info!("语音合成就绪");
            Arc::new(engine)
        }
        Err(err) => {
            tracing::error!("语音合成起不来（{err:#}），这一场没有声音，只有字幕");
            Arc::new(MuteTts)
        }
    };

    let session = Session::new(bank::demo_bank(), WINDOW, Instant::now())?;
    let (app, spoken_rx) = App::new(session);
    let mut director = Director::new(app.clone(), ai, tts, Voices::default(), spoken_rx);
    // 开场第一题也先报题再计时，跟后面每一题一致。
    director.start();

    // 导演被两个任务共用：一个按节拍推进，一个往里递弹幕。
    // `Director::tick` 是同步的（见它的注释），所以普通 Mutex 就够，
    // 不存在持锁跨 await 的问题。
    let director = Arc::new(Mutex::new(director));

    // 弹幕源 → 队列。队列给足余量，避免直播间刷起来的时候把源卡住。
    let (tx, mut rx) = mpsc::channel(1024);
    let source = pick_source();
    tracing::info!(source = source.name(), "弹幕源启动");

    tokio::spawn(async move {
        if let Err(err) = source.run(tx).await {
            tracing::error!("弹幕源退出：{err:#}");
        }
    });

    // 队列 → 引擎。
    //
    // 弹幕同时喂给导演一份**复印件**，用于控场互动——但它拿到的是原文，
    // 会不会被用由导演自己按 `sanitize_danmaku` 把关，见 `observe_danmaku`。
    let consumer = app.clone();
    let ears = Arc::clone(&director);
    tokio::spawn(async move {
        while let Some(danmaku) = rx.recv().await {
            consumer.handle_danmaku(&danmaku);
            // 顺手给导演一份，让它知道直播间在聊什么。收不收、怎么用，
            // 由导演自己按 `sanitize_danmaku` 把关。
            ears.lock().unwrap_or_else(|e| e.into_inner()).observe_danmaku(&danmaku.content);
        }
        tracing::warn!("弹幕队列已关闭");
    });

    // 导演每 250ms 走一步：报题、开表、封盘、公布、换题，
    // 顺带把该念的话排上。**这一条就是「到时间自动换题」。**
    //
    // 它自己调 `app.tick()` 做自动封盘，所以这里不再单起一个计时器。
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(250));
        loop {
            ticker.tick().await;
            director.lock().unwrap_or_else(|e| e.into_inner()).tick();
        }
    });

    let listener = TcpListener::bind(BIND).await?;
    tracing::info!("画面    http://{BIND}/overlay    （OBS 浏览器源用这个，源设成 1920×1080）");
    tracing::info!("        想叠在摄像头上、不要米黄底，就用 /overlay?solid=0");
    tracing::info!("        ?sfx=0 关音效，?vol=0.72 调音量（音效是页面现场合成的，没有音频文件）");
    tracing::info!("        声音要进直播：OBS 里右键那个浏览器源 → 高级音频属性 → 音频输出改成「输出到 OBS」");
    tracing::info!("控制台  http://{BIND}/console");

    axum::serve(listener, livestream_web::router(app)).await?;
    Ok(())
}
