//! 工具抽象。
//!
//! 工具只负责"给我参数，我给你结果"，不涉及审批与并发调度，
//! 那两件事由 [`crate::tools_exec`] 统一处理。

use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

use crate::event::ToolOutcome;

/// 工具执行失败。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("参数无效: {0}")]
    InvalidParams(String),
    #[error("文件不存在: {0}")]
    NotFound(String),
    #[error("超出允许的工作区范围: {0}")]
    OutsideWorkspace(String),
    #[error("{0}")]
    Failed(String),
}

/// 强制用户在批准前看到的内容。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    /// 一行标题，如 `bash: cargo test`。
    pub title: String,
    /// 完整内容（命令全文、diff 等）。
    pub body: String,
}

/// 审批裁决结果。
///
/// 刻意和 [`Permission`] 分开：之前 `request_approval` 返回 `None`
/// （"这个工具不用问"）被直接映射成 `Permission::Allow`，
/// 于是"没问"和"用户按了 y"在执行器眼里完全一样——
/// 既没法给用户不同反馈，定位问题也费劲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 工具自己判定无需审批（只读操作）
    NotRequired,
    /// 用户允许这一次
    Allow,
    /// 用户允许本轮内该工具的全部调用
    AllowSession,
    /// 用户拒绝，或审批通道已经关闭
    Deny,
}

impl Decision {
    pub fn allowed(self) -> bool {
        matches!(
            self,
            Decision::NotRequired | Decision::Allow | Decision::AllowSession
        )
    }

    /// 拒绝时回给模型的说明。
    pub fn denial_reason(self) -> Option<&'static str> {
        match self {
            Decision::Deny => Some(
                "用户拒绝了这次执行。改用其他方案，或先向用户说明为什么需要它。",
            ),
            _ => None,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// 稳定标识，也是模型看到的函数名。
    fn name(&self) -> &str;

    /// 给模型看的一句话说明。
    fn description(&self) -> &str;

    /// JSON Schema（object）。
    fn parameters(&self) -> Value;

    /// 摘要级别的副作用描述，用于权限分级与 UI 预览。
    fn preview(&self, args: &Value) -> String;

    /// 这个工具单次执行的时间上限。
    ///
    /// `None` 表示用全局上限。工具可以放宽（跑集成测试很慢），
    /// 但收紧只能靠全局配置，避免单个工具把自己卡死。
    fn max_runtime(&self) -> Option<Duration> {
        None
    }

    /// 执行前是否需要用户批准。
    async fn request_approval(&self, _args: &Value) -> Option<PendingApproval> {
        None
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, ToolError>;
}

/// 便捷构造 object schema。
pub fn object_schema(props: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}
