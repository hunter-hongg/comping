//! `llm` —— 多协议模型适配层。
//!
//! 每个子模块只做一件事：把 `agent_core` 的中立表示翻译成某家协议，
//! 再把流式响应翻译回 `AgentEvent`。重试、SSE 切分、错误归一在 [`http`]。
//!
//! 要接一个新模型，两种办法：
//! - 换个 `base_url` 走 OpenAI 兼容协议（零代码）
//! - 新写一个实现 `Provider` 的模块

pub mod anthropic;
pub mod gemini;
pub mod http;
pub mod openai;
pub mod registry;

pub use registry::{ModelEntry, ProviderConfig, build, known_models};
