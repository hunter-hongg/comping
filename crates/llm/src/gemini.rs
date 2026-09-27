//! Google Gemini `generateContent` 适配器。
//!
//! 与前两家的主要差异：
//! - `contents[].parts` 是多模态数组，工具调用是 `functionCall`
//! - 工具结果要**作为下一条 user 消息的 `functionResponse` part**
//! - system prompt 走 `systemInstruction`
//! - API key 走 query 参数 `?key=`，不是 header
//! - 流式用 `:streamGenerateContent?alt=sse`

use agent_core::{
    Capabilities, ChatRequest, ChatResponse, Message, ModelInfo, Provider, Role,
    StopReason, ToolCall, Usage,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::http;
use crate::registry::ProviderConfig;

pub const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiProvider {
    client: reqwest::Client,
    base: String,
    api_key: String,
    default_model: String,
    models: Vec<ModelInfo>,
}

impl GeminiProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let api_key = cfg
            .api_key
            .clone()
            .or_else(|| std::env::var("GEMINI_API_KEY").ok())
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
            .ok_or_else(|| {
                anyhow::anyhow!("缺少 Gemini API key：设置 api_key 或环境变量 GEMINI_API_KEY")
            })?;

        Ok(Self {
            client: http::build_client()?,
            default_model: cfg.model.clone().unwrap_or_else(|| "gemini-2.5-pro".into()),
            models: crate::registry::known_models("gemini"),
            base: base.trim_end_matches('/').to_string(),
            api_key,
        })
    }

    pub fn with_models(mut self, models: Vec<ModelInfo>) -> Self {
        if !models.is_empty() {
            self.models = models;
        }
        self
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
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
        let body = encode_request(&request);
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base, model, self.api_key
        );

        let resp = http::post_stream(&self.client, &url, &[], &body).await?;

        let mut state = StreamState::default();
        http::consume_sse(resp, |ev| {
            state.on_event(&ev, &tx);
            Ok(())
        })
        .await?;

        Ok(state.finish())
    }
}

/// 中立请求 → Gemini body。
pub fn encode_request(req: &ChatRequest) -> Value {
    let mut contents: Vec<Value> = Vec::new();

    for m in &req.messages {
        if m.role == Role::System {
            continue;
        }
        if let Some(v) = encode_content(m) {
            contents.push(v);
        }
    }

    let mut body = json!({ "contents": contents });

    if let Some(sys) = &req.system {
        if !sys.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": sys }] });
        }
    }

    let mut cfg = serde_json::Map::new();
    if let Some(t) = req.max_tokens {
        cfg.insert("maxOutputTokens".into(), json!(t));
    }
    if let Some(t) = req.temperature {
        cfg.insert("temperature".into(), json!(t));
    }
    if !cfg.is_empty() {
        body["generationConfig"] = Value::Object(cfg);
    }

    if !req.tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": req.tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "parameters": strip_unsupported_schema(&t.parameters),
            })).collect::<Vec<_>>()
        }]);
    }

    body
}

fn encode_content(m: &Message) -> Option<Value> {
    let role = match m.role {
        Role::Assistant => "model",
        _ => "user",
    };

    let mut parts: Vec<Value> = Vec::new();

    match m.role {
        Role::Tool => {
            // 编码时把 id 编成 "name#n"，这里还原成 Gemini 认的 name。
            let name = m
                .tool_call_id
                .as_deref()
                .unwrap_or_default()
                .split('#')
                .next()
                .unwrap_or_default()
                .to_string();
            parts.push(json!({
                "functionResponse": {
                    "name": name,
                    "response": { "result": m.content },
                }
            }));
        }
        _ => {
            if !m.content.trim().is_empty() {
                parts.push(json!({ "text": m.content }));
            }
            for c in &m.tool_calls {
                parts.push(json!({
                    "functionCall": { "name": c.name, "args": c.arguments }
                }));
            }
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(json!({ "role": role, "parts": parts }))
}

/// Gemini 的 schema 子集比 JSON Schema 小，得剔掉它不认的关键字。
fn strip_unsupported_schema(schema: &Value) -> Value {
    const UNSUPPORTED: &[&str] = &[
        "additionalProperties",
        "$schema",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "default",
        "oneOf",
        "allOf",
        "not",
        "const",
    ];
    let mut v = schema.clone();
    if let Some(obj) = v.as_object_mut() {
        for k in UNSUPPORTED {
            obj.remove(*k);
        }
        // properties 递归清理
        if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
            for (_, sub) in props.iter_mut() {
                *sub = strip_unsupported_schema(sub);
            }
        }
    }
    v
}

#[derive(Default)]
struct StreamState {
    text: String,
    /// 累积的 functionCall，顺序即模型给出的顺序。
    calls: Vec<ToolCall>,
    usage: Usage,
    stop: StopReason,
}

impl StreamState {
    fn on_event(&mut self, ev: &http::SseEvent, tx: &tokio::sync::mpsc::UnboundedSender<String>) {
        if ev.data == http::SSE_DONE || ev.data.trim().is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return;
        };

        if let Some(u) = v.get("usageMetadata") {
            self.usage.input_tokens = u["promptTokenCount"].as_u64().unwrap_or(0) as u32;
            self.usage.output_tokens = u["candidatesTokenCount"].as_u64().unwrap_or(0) as u32;
            // Gemini 只给 cachedContentTokenCount，没有独立的 write 计数
            self.usage.cache_read_tokens = u["cachedContentTokenCount"].as_u64().unwrap_or(0) as u32;
        }

        let Some(cand) = v["candidates"].as_array().and_then(|c| c.first()) else {
            return;
        };

        if let Some(reason) = cand["finishReason"].as_str() {
            self.stop = match reason {
                "STOP" => StopReason::EndTurn,
                "MAX_TOKENS" => StopReason::MaxTokens,
                "SAFETY" | "PROHIBITED_CONTENT" | "RECITATION" => StopReason::Refusal,
                _ => self.stop,
            };
        }

        let Some(parts) = cand["content"]["parts"].as_array() else {
            return;
        };

        for p in parts {
            if let Some(t) = p["text"].as_str() {
                if !t.is_empty() {
                    self.text.push_str(t);
                    let _ = tx.send(t.to_string());
                }
            }
            if let Some(fc) = p.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or_default().to_string();
                if name.is_empty() {
                    continue;
                }
                let args = fc
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                // Gemini 不给 tool call id，只用函数名关联。
                // 因此把 id 编成 "name" 或 "name#n"，回填时再还原成 name。
                let id = self.unique_id(&name);
                self.calls.push(ToolCall::new(id, name, args));
            }
        }
    }

    /// 生成 `name` 或 `name#n` 形式的唯一 id。
    fn unique_id(&self, name: &str) -> String {
        if !self.calls.iter().any(|c| c.name == name) {
            name.to_string()
        } else {
            let mut n = 2;
            loop {
                let cand = format!("{name}#{n}");
                if !self.calls.iter().any(|c| c.name == name && c.id == cand) {
                    return cand;
                }
                n += 1;
            }
        }
    }

    fn finish(mut self) -> ChatResponse {
        if self.stop == StopReason::EndTurn && !self.calls.is_empty() {
            self.stop = StopReason::ToolUse;
        }
        ChatResponse {
            content: self.text,
            tool_calls: std::mem::take(&mut self.calls),
            usage: self.usage,
            stop_reason: self.stop,
        }
    }
}

pub fn default_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "gemini-2.5-pro".into(),
            display_name: "Gemini 2.5 Pro".into(),
            capabilities: Capabilities {
                tools: true,
                vision: true,
                prompt_cache: true,
                context_window: 1_000_000,
            },
        },
        ModelInfo {
            id: "gemini-2.5-flash".into(),
            display_name: "Gemini 2.5 Flash".into(),
            capabilities: Capabilities {
                tools: true,
                vision: true,
                prompt_cache: true,
                context_window: 1_000_000,
            },
        },
    ]
}
