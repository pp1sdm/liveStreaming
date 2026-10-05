//! AI 接管直播所需要的两样东西：**会说话**和**会写字**。
//!
//! - [`tts`]：把台词合成成 MP3。服务端合成，页面只负责播——原因见模块注释。
//! - [`deepseek`]：出题和讲解，走 DeepSeek 的 OpenAI 兼容接口。
//!
//! 这一层刻意不认识「题目」「回合」「直播」这些概念，只做「文本进、音频出」
//! 和「提示词进、文本出」。编排逻辑在 `livestream-app` 的 director 里，
//! 这样这层可以被独立地测，而 `livestream-web` 也不必依赖网络客户端。

pub mod deepseek;
pub mod script;
pub mod tts;

pub use deepseek::DeepSeek;
pub use script::{Line, LineKind};
pub use tts::{EdgeTts, Lang, Segment, Tts, Voices};
