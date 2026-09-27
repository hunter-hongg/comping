//! Provider trait：中立层与厂商之间的唯一接缝。

use async_trait::async_trait;

use crate::message::{ChatRequest, ChatResponse, ModelInfo};

/// 一个模型后端。
///
/// trait 刻意做得很窄：只描述"如何把请求变成事件流"。
/// 所有厂商特有的东西（缓存、思考预算、多模态）都通过
/// [`ChatRequest`] 里的中立字段透传，不污染 trait 签名。
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// 稳定的短名，用于配置查找与日志，如 `anthropic`。
    fn name(&self) -> &str;

    /// 该 provider 默认推荐的模型。
    fn default_model(&self) -> &str;

    /// 可用模型清单（含能力位），供 `/model` 面板展示。
    fn models(&self) -> &[ModelInfo];

    /// 发起一次流式请求。
    ///
    /// 实现方通过 `tx` 推送文本增量片段，返回值里给出合并后的完整回复。
    /// 这样 agent 循环不必二次拼接，也便于适配器边解析边推送。
    async fn stream(
        &self,
        request: ChatRequest,
        model: &str,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> anyhow::Result<ChatResponse>;
}
