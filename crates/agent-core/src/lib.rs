//! `agent-core` —— 与厂商、与 UI 都无关的 coding agent 内核。
//!
//! 分层约定：
//! - [`message`]：中立的消息/请求/用量表示，是所有适配器的输入输出契约
//! - [`provider`]：接向后端的唯一 trait
//! - [`tool`]：工具接口；审批与并发由 [`tools_exec`] 负责
//! - [`conversation`]：历史、token 预算、压缩
//! - [`agent`]：主循环，只产出 [`event::AgentEvent`]
//!
//! 任何渲染层都不应该出现在这个 crate 里。

pub mod agent;
pub mod cancel;
pub mod conversation;
pub mod event;
pub mod message;
pub mod provider;
pub mod tool;
pub mod tools_exec;

pub use agent::{AgentConfig, ControlHandle, Runner, describe_stop, describe_stop_reason, spawn_run};
pub use conversation::{CompactPlan, ContextBudget, Conversation};
pub use event::{AgentEvent, Control, NoticeLevel, Permission, RunEndReason, ToolOutcome};
pub use message::{
    CacheHint, Capabilities, ChatRequest, ChatResponse, Message, ModelInfo, Role, StopReason,
    ToolCall, ToolSpec, Usage,
};
pub use provider::Provider;
pub use cancel::Cancel;
pub use tool::{Decision, PendingApproval, Tool, ToolError, object_schema};
pub use tools_exec::{CANCELLED, ExecOptions, Registry, Risk};
