//! Agent 对外推送的事件流。
//!
//! 这是整个项目的核心抽象：agent 循环只产出事件，不关心谁在消费。
//! TUI 渲染它，headless 模式打印它，将来 Web UI 通过 SSE 转发它。

use crate::message::{StopReason, ToolCall, Usage};
use crate::tool::Decision;

/// 一次工具调用的执行结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub output: String,
    pub ok: bool,
    /// 供 UI 展示的一行摘要，如 `read src/main.rs (42 行)`。
    pub summary: String,
    /// 输出被截断时的原始落盘路径。
    pub truncated_to: Option<String>,
}

impl ToolOutcome {
    pub fn ok(summary: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ok: true,
            summary: summary.into(),
            truncated_to: None,
        }
    }

    pub fn err(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            summary: first_line(&output).to_string(),
            output,
            ok: false,
            truncated_to: None,
        }
    }
}

/// 权限决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// 本次允许。
    Allow,
    /// 本轮内同类工具全部允许。
    AllowSession,
    /// 拒绝，理由回传给模型。
    Deny,
}

/// 事件。
///
/// 只有需要 clone 的变体才 clone：`ApprovalNeeded` 里装着只能消费一次的
/// oneshot sender，整体放弃 `Clone`，改由调用方按引用处理。
#[derive(Debug)]
pub enum AgentEvent {
    /// 用户提交了一条输入（已入队）。
    UserMessage { text: String },
    /// 助手文本增量，用于流式渲染。
    AssistantDelta { text: String },
    /// 助手调用工具。
    ToolCallStarted { call: ToolCall },
    /// 工具执行完毕。
    ToolCallFinished {
        call: ToolCall,
        outcome: ToolOutcome,
        /// 相对本轮开始的耗时。
        elapsed_ms: u64,
    },
    /// 需要用户裁决。该工具已挂起，直到收到回应或被取消。
    ///
    /// 多个工具同时要批准时会连发多个这种事件，**消费方必须排队**，
    /// 不能只留最后一个——丢掉任何一个都会让那个工具的 `respond`
    /// 被 drop，结果被误判成"用户拒绝了"。
    ApprovalNeeded {
        call: ToolCall,
        /// 一行标题，如 `bash · cargo test --all`。
        title: String,
        /// 完整内容（命令全文、文件 diff 等）。
        body: String,
        /// 回传决定。接收端关闭时执行器按拒绝处理。
        respond: tokio::sync::mpsc::Sender<Decision>,
    },
    /// 一轮助手回复结束（含工具调用）。
    TurnEnd {
        /// 本轮内的助手文本（已合并增量）。
        text: String,
        usage: Usage,
        stop_reason: StopReason,
        /// 本轮是否发起了工具调用。
        had_tool_calls: bool,
        elapsed_ms: u64,
        /// 已进行的轮数（从 1 开始）。
        step: usize,
    },
    /// 整次用户请求结束。
    RunEnd { reason: RunEndReason },
    /// 轮数或 token 超限等非致命终止。
    Notice { level: NoticeLevel, text: String },
    /// 致命错误，之后必然紧跟一个 [`AgentEvent::RunEnd`]。
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEndReason {
    /// 模型不再要求工具，正常收尾。
    Completed,
    /// 达到最大轮数。
    MaxSteps { steps: usize },
    /// 上下文预算耗尽，已尝试压缩。
    ContextExhausted,
    /// 被用户中断。
    Interrupted,
    /// 出错。
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
}

/// 用户在运行中下达的干预。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// 中断当前请求。正在跑的工具会停下来。
    Interrupt,
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}
