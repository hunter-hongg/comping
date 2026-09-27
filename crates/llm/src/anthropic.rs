//! Anthropic Messages API 适配器。
//!
//! 中立表示 → Anthropic 的映射要点：
//! - system 是顶层字段，不是消息
//! - `tool_use` / `tool_result` 靠消息里的 content block 区分
//! - 流式增量需要按 `index` 合并 tool call 的参数片段

use agent_core::{
    CacheHint, Capabilities, ChatRequest, ChatResponse, Message, ModelInfo, Provider,
    Role, StopReason, ToolCall, Usage,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::http;
use crate::registry::ProviderConfig;

pub const DEFAULT_BASE: &str = "https://api.anthropic.com/v1";
pub const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    base: String,
    api_key: String,
    default_model: String,
    models: Vec<ModelInfo>,
}

impl AnthropicProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let base = base.trim_end_matches('/').to_string();
        let api_key = cfg
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| anyhow::anyhow!(
                "缺少 Anthropic API key：设置 config 里的 api_key 或环境变量 ANTHROPIC_API_KEY"
            ))?;

        Ok(Self {
            client: http::build_client()?,
            default_model: cfg
                .model
                .clone()
                .unwrap_or_else(|| "claude-sonnet-4-5".to_string()),
            models: crate::registry::known_models("anthropic"),
            base,
            api_key,
        })
    }

    pub fn with_models(mut self, models: Vec<ModelInfo>) -> Self {
        if !models.is_empty() {
            self.models = models;
        }
        self
    }

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("x-api-key".into(), self.api_key.clone()),
            ("anthropic-version".into(), API_VERSION.into()),
            ("content-type".into(), "application/json".into()),
        ]
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
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
        let body = encode_request(&request, model);

        let resp = http::post_stream(
            &self.client,
            &format!("{}/messages", self.base),
            &self.headers(),
            &body,
        )
        .await?;

        let mut state = StreamState::default();
        http::consume_sse(resp, |ev| {
            state.on_event(&ev, &tx);
            Ok(())
        })
        .await?;

        Ok(state.finish())
    }
}

/// 中立请求 → Anthropic body。
pub fn encode_request(req: &ChatRequest, model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // Anthropic 不接受空 content 的 user 消息，也不接受连续同角色，
    // 这里按 role 变化切块并跳过空块。
    for m in &req.messages {
        if m.role == Role::System {
            continue; // system 走顶层字段
        }
        push_anthropic_message(&mut messages, m);
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(8192),
        "stream": true,
    });

    if let Some(sys) = &req.system {
        if !sys.is_empty() {
            body["system"] = json!([{ "type": "text", "text": sys, "cache_control": { "type": "ephemeral" } }]);
        }
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
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect(),
        );
    }

    apply_cache_breakpoints(&mut body, req.cache_hint);
    body
}

/// 把一条中立消息转成 Anthropic 的 content blocks，并处理角色合并。
fn push_anthropic_message(out: &mut Vec<Value>, m: &Message) {
    let role = match m.role {
        Role::User | Role::System => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user", // tool_result 走 user 角色
    };

    let mut blocks: Vec<Value> = Vec::new();

    if m.role == Role::Tool {
        blocks.push(json!({
            "type": "tool_result",
            "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.content,
            "is_error": m.content.starts_with("错误:"),
        }));
    } else {
        if !m.content.trim().is_empty() {
            blocks.push(json!({ "type": "text", "text": m.content }));
        }
        for c in &m.tool_calls {
            blocks.push(json!({
                "type": "tool_use",
                "id": c.id,
                "name": c.name,
                "input": c.arguments,
            }));
        }
    }

    if blocks.is_empty() {
        return;
    }

    // 与上一条同角色则合并 content，避免 API 报错
    if let Some(last) = out.last_mut() {
        if last["role"] == json!(role) {
            let arr = last["content"].as_array_mut().unwrap();
            arr.extend(blocks);
            return;
        }
    }

    out.push(json!({ "role": role, "content": blocks }));
}

/// 打 cache_control 断点：system 已打，这里给倒数第 N 条消息再打。
fn apply_cache_breakpoints(body: &mut Value, hint: Option<CacheHint>) {
    let Some(hint) = hint else { return };
    let n = hint.messages.min(4);
    let Some(arr) = body["messages"].as_array_mut() else {
        return;
    };
    let len = arr.len();
    if len == 0 {
        return;
    }
    // 从尾部往前打 n 个断点
    let start = len.saturating_sub(n.max(1));
    for msg in arr.iter_mut().skip(start) {
        let Some(blocks) = msg["content"].as_array_mut() else {
            continue;
        };
        if let Some(last) = blocks.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
    }
}

/// 流式状态机：Anthropic 的增量事件需要按 index 归并。
#[derive(Default)]
struct StreamState {
    text: String,
    /// index → (id, name, 参数 JSON 片段)
    partial: std::collections::BTreeMap<u32, PartialCall>,
    usage: Usage,
    stop: StopReason,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

impl StreamState {
    fn on_event(&mut self, ev: &http::SseEvent, tx: &tokio::sync::mpsc::UnboundedSender<String>) {
        // Anthropic 用 event 名区分负载类型；也兼容只发 data 的代理。
        let kind = ev.event.as_deref().unwrap_or("message");
        if ev.data == http::SSE_DONE {
            return;
        }

        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return;
        };

        match kind {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    self.usage.input_tokens = num(u, "input_tokens");
                    self.usage.cache_read_tokens = num(u, "cache_read_input_tokens");
                    self.usage.cache_write_tokens = num(u, "cache_creation_input_tokens");
                }
            }
            "content_block_start" => {
                let index = v["index"].as_u64().unwrap_or(0) as u32;
                let block = &v["content_block"];
                if block["type"] == json!("tool_use") {
                    self.partial.insert(
                        index,
                        PartialCall {
                            id: block["id"].as_str().unwrap_or_default().to_string(),
                            name: block["name"].as_str().unwrap_or_default().to_string(),
                            args: String::new(),
                        },
                    );
                }
            }
            "content_block_delta" => {
                let index = v["index"].as_u64().unwrap_or(0) as u32;
                let delta = &v["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let t = delta["text"].as_str().unwrap_or_default();
                        if !t.is_empty() {
                            self.text.push_str(t);
                            let _ = tx.send(t.to_string());
                        }
                    }
                    "input_json_delta" => {
                        if let Some(e) = self.partial.get_mut(&index) {
                            if let Some(p) = delta["partial_json"].as_str() {
                                e.args.push_str(p);
                            }
                        }
                    }
                    "thinking_delta" => {
                        // 扩展思考内容并入文本，MVP 不单独渲染
                        if let Some(t) = delta["thinking"].as_str() {
                            self.text.push_str(t);
                            let _ = tx.send(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                self.usage.output_tokens = num(&v["usage"], "output_tokens");
                self.stop = match v["delta"]["stop_reason"].as_str() {
                    Some("tool_use") => StopReason::ToolUse,
                    Some("max_tokens") => StopReason::MaxTokens,
                    Some("refusal") => StopReason::Refusal,
                    _ => self.stop,
                };
            }
            "error" => {
                tracing::error!(data = %ev.data, "anthropic stream error");
            }
            _ => {}
        }
    }

    fn finish(mut self) -> ChatResponse {
        let tool_calls = std::mem::take(&mut self.partial)
            .into_values()
            .map(|p| {
                let args = serde_json::from_str(&p.args)
                    .unwrap_or_else(|_| json!({ "__raw": p.args }));
                ToolCall::new(p.id, p.name, args)
            })
            .collect::<Vec<_>>();

        if self.stop == StopReason::EndTurn && !tool_calls.is_empty() {
            self.stop = StopReason::ToolUse;
        }

        ChatResponse {
            content: self.text,
            tool_calls,
            usage: self.usage,
            stop_reason: self.stop,
        }
    }
}

fn num(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0) as u32
}

/// 内置模型表。上下文窗口按官方公布的 1M 标注。
pub fn default_models() -> Vec<ModelInfo> {
    let cap = |window| Capabilities {
        tools: true,
        vision: true,
        prompt_cache: true,
        context_window: window,
    };
    vec![
        ModelInfo {
            id: "claude-sonnet-4-5".into(),
            display_name: "Claude Sonnet 4.5".into(),
            capabilities: cap(1_000_000),
        },
        ModelInfo {
            id: "claude-opus-4-1".into(),
            display_name: "Claude Opus 4.1".into(),
            capabilities: cap(200_000),
        },
        ModelInfo {
            id: "claude-haiku-4-5".into(),
            display_name: "Claude Haiku 4.5".into(),
            capabilities: cap(200_000),
        },
    ]
}
