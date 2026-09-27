//! Agent 主循环。
//!
//! 一次用户请求 = 若干 step。每个 step：
//!   1. 组装请求（必要时先压缩上下文）
//!   2. 流式请求模型，收集文本 + tool_calls
//!   3. 有工具调用就并行执行，把结果作为 tool 消息回填
//!   4. 重复，直到模型不再要工具或触发停止条件
//!
//! 循环只通过 `AgentEvent` 对外说话，因此 TUI / headless / Web 前端可互换。

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::conversation::Conversation;
use crate::event::{AgentEvent, Control, NoticeLevel, RunEndReason};
use crate::cancel::Cancel;
use crate::event::ToolOutcome;
use crate::message::{Message, StopReason, ToolCall};
use crate::tools_exec::ExecOptions;
use crate::provider::Provider;
use crate::tools_exec::Registry;

/// 运行期可调参数。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 单次请求内最大 step 数，防死循环。
    pub max_steps: usize,
    /// 工具并发上限。
    pub tool_concurrency: usize,
    /// 达到 soft limit 时是否自动压缩。
    pub auto_compact: bool,
    /// 单个工具的超时上限。`None` 表示不限时。
    ///
    /// 这是兜底：工具实现里挂死了，至少 agent 循环不会被拖住。
    pub tool_timeout: Option<std::time::Duration>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 25,
            tool_concurrency: 4,
            auto_compact: true,
            tool_timeout: Some(std::time::Duration::from_secs(300)),
        }
    }
}

/// 交给后台任务的控制面。
#[derive(Clone)]
pub struct ControlHandle {
    tx: mpsc::UnboundedSender<Control>,
    /// 取消信号。工具层靠它停下来，模型循环靠它跳出。
    cancel: Cancel,
}

impl ControlHandle {
    /// 中断当前请求。
    ///
    /// 会同时置位取消信号：正在并行跑的工具会停止启动新任务，
    /// 等待中的批准会立刻按拒绝处理。
    pub fn interrupt(&self) {
        self.cancel.cancel();
        let _ = self.tx.send(Control::Interrupt);
    }

    /// 取消令牌。UI 想自己观察中断状态时用。
    pub fn cancel(&self) -> &Cancel {
        &self.cancel
    }

}

/// 一次性运行句柄：提交输入、接收事件、随时中断。
pub struct Runner {
    events: mpsc::UnboundedReceiver<AgentEvent>,
    control: ControlHandle,
    task: Option<tokio::task::JoinHandle<(RunEndReason, Conversation)>>,
    /// 任务结束后取回更新过的会话，供 UI 持久化。
    finished: Arc<std::sync::Mutex<Option<Conversation>>>,
}

impl Runner {
    pub fn events(&mut self) -> &mut mpsc::UnboundedReceiver<AgentEvent> {
        &mut self.events
    }

    pub fn control(&self) -> ControlHandle {
        self.control.clone()
    }

    /// 等待后台任务结束。
    pub async fn join(&mut self) -> Option<RunEndReason> {
        match self.task.take() {
            Some(t) => t.await.ok().map(|(reason, _)| reason),
            None => None,
        }
    }

    /// 轮询式检查任务是否结束，结束则取回会话。
    ///
    /// UI 侧不想 await 一个长任务时用这个：返回 `Some` 表示运行已收尾。
    pub fn try_take_conversation(&self) -> Option<Conversation> {
        let mut slot = self.finished.lock().ok()?;
        slot.take()
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }
}

/// 启动一次 agent 运行。
///
/// `model` 显式传入而不是问 `provider.default_model()`：
/// 这样切换模型只是改一个字段，不必重建 provider（重建会重新建 HTTP 连接）。
pub fn spawn_run(
    provider: Arc<dyn Provider>,
    registry: Registry,
    mut conversation: Conversation,
    system: String,
    config: AgentConfig,
    input: String,
    model: String,
) -> Runner {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Control>();

    let cancel = Cancel::new();
    let control = ControlHandle {
        tx: ctl_tx,
        cancel: cancel.clone(),
    };

    let finished = Arc::new(std::sync::Mutex::new(None));
    let slot = finished.clone();

    let task = tokio::spawn(async move {
        let reason = run_inner(
            &provider,
            &registry,
            &mut conversation,
            &system,
            &config,
            &mut ctl_rx,
            &cancel,
            input,
            &model,
            &event_tx,
        )
        .await;

        // 把最终会话交出去，UI 才能接着上一轮继续。
        if let Ok(mut s) = slot.lock() {
            *s = Some(conversation.clone());
        }
        (reason, conversation)
    });

    Runner {
        events: event_rx,
        control,
        task: Some(task),
        finished,
    }
}

/// 把工具结果写回对话。
///
/// 无论成功、失败还是被取消，都必须写一条 `tool_result`：
/// 少一条，下一轮请求里就出现"有 tool_call 没有 tool_result"，
/// Anthropic / OpenAI 会直接返回 400。
fn push_tool_result(conversation: &mut Conversation, call: ToolCall, res: Result<ToolOutcome, String>) {
    let content = match res {
        Ok(out) => {
            let mut c = out.output;
            if let Some(p) = &out.truncated_to {
                c.push_str(&format!("\n[输出过长，完整内容见 {p}]"));
            }
            c
        }
        Err(e) => format!("错误: {e}"),
    };
    conversation.push(Message::tool_result(call.id, content));
}

/// 真正的循环体，拆出来只为让 `spawn_run` 的闭包保持扁平。
#[allow(clippy::too_many_arguments)]
async fn run_inner(
    provider: &Arc<dyn Provider>,
    registry: &Registry,
    conversation: &mut Conversation,
    system: &str,
    config: &AgentConfig,
    ctl_rx: &mut mpsc::UnboundedReceiver<Control>,
    cancel: &Cancel,
    input: String,
    model: &str,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) -> RunEndReason {
    conversation.push(Message::user(input));
    registry.reset_session();

    for step in 1..=config.max_steps {
        // 上下文压缩
        if config.auto_compact && conversation.should_compact() {
            let before = conversation.estimate_tokens();
            if conversation.compact(None) {
                let after = conversation.estimate_tokens();
                let _ = event_tx.send(AgentEvent::Notice {
                    level: NoticeLevel::Info,
                    text: format!("上下文已压缩: {before} → {after} tokens"),
                });
            }
        }

        let step_started = Instant::now();
        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();

        // 把增量转发到对外事件流
        let fwd = event_tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(text) = delta_rx.recv().await {
                if !text.is_empty() && fwd.send(AgentEvent::AssistantDelta { text }).is_err() {
                    break;
                }
            }
        });

        let request = conversation.to_request(system, registry.specs());
        let result = provider.stream(request, model, delta_tx).await;

        // 等转发任务排空，保证 UI 文本与 TurnEnd 顺序一致
        let _ = forwarder.await;

        let response = match result {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("{e:#}");
                let _ = event_tx.send(AgentEvent::Failed {
                    error: msg.clone(),
                });
                return RunEndReason::Error { message: msg };
            }
        };

        conversation.record_assistant(
            &response.content,
            response.tool_calls.clone(),
            &response.usage,
        );

        let _ = event_tx.send(AgentEvent::TurnEnd {
            text: response.content.clone(),
            usage: response.usage,
            stop_reason: response.stop_reason,
            had_tool_calls: !response.tool_calls.is_empty(),
            elapsed_ms: step_started.elapsed().as_millis() as u64,
            step,
        });

        // 用户干预优先于一切。取消信号是权威来源——
        // 工具执行期间的中断走不到这里，但会置位这个信号。
        if cancel.is_cancelled() {
            let _ = event_tx.send(AgentEvent::RunEnd {
                reason: RunEndReason::Interrupted,
            });
            return RunEndReason::Interrupted;
        }
        // 控制通道被关闭 = UI 已经走了，等价于请求中断
        match ctl_rx.try_recv() {
            Ok(Control::Interrupt) | Err(mpsc::error::TryRecvError::Disconnected) => {
                let _ = event_tx.send(AgentEvent::RunEnd {
                    reason: RunEndReason::Interrupted,
                });
                return RunEndReason::Interrupted;
            }
            Err(mpsc::error::TryRecvError::Empty) => {}
        }

        if response.tool_calls.is_empty() {
            let _ = event_tx.send(AgentEvent::RunEnd {
                reason: RunEndReason::Completed,
            });
            return RunEndReason::Completed;
        }

        // 工具执行
        let results = registry
            .execute_all(
                response.tool_calls.clone(),
                ExecOptions {
                    concurrency: config.tool_concurrency,
                    timeout: config.tool_timeout,
                },
                event_tx,
                cancel,
            )
            .await;

        // 工具跑一半被打断：先把结果收尾，再走下面的中断分支
        if cancel.is_cancelled() {
            for (call, res) in results {
                push_tool_result(conversation, call, res);
            }
            let _ = event_tx.send(AgentEvent::RunEnd {
                reason: RunEndReason::Interrupted,
            });
            return RunEndReason::Interrupted;
        }

        for (call, res) in results {
            push_tool_result(conversation, call, res);
        }
    }

    let _ = event_tx.send(AgentEvent::RunEnd {
        reason: RunEndReason::MaxSteps {
            steps: config.max_steps,
        },
    });
    RunEndReason::MaxSteps {
        steps: config.max_steps,
    }
}

/// 停轮原因到用户可读文本。
pub fn describe_stop(reason: &RunEndReason) -> String {
    match reason {
        RunEndReason::Completed => "完成".into(),
        RunEndReason::MaxSteps { steps } => format!("达到最大轮数 {steps}，已停止"),
        RunEndReason::ContextExhausted => "上下文预算耗尽".into(),
        RunEndReason::Interrupted => "已中断".into(),
        RunEndReason::Error { message } => format!("出错: {message}"),
    }
}

/// 让 `StopReason` 在 UI 里有稳定文案。
pub fn describe_stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "完成",
        StopReason::ToolUse => "调用工具",
        StopReason::MaxTokens => "达到长度上限",
        StopReason::Refusal => "模型拒答",
    }
}
