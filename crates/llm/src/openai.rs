//! OpenAI Chat Completions 适配器。
//!
//! 覆盖面最大的一家：OpenAI / DeepSeek / vLLM / Ollama / OpenRouter
//! 都吃这个协议，`base_url` 一换就是新模型。
//!
//! 中立表示 → OpenAI 的映射要点：
//! - system 是 `messages` 里的第一条
//! - 工具调用是 `tool_calls`，参数是**字符串化 JSON**，需要反向解析
//! - 流式增量按 `index` 归并；结束靠 `finish_reason`

use agent_core::{
    Capabilities, ChatRequest, ChatResponse, ModelInfo, Provider, Role,
    StopReason, ToolCall, Usage,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::http;
use crate::registry::ProviderConfig;

pub const DEFAULT_BASE: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
    client: reqwest::Client,
    base: String,
    api_key: Option<String>,
    default_model: String,
    models: Vec<ModelInfo>,
    /// 兼容端点里有的不发 usage。
    supports_usage: bool,
}

impl OpenAiProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let api_key = cfg
            .api_key
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok());
        if api_key.is_none() && !cfg.base_url.is_some() {
            anyhow::bail!("缺少 OpenAI API key：设置 config 里的 api_key 或环境变量 OPENAI_API_KEY");
        }

        Ok(Self {
            client: http::build_client()?,
            default_model: cfg.model.clone().unwrap_or_else(|| "gpt-4.1".to_string()),
            models: crate::registry::known_models("openai"),
            base: base.trim_end_matches('/').to_string(),
            api_key,
            supports_usage: cfg.supports_usage.unwrap_or(true),
        })
    }

    pub fn with_models(mut self, models: Vec<ModelInfo>) -> Self {
        if !models.is_empty() {
            self.models = models;
        }
        self
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![("content-type".into(), "application/json".into())];
        // 本地端点（Ollama/vLLM）常常不校验 key，但仍要求这个头存在。
        if let Some(k) = &self.api_key {
            h.insert(0, ("authorization".into(), format!("Bearer {k}")));
        }
        h
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    async fn stream(
        &self,
        request: ChatRequest,
        model: &str,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<ChatResponse> {
        let body = encode_request(&request, model, self.supports_usage);
        let url = format!("{}/chat/completions", self.base);

        let resp = http::post_stream(&self.client, &url, &self.headers(), &body).await?;

        let mut state = StreamState::default();
        http::consume_sse(resp, |ev| {
            state.on_event(&ev, &tx);
            Ok(())
        })
        .await?;

        Ok(state.finish())
    }
}

/// 中立请求 → OpenAI body。
pub fn encode_request(req: &ChatRequest, model: &str, with_usage: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(sys) = &req.system {
        if !sys.is_empty() {
            messages.push(json!({ "role": "system", "content": sys }));
        }
    }

    for m in &req.messages {
        match m.role {
            Role::System => continue, // 已提到最前面
            Role::User => messages.push(json!({ "role": "user", "content": m.content })),
            Role::Assistant => {
                let mut msg = json!({ "role": "assistant", "content": m.content });
                if !m.tool_calls.is_empty() {
                    msg["tool_calls"] = Value::Array(
                        m.tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        // OpenAI 这里是字符串，不是对象
                                        "arguments": c.arguments.to_string(),
                                    }
                                })
                            })
                            .collect(),
                    );
                }
                messages.push(msg);
            }
            Role::Tool => messages.push(json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.content,
            })),
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": with_usage },
    });

    if let Some(t) = req.max_tokens {
        body["max_tokens"] = json!(t);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }

    // OpenAI 侧缓存靠前缀命中，隐式生效，不需要显式断点。
    body
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct StreamState {
    text: String,
    partial: std::collections::BTreeMap<u32, PartialCall>,
    usage: Usage,
    stop: StopReason,
    got_usage: bool,
}

impl StreamState {
    fn on_event(&mut self, ev: &http::SseEvent, tx: &tokio::sync::mpsc::UnboundedSender<String>) {
        if ev.data == http::SSE_DONE || ev.data.trim().is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return;
        };

        // 错误体有时也走 200 + SSE
        if let Some(err) = v.get("error").and_then(|e| e.as_object()) {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            tracing::error!(%msg, "openai stream error");
            return;
        }

        let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
            // usage-only 的尾包，choices 为空
            if let Some(u) = v.get("usage") {
                self.apply_usage(u);
            }
            return;
        };

        let delta = &choice["delta"];

        if let Some(t) = delta["content"].as_str() {
            if !t.is_empty() {
                self.text.push_str(t);
                let _ = tx.send(t.to_string());
            }
        }

        if let Some(calls) = delta["tool_calls"].as_array() {
            for c in calls {
                let index = c["index"].as_u64().unwrap_or(0) as u32;
                let slot = self.partial.entry(index).or_default();
                if let Some(id) = c["id"].as_str() {
                    slot.id = id.to_string();
                }
                if let Some(name) = c["function"]["name"].as_str() {
                    slot.name.push_str(name);
                }
                if let Some(args) = c["function"]["arguments"].as_str() {
                    slot.args.push_str(args);
                }
            }
        }

        if let Some(fr) = choice["finish_reason"].as_str() {
            self.stop = match fr {
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                "content_filter" => StopReason::Refusal,
                _ => StopReason::EndTurn,
            };
        }

        if let Some(u) = v.get("usage") {
            self.apply_usage(u);
        }
    }

    fn apply_usage(&mut self, u: &Value) {
        if u.is_null() {
            return;
        }
        self.got_usage = true;
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as u32;
        let prompt = u["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        self.usage.input_tokens = prompt.saturating_sub(cached);
        self.usage.cache_read_tokens = cached;
        self.usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0) as u32;
    }

    fn finish(mut self) -> ChatResponse {
        let tool_calls: Vec<ToolCall> = std::mem::take(&mut self.partial)
            .into_values()
            .filter(|p| !p.name.is_empty())
            .map(|p| {
                let args = serde_json::from_str(&p.args)
                    .unwrap_or_else(|_| json!({ "__raw": p.args.trim() }));
                ToolCall::new(p.id, p.name, args)
            })
            .collect();

        if self.stop == StopReason::EndTurn && !tool_calls.is_empty() {
            self.stop = StopReason::ToolUse;
        }
        if !self.got_usage {
            self.usage.output_tokens = (self.text.len() / 4) as u32;
        }

        ChatResponse {
            content: self.text,
            tool_calls,
            usage: self.usage,
            stop_reason: self.stop,
        }
    }
}

/// 供 registry 使用的默认能力表。
pub fn default_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "gpt-4.1".into(),
            display_name: "GPT-4.1".into(),
            capabilities: Capabilities {
                tools: true,
                vision: true,
                prompt_cache: true,
                context_window: 1_000_000,
            },
        },
        ModelInfo {
            id: "o4-mini".into(),
            display_name: "o4-mini (推理)".into(),
            capabilities: Capabilities {
                tools: true,
                vision: true,
                prompt_cache: true,
                context_window: 200_000,
            },
        },
        ModelInfo {
            id: "gpt-4.1-mini".into(),
            display_name: "GPT-4.1 mini".into(),
            capabilities: Capabilities {
                tools: true,
                vision: true,
                prompt_cache: true,
                context_window: 1_000_000,
            },
        },
    ]
}
