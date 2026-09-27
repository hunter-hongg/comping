//! 工具注册表与并行执行器。
//!
//! 权限裁决在这里统一完成：工具本身只管执行，不管审批。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{StreamExt, stream::FuturesUnordered};

use crate::cancel::Cancel;
use crate::event::{AgentEvent, ToolOutcome};
use crate::message::{ToolCall, ToolSpec};
use crate::tool::{Decision, Tool};

/// 权限等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// 只读，自动执行。
    Safe,
    /// 写文件 / 执行命令，需要确认。
    Confirm,
    /// 永不执行。
    Deny,
}

/// 工具注册表。
#[derive(Clone, Default)]
pub struct Registry {
    tools: HashMap<String, Arc<dyn Tool>>,
    risk: HashMap<String, Risk>,
    /// 本轮内被用户「全部允许」的工具名。
    session_allowed: Arc<Mutex<HashSet<String>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>, risk: Risk) {
        self.risk.insert(tool.name().to_string(), risk);
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn risk_of(&self, name: &str) -> Risk {
        *self.risk.get(name).unwrap_or(&Risk::Confirm)
    }

    /// 覆盖等级，配置里的持久化 allowlist 会用到。
    pub fn set_risk(&mut self, name: &str, risk: Risk) {
        self.risk.insert(name.to_string(), risk);
    }

    /// 暴露给模型的工具清单（Deny 的不出现在这里）。
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut v: Vec<_> = self
            .tools
            .values()
            .filter(|t| self.risk_of(t.name()) != Risk::Deny)
            .map(|t| ToolSpec::new(t.name(), t.description(), t.parameters()))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn allow_session(&self, name: &str) {
        if let Ok(mut s) = self.session_allowed.lock() {
            s.insert(name.to_string());
        }
    }

    /// 一次用户请求开始时调用，清空上轮的临时放行。
    pub fn reset_session(&self) {
        if let Ok(mut s) = self.session_allowed.lock() {
            s.clear();
        }
    }

    fn is_session_allowed(&self, name: &str) -> bool {
        self.session_allowed
            .lock()
            .map(|s| s.contains(name))
            .unwrap_or(false)
    }

    /// 并行执行一批调用，按调用顺序返回结果。
    ///
    /// 三条硬约束：
    ///
    /// 1. **每个调用都必须有结果**，哪怕是错误。丢掉任何一个，
    ///    下一轮请求里就会出现"有 tool_call 没有 tool_result"，
    ///    Anthropic / OpenAI 会直接 400。
    /// 2. **超时兜底**：`limit` 之外再套一层 `timeout`，
    ///    防止某个工具的实现挂死把整轮 agent 拖住。
    /// 3. **可取消**：`cancel` 一触发就停止启动新工具，
    ///    已经在跑的等它自己结束（工具内部通常带 `kill_on_drop`），
    ///    没跑到的立刻返回"已取消"。
    pub async fn execute_all(
        &self,
        calls: Vec<ToolCall>,
        opts: ExecOptions,
        tx: &tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        cancel: &Cancel,
    ) -> Vec<(ToolCall, Result<ToolOutcome, String>)> {
        let sem = Arc::new(tokio::sync::Semaphore::new(opts.concurrency.max(1)));
        type Fut = std::pin::Pin<
            Box<
                dyn std::future::Future<Output = (usize, ToolCall, Result<ToolOutcome, String>, u64)>
                    + Send,
            >,
        >;
        let mut futs = FuturesUnordered::<Fut>::new();
        // 留一份原始调用：取消时要用它给没跑到的调用补结果，
        // 补出来的必须带真实 id，否则对话里的 tool_result 对不上 tool_call
        let all_calls = calls.clone();
        let total = all_calls.len();

        for (idx, call) in calls.into_iter().enumerate() {
            // 已经取消了就不再启动新工具，直接给结果
            if cancel.is_cancelled() {
                futs.push(Box::pin(std::future::ready((
                    idx,
                    call,
                    Err(CANCELLED.to_string()),
                    0,
                ))));
                continue;
            }

            let Some(tool) = self.get(&call.name) else {
                let msg = format!("未知工具: {}", call.name);
                futs.push(Box::pin(std::future::ready((idx, call, Err(msg), 0))));
                continue;
            };

            // 工具自己的上限与全局上限取小的那个
            let budget = match (tool.max_runtime(), opts.timeout) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, b) => b,
            };

            let risk = self.risk_of(&call.name);
            let pre_allowed = self.is_session_allowed(&call.name);
            let sem = sem.clone();
            let tx = tx.clone();
            let registry = self.clone();
            let cancel = cancel.clone();

            futs.push(Box::pin(async move {
                let started = Instant::now();
                let _permit = sem.acquire_owned().await.ok();

                // 拿到许可后可能已经被取消了
                if cancel.is_cancelled() {
                    return (idx, call, Err(CANCELLED.to_string()), 0);
                }

                // 权限裁决
                let decision = match risk {
                    Risk::Deny => Decision::Deny,
                    Risk::Safe => Decision::NotRequired,
                    Risk::Confirm if pre_allowed => Decision::AllowSession,
                    Risk::Confirm => {
                        // 批准本身也要能被取消，否则用户按 Esc 后
                        // 工具会一直等一个没人会来的回答
                        let d = registry.ask(&tx, &tool, &call, &cancel).await;
                        if d == Decision::AllowSession {
                            registry.allow_session(tool.name());
                        }
                        d
                    }
                };

                if !decision.allowed() {
                    let name = call.name.clone();
                    let reason = if risk == Risk::Deny {
                        format!("工具 {name} 被策略禁用，请改用其他方式完成。")
                    } else {
                        format!("{name} 未获批准。{}", decision.denial_reason().unwrap_or(""))
                    };
                    return (idx, call, Err(reason), 0);
                }

                let _ = tx.send(AgentEvent::ToolCallStarted {
                    call: call.clone(),
                });

                // 超时兜底：到点就 drop 掉 future，
                // 工具内部的 kill_on_drop 会连带杀掉子进程
                let res = match budget {
                    None => tool.call(call.arguments.clone()).await.map_err(|e| e.to_string()),
                    Some(d) => match tokio::time::timeout(d, tool.call(call.arguments.clone())).await
                    {
                        Ok(r) => r.map_err(|e| e.to_string()),
                        Err(_) => Err(format!(
                            "{} 执行超过 {} 秒被强制终止。如确实需要更久，请调高配置里的超时。",
                            call.name,
                            d.as_secs()
                        )),
                    },
                };

                let elapsed = started.elapsed().as_millis() as u64;
                (idx, call, res, elapsed)
            }));
        }

        let mut collected = Vec::with_capacity(total);
        let mut done: Vec<bool> = vec![false; total];
        loop {
            // 关键：取消必须能打断"正在等某个工具跑完"这件事。
            // 写成 `while let Some(x) = futs.next().await` 的话，
            // 一个跑 300 秒的命令会把取消完全挡住——循环体一次都转不到。
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                it = futs.next() => it,
            };
            // None = 被取消，或队列已空；两种情况都该退出循环
            let Some((idx, call, res, elapsed)) = next else {
                break;
            };
            // 失败也要发事件：UI 是靠这个把"执行中…"换成结果的。
            // 以前只在 Ok 时发，导致每个失败的工具都在界面上留一条
            // 永远转不完的幽灵条目（工具报错在实践中很常见）。
            let outcome = match &res {
                Ok(out) => out.clone(),
                Err(e) => ToolOutcome::err(e.clone()),
            };
            let _ = tx.send(AgentEvent::ToolCallFinished {
                call: call.clone(),
                outcome,
                elapsed_ms: elapsed,
            });
            if idx < done.len() {
                done[idx] = true;
            }
            collected.push((idx, call, res, elapsed));
        }

        // 补齐没跑完的调用。必须用原始 call 带上真实 id，
        // 否则下一轮请求里 tool_result 找不到对应的 tool_call。
        let mut out = collected;
        for (idx, finished) in done.iter().enumerate() {
            if !finished {
                out.push((idx, all_calls[idx].clone(), Err(CANCELLED.to_string()), 0));
            }
        }

        out.sort_by_key(|(idx, _, _, _)| *idx);
        out.into_iter()
            .map(|(_, call, res, _)| (call, res))
            .collect()
    }

    /// 推送批准请求并等待 UI 回应。
    ///
    /// 等待期间保持对 `cancel` 的敏感：用户按 Esc 之后，
    /// 不该还有一个工具永远悬着等一个没人会来的批准。
    async fn ask(
        &self,
        tx: &tokio::sync::mpsc::UnboundedSender<AgentEvent>,
        tool: &Arc<dyn Tool>,
        call: &ToolCall,
        cancel: &Cancel,
    ) -> Decision {
        let Some(pending) = tool.request_approval(&call.arguments).await else {
            return Decision::NotRequired;
        };

        // 用 mpsc 而不是 oneshot：oneshot 的 Sender 被 drop 就等于"没答过"，
        // 无法区分"UI 换了实现"和"用户明确拒了"。这里靠 recv() 返回 None
        // 统一按拒绝处理，语义更明确。
        let (respond, mut wait) = tokio::sync::mpsc::channel::<Decision>(1);
        let title = format!("{} · {}", tool.name(), pending.title);

        if tx
            .send(AgentEvent::ApprovalNeeded {
                call: call.clone(),
                title,
                body: pending.body,
                respond,
            })
            .is_err()
        {
            // UI 已经走了，只能拒绝
            return Decision::Deny;
        }

        tokio::select! {
            biased;
            // 先判取消：用户刚按的 Esc 不该输给一个已经排队的批准
            _ = cancel.cancelled() => Decision::Deny,
            d = wait.recv() => d.unwrap_or(Decision::Deny),
        }
    }
}

/// 执行一批工具时的参数。
#[derive(Debug, Clone, Copy)]
pub struct ExecOptions {
    /// 并发上限
    pub concurrency: usize,
    /// 单个工具的超时上限。`None` 表示不限时。
    pub timeout: Option<Duration>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// 未执行（被取消）的调用回给模型的话。
pub const CANCELLED: &str = "用户中断了这次请求，该工具没有执行。";
