//! Provider 注册表：把配置变成可用的 [`Provider`] 实例。

use std::sync::Arc;

use agent_core::{Capabilities, ModelInfo, Provider};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{anthropic, gemini, openai};

/// 单个 provider 的配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 协议名：`anthropic` / `openai` / `gemini`。
    #[serde(default)]
    pub kind: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    /// 自定义模型清单，配置了会覆盖内置表。
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// 部分兼容端点不返回 usage。
    pub supports_usage: Option<bool>,
}

/// 配置里的模型条目（比 `ModelInfo` 少几个必填项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default = "default_context")]
    pub context_window: u32,
    #[serde(default = "yes")]
    pub tools: bool,
    #[serde(default = "yes")]
    pub vision: bool,
    #[serde(default = "yes")]
    pub prompt_cache: bool,
}

fn default_context() -> u32 {
    128_000
}
fn yes() -> bool {
    true
}

impl From<&ModelEntry> for ModelInfo {
    fn from(e: &ModelEntry) -> Self {
        ModelInfo {
            display_name: e.display_name.clone().unwrap_or_else(|| e.id.clone()),
            id: e.id.clone(),
            capabilities: Capabilities {
                tools: e.tools,
                vision: e.vision,
                prompt_cache: e.prompt_cache,
                context_window: e.context_window,
            },
        }
    }
}

/// 按配置构造一个 provider。
pub fn build(name: &str, cfg: &ProviderConfig) -> Result<Arc<dyn Provider>> {
    let kind = cfg
        .kind
        .clone()
        .or_else(|| infer_kind(name))
        .unwrap_or_else(|| "openai".into());

    let custom: Vec<ModelInfo> = cfg.models.iter().map(ModelInfo::from).collect();

    let p: Arc<dyn Provider> = match kind.as_str() {
        "anthropic" => Arc::new(anthropic::AnthropicProvider::new(cfg)?.with_models(custom)),
        "openai" | "compatible" => Arc::new(openai::OpenAiProvider::new(cfg)?.with_models(custom)),
        "gemini" | "google" => Arc::new(gemini::GeminiProvider::new(cfg)?.with_models(custom)),
        other => bail!("未知的 provider 类型 `{other}`，可选 anthropic / openai / gemini"),
    };

    Ok(p)
}

/// 从名字猜协议，让 `[providers.deepseek]` 这类配置能少写一行。
fn infer_kind(name: &str) -> Option<String> {
    match name.to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("anthropic".into()),
        "openai" | "gpt" => Some("openai".into()),
        "gemini" | "google" => Some("gemini".into()),
        // 其余一律按 OpenAI 兼容处理：DeepSeek / vLLM / Ollama / OpenRouter 都吃这个协议
        _ => Some("openai".into()),
    }
}

/// 内置模型表。
pub fn known_models(kind: &str) -> Vec<ModelInfo> {
    match kind {
        "anthropic" => anthropic::default_models(),
        "gemini" => gemini::default_models(),
        _ => openai::default_models(),
    }
}
