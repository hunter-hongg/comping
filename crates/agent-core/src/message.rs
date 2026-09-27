//! 中间消息表示：不泄漏任何厂商私有格式。
//!
//! 所有 provider 适配器都只负责 `ChatRequest`/`ChatResponse` 与自家协议的互转。

use serde::{Deserialize, Serialize};

/// 对话角色。`Tool` 承载工具结果，`Assistant` 可携带工具调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// 模型请求调用某个工具。`id` 由 agent-core 分配，保证跨轮次可追溯。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 已解析的参数。适配器需要时自行 `serde_json::to_string`。
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    /// 参数的紧凑字符串形式，用于日志与 UI 展示。
    pub fn arguments_json(&self) -> String {
        self.arguments.to_string()
    }
}

/// 一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// 仅 `Role::Assistant` 非空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// 仅 `Role::Tool` 非空：指向被回填结果的 `ToolCall.id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// 工具结果消息。`id` 必须与对应的 `ToolCall.id` 一致。
    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        }
    }

    pub fn with_tool_calls(mut self, calls: Vec<ToolCall>) -> Self {
        self.tool_calls = calls;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty() && self.tool_calls.is_empty()
    }
}

/// 工具的 JSON Schema 定义，按 OpenAI `tools[].function` 的形状归一。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// 直接是一个 JSON Schema object。
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// 发送给 provider 的一次请求。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// 厂商侧缓存提示（Anthropic 的 cache_control 等）。中立层只做透传。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hint: Option<CacheHint>,
}

impl ChatRequest {
    pub fn new(system: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            system: Some(system.into()),
            messages,
            ..Default::default()
        }
    }

    /// 最近若干条消息，用于上下文预算估算。
    pub fn tail(&self, n: usize) -> &[Message] {
        if self.messages.len() <= n {
            &self.messages
        } else {
            &self.messages[self.messages.len() - n..]
        }
    }
}

/// 跨厂商统一的缓存提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHint {
    /// 只缓存 system 段。
    pub system: bool,
    /// 缓存到最近 N 条消息为止（Anthropic 最多 4 个 breakpoint）。
    pub messages: usize,
}

impl Default for CacheHint {
    fn default() -> Self {
        Self {
            system: true,
            messages: 2,
        }
    }
}

/// 用量统计，跨厂商字段归一。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

impl Usage {
    /// 计入账单的口径：缓存读按折扣算，这里只做粗略总量。
    pub fn total(&self) -> u32 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    /// 缓存命中率（0.0–1.0），没有可缓存前缀时为 0。
    pub fn cache_hit_ratio(&self) -> f32 {
        let cacheable = self.input_tokens + self.cache_read_tokens;
        if cacheable == 0 {
            0.0
        } else {
            self.cache_read_tokens as f32 / cacheable as f32
        }
    }

    pub fn merge(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// 逐字段相减，得到 `self` 相对 `base` 的增量。
    ///
    /// 用 `saturating_sub`：用量是累加的，重放或恢复会话时不会出现负数，
    /// 万一顺序乱了也不该 panic。
    pub fn since(&self, base: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_sub(base.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(base.output_tokens),
            cache_read_tokens: self.cache_read_tokens.saturating_sub(base.cache_read_tokens),
            cache_write_tokens: self.cache_write_tokens.saturating_sub(base.cache_write_tokens),
        }
    }
}

/// 模型能力位。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tools: bool,
    pub vision: bool,
    pub prompt_cache: bool,
    /// 上下文窗口大小。
    pub context_window: u32,
}

impl Capabilities {
    pub const fn text_only() -> Self {
        Self {
            tools: true,
            vision: false,
            prompt_cache: false,
            context_window: 128_000,
        }
    }
}

/// provider 内可选模型的信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub capabilities: Capabilities,
}

/// 助手一轮的完整回复。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
}
