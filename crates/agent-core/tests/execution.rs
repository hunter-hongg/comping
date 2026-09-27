//! 工具执行器的并发、超时与取消。
//!
//! 这些是 agent 循环最容易悄悄坏掉的地方：
//! - 并行工具同时要批准时，批准通道不能互相顶掉
//! - 工具挂死时必须有兜底
//! - 取消后**每个** tool_call 都必须留下 tool_result，
//!   否则下一轮请求会被 API 以 400 拒掉

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agent_core::{
    AgentConfig, Cancel, Decision, ExecOptions, PendingApproval, Permission, Registry,
    Risk, Tool, ToolError, ToolOutcome,
};
use serde_json::{Value, json};

/// 行为可配的测试工具。
struct TestTool {
    name: &'static str,
    /// 跑多久
    delay: Duration,
    /// 是否需要批准
    needs_approval: bool,
    /// 声明的上限
    max_runtime: Option<Duration>,
    /// 已经开始执行的次数
    started: Arc<AtomicUsize>,
}

impl TestTool {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            delay: Duration::ZERO,
            needs_approval: false,
            max_runtime: None,
            started: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn slow(ms: u64) -> Self {
        Self {
            delay: Duration::from_millis(ms),
            ..Self::new("slow")
        }
    }

    fn gated(name: &'static str) -> Self {
        Self {
            needs_approval: true,
            ..Self::new(name)
        }
    }

    /// 跑 5 秒但自称只要 50ms
    fn capped(d: Duration) -> Self {
        Self {
            delay: Duration::from_secs(5),
            max_runtime: Some(d),
            ..Self::new("capped")
        }
    }

    fn with_counter(name: &'static str) -> (Self, Arc<AtomicUsize>) {
        let c = Arc::new(AtomicUsize::new(0));
        (
            Self {
                started: c.clone(),
                ..Self::new(name)
            },
            c,
        )
    }
}

#[async_trait::async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "测试用工具"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn preview(&self, _args: &Value) -> String {
        self.name.to_string()
    }

    fn max_runtime(&self) -> Option<Duration> {
        self.max_runtime
    }

    async fn request_approval(&self, _args: &Value) -> Option<PendingApproval> {
        self.needs_approval.then(|| PendingApproval {
            title: format!("执行 {}", self.name),
            body: "要执行吗".into(),
        })
    }

    async fn call(&self, _args: Value) -> Result<ToolOutcome, ToolError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(ToolOutcome::ok("done", "ok"))
    }
}

fn call(id: &str, name: &str) -> agent_core::ToolCall {
    agent_core::ToolCall::new(id, name, json!({}))
}

fn registry_with(entries: Vec<(Arc<dyn Tool>, Risk)>) -> Registry {
    let mut r = Registry::new();
    for (t, risk) in entries {
        r.register(t, risk);
    }
    r
}

fn opts(concurrency: usize) -> ExecOptions {
    ExecOptions {
        concurrency,
        timeout: Some(Duration::from_secs(10)),
    }
}

fn chan() -> (
    tokio::sync::mpsc::UnboundedSender<agent_core::AgentEvent>,
    tokio::sync::mpsc::UnboundedReceiver<agent_core::AgentEvent>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

fn count_approvals(rx: &mut tokio::sync::mpsc::UnboundedReceiver<agent_core::AgentEvent>) -> usize {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, agent_core::AgentEvent::ApprovalNeeded { .. }) {
            n += 1;
        }
    }
    n
}

// ================================================================ 超时

#[tokio::test]
async fn 工具超时会被终止并报错() {
    let reg = registry_with(vec![(Arc::new(TestTool::slow(5_000)), Risk::Safe)]);
    let (tx, mut rx) = chan();
    let cancel = Cancel::new();

    let t0 = Instant::now();
    let out = reg
        .execute_all(
            vec![call("c1", "slow")],
            ExecOptions {
                concurrency: 1,
                timeout: Some(Duration::from_millis(100)),
            },
            &tx,
            &cancel,
        )
        .await;
    let elapsed = t0.elapsed();

    assert_eq!(out.len(), 1);
    let err = out[0].1.as_ref().unwrap_err();
    assert!(err.contains("超时") || err.contains("秒"), "实际: {err}");
    assert!(
        elapsed < Duration::from_secs(2),
        "应该在超时点就返回，实际花了 {elapsed:?}"
    );
    let _ = rx.try_recv();
}

#[tokio::test]
async fn 工具没超时就正常完成() {
    let reg = registry_with(vec![(Arc::new(TestTool::slow(20)), Risk::Safe)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let out = reg
        .execute_all(
            vec![call("c1", "slow")],
            ExecOptions {
                concurrency: 1,
                timeout: Some(Duration::from_secs(30)),
            },
            &tx,
            &cancel,
        )
        .await;

    assert!(out[0].1.is_ok());
}

#[tokio::test]
async fn 工具自带上限比全局更短时取更短() {
    // 工具声明 50ms，全局 30s → 应按 50ms 算
    let reg = registry_with(vec![(Arc::new(TestTool::capped(Duration::from_millis(50))), Risk::Safe)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let t0 = Instant::now();
    let out = reg
        .execute_all(
            vec![call("c1", "capped")],
            ExecOptions {
                concurrency: 1,
                timeout: Some(Duration::from_secs(30)),
            },
            &tx,
            &cancel,
        )
        .await;
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(out[0].1.is_err());
}

#[tokio::test]
async fn 工具自带上限比全局更长时仍受全局约束() {
    // 工具想跑 30s，全局 100ms → 应按 100ms 算
    let reg = registry_with(vec![(
        Arc::new(TestTool {
            name: "stubborn",
            delay: Duration::from_secs(30),
            max_runtime: Some(Duration::from_secs(30)),
            needs_approval: false,
            started: Arc::new(AtomicUsize::new(0)),
        }),
        Risk::Safe,
    )]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let t0 = Instant::now();
    let out = reg
        .execute_all(
            vec![call("c1", "stubborn")],
            ExecOptions {
                concurrency: 1,
                timeout: Some(Duration::from_millis(100)),
            },
            &tx,
            &cancel,
        )
        .await;
    assert!(t0.elapsed() < Duration::from_secs(2), "全局上限必须生效");
    assert!(out[0].1.is_err());
}

#[tokio::test]
async fn 零并发参数不会死锁() {
    let reg = registry_with(vec![(Arc::new(TestTool::slow(10)), Risk::Safe)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();
    let out = reg
        .execute_all(vec![call("c1", "slow")], opts(0), &tx, &cancel)
        .await;
    assert_eq!(out.len(), 1, "concurrency=0 应当被兜底成 1");
    assert!(out[0].1.is_ok());
}

// ================================================================ 取消

/// 回归：以前收集循环是 `while let Some(x) = futs.next().await`，
/// 取消只在两次完成之间才被检查到，于是**一个跑很久的工具会把取消完全挡住**。
/// 循环体一次都转不到，agent 循环会一直等下去。
#[tokio::test]
async fn 取消能打断正在跑的工具() {
    let reg = registry_with(vec![(Arc::new(TestTool::slow(30_000)), Risk::Safe)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let handle = {
        let reg = reg.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            reg.execute_all(vec![call("c1", "slow")], opts(2), &tx, &cancel)
                .await
        })
    };

    // 让工具真的开始跑
    tokio::time::sleep(Duration::from_millis(80)).await;
    cancel.cancel();

    // 关键：不能等 30 秒
    let out = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("取消应当立刻打断正在执行的工具，而不是等它跑完")
        .unwrap();

    assert_eq!(out.len(), 1, "被取消的调用也必须回一条结果");
    assert!(out[0].1.is_err());
}

#[tokio::test]
async fn 取消后不再启动新工具() {
    let (tool, counter) = TestTool::with_counter("slow");
    let reg = registry_with(vec![(Arc::new(tool), Risk::Safe)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();
    cancel.cancel(); // 事先就取消

    let calls: Vec<_> = (0..5).map(|i| call(&format!("c{i}"), "slow")).collect();
    let out = reg.execute_all(calls, opts(4), &tx, &cancel).await;

    assert_eq!(counter.load(Ordering::SeqCst), 0, "取消后不该有工具启动");
    // 但每个调用都还是要有结果
    assert_eq!(out.len(), 5);
    for (c, r) in &out {
        assert!(c.id.starts_with('c'), "补的结果必须带真实 id，实际 {:?}", c.id);
        assert!(r.as_ref().unwrap_err().contains("中断"));
    }
}

#[tokio::test]
async fn 取消时正在跑的工具结果仍然返回() {
    let reg = registry_with(vec![(Arc::new(TestTool::slow(20)), Risk::Safe)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let handle = {
        let reg = reg.clone();
        let cancel = cancel.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            reg.execute_all(vec![call("c1", "slow")], opts(2), &tx, &cancel)
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(80)).await;
    cancel.cancel();

    let out = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("取消后不该挂住")
        .unwrap();
    assert_eq!(out.len(), 1);
    assert!(
        out[0].1.is_ok() || out[0].1.as_ref().unwrap_err().contains("中断"),
        "要么跑完了，要么明确说被取消：{:?}",
        out[0].1
    );
}

#[tokio::test]
async fn 取消后每个调用都有结果() {
    // 5 个调用，其中 3 个是慢的；取消后必须还是 5 条结果
    let mut reg = Registry::new();
    reg.register(Arc::new(TestTool::slow(300)), Risk::Safe);
    for i in 0..3 {
        reg.register(
            Arc::new(TestTool {
                name: leak_name(i),
                delay: Duration::from_millis(10),
                needs_approval: false,
                max_runtime: None,
                started: Arc::new(AtomicUsize::new(0)),
            }),
            Risk::Safe,
        );
    }
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let handle = {
        let reg = reg.clone();
        let cancel = cancel.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let calls: Vec<_> = (0..3)
                .map(|i| call(&format!("fast{i}"), leak_name(i)))
                .chain(std::iter::once(call("slow", "slow")))
                .collect();
            reg.execute_all(calls, opts(4), &tx, &cancel).await
        })
    };

    tokio::time::sleep(Duration::from_millis(60)).await;
    cancel.cancel();

    let out = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("不该挂住")
        .unwrap();

    assert_eq!(out.len(), 4, "4 个调用必须回 4 条结果");
    let ids: Vec<&str> = out.iter().map(|(c, _)| c.id.as_str()).collect();
    for want in ["fast0", "fast1", "fast2", "slow"] {
        assert!(ids.contains(&want), "缺少 {want} 的结果，实际 {ids:?}");
    }
}

/// `name` 是 `&'static str`，这里现编一个。
fn leak_name(i: usize) -> &'static str {
    ["t0", "t1", "t2"][i]
}

// ================================================================ 批准

#[tokio::test]
async fn 只读工具不触发批准() {
    let reg = registry_with(vec![(Arc::new(TestTool::new("ro")), Risk::Safe)]);
    let (tx, mut rx) = chan();
    let cancel = Cancel::new();

    let out = reg
        .execute_all(vec![call("c1", "ro")], opts(2), &tx, &cancel)
        .await;

    assert!(out[0].1.is_ok());
    assert_eq!(count_approvals(&mut rx), 0, "只读工具不该弹批准");
}

#[tokio::test]
async fn 并行批准会被连发多次() {
    // 三个都要批准的并行工具
    let mut reg = Registry::new();
    for n in ["a", "b", "c"] {
        reg.register(Arc::new(TestTool::gated(n)), Risk::Confirm);
    }
    let (tx, mut rx) = chan();
    let cancel = Cancel::new();

    let handle = {
        let reg = reg.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            reg.execute_all(
                vec![call("c1", "a"), call("c2", "b"), call("c3", "c")],
                opts(3),
                &tx,
                &cancel,
            )
            .await
        })
    };

    // 等三个批准请求都到齐
    let mut seen = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while seen < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        while let Ok(ev) = rx.try_recv() {
            if let agent_core::AgentEvent::ApprovalNeeded { respond, .. } = ev {
                seen += 1;
                let _ = respond.try_send(Decision::Allow);
            }
        }
    }

    let out = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("不该挂住")
        .unwrap();

    assert_eq!(seen, 3, "三个并行工具都该发出批准请求");
    assert_eq!(out.len(), 3);
    // 全部放行 → 全部成功
    for (c, r) in &out {
        assert!(r.is_ok(), "{} 应该执行成功，实际 {r:?}", c.id);
    }
}

#[tokio::test]
async fn 拒绝时结果里带拒绝原因() {
    let reg = registry_with(vec![(Arc::new(TestTool::gated("a")), Risk::Confirm)]);
    let (tx, mut rx) = chan();
    let cancel = Cancel::new();

    let handle = {
        let reg = reg.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            reg.execute_all(vec![call("c1", "a")], opts(1), &tx, &cancel)
                .await
        })
    };

    // 等批准请求到了再拒
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match rx.try_recv() {
            Ok(agent_core::AgentEvent::ApprovalNeeded { respond, .. }) => {
                let _ = respond.try_send(Decision::Deny);
                break;
            }
            _ if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            _ => break,
        }
    }

    let out = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("不该挂住")
        .unwrap();

    let err = out[0].1.as_ref().unwrap_err();
    assert!(err.contains("拒绝"), "实际: {err}");
}

#[tokio::test]
async fn 批准期间被取消会立刻放行() {
    let reg = registry_with(vec![(Arc::new(TestTool::gated("a")), Risk::Confirm)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let handle = {
        let reg = reg.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            reg.execute_all(vec![call("c1", "a")], opts(1), &tx, &cancel)
                .await
        })
    };

    // 谁也不批准，只取消
    tokio::time::sleep(Duration::from_millis(60)).await;
    cancel.cancel();

    let out = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("取消后不该继续等批准")
        .unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].1.is_err(), "没批准就不该执行");
}

#[tokio::test]
async fn ui_提前消失时按拒绝处理() {
    let reg = registry_with(vec![(Arc::new(TestTool::gated("a")), Risk::Confirm)]);
    let (tx, rx) = chan();
    let cancel = Cancel::new();
    drop(rx); // 模拟 UI 退出

    let out = reg
        .execute_all(vec![call("c1", "a")], opts(1), &tx, &cancel)
        .await;

    assert_eq!(out.len(), 1);
    assert!(out[0].1.is_err(), "UI 没了就该按拒绝，不能放行");
}

#[tokio::test]
async fn 禁用的工具不执行() {
    let reg = registry_with(vec![(Arc::new(TestTool::new("a")), Risk::Deny)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let out = reg
        .execute_all(vec![call("c1", "a")], opts(1), &tx, &cancel)
        .await;

    assert!(out[0].1.as_ref().unwrap_err().contains("禁用"));
    // 被禁的工具也不该出现在给模型的工具清单里
    let names: Vec<String> = reg.specs().iter().map(|s| s.name.clone()).collect();
    assert!(!names.contains(&"a".to_string()));
}

#[tokio::test]
async fn 未知工具给明确错误() {
    let reg = Registry::new();
    let (tx, _rx) = chan();
    let cancel = Cancel::new();
    let out = reg
        .execute_all(vec![call("c1", "nonexistent")], opts(1), &tx, &cancel)
        .await;
    assert!(out[0].1.as_ref().unwrap_err().contains("未知工具"));
}

#[tokio::test]
async fn 并发上限真的生效() {
    let reg = registry_with(vec![(Arc::new(TestTool::slow(120)), Risk::Safe)]);
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let t0 = Instant::now();
    let calls: Vec<_> = (0..6).map(|i| call(&format!("c{i}"), "slow")).collect();
    let out = reg.execute_all(calls, opts(2), &tx, &cancel).await;
    let elapsed = t0.elapsed();

    assert_eq!(out.len(), 6);
    // 6 个 120ms 的任务、并发 2 → 至少 3 轮 ≈ 360ms
    assert!(
        elapsed >= Duration::from_millis(300),
        "并发 2 跑 6 个 120ms 的任务不该快于 300ms，实际 {elapsed:?}"
    );
}

#[tokio::test]
async fn 结果按调用顺序返回() {
    // 故意让后面的先跑完
    let mut reg = Registry::new();
    reg.register(Arc::new(TestTool::slow(150)), Risk::Safe);
    for (i, n) in ["p", "q", "r"].iter().enumerate() {
        reg.register(
            Arc::new(TestTool {
                name: n,
                delay: Duration::from_millis(10 * (i as u64 + 1)),
                needs_approval: false,
                max_runtime: None,
                started: Arc::new(AtomicUsize::new(0)),
            }),
            Risk::Safe,
        );
    }
    let (tx, _rx) = chan();
    let cancel = Cancel::new();

    let calls = vec![call("c0", "slow"), call("c1", "p"), call("c2", "q"), call("c3", "r")];
    let out = reg.execute_all(calls, opts(4), &tx, &cancel).await;

    let ids: Vec<&str> = out.iter().map(|(c, _)| c.id.as_str()).collect();
    assert_eq!(ids, vec!["c0", "c1", "c2", "c3"], "结果顺序必须稳定");
}

// ============================================================ Decision

#[test]
fn decision_的允许判定() {
    assert!(Decision::NotRequired.allowed());
    assert!(Decision::Allow.allowed());
    assert!(Decision::AllowSession.allowed());
    assert!(!Decision::Deny.allowed());
    assert!(Decision::Deny.denial_reason().is_some());
    assert!(Decision::Allow.denial_reason().is_none());
}

/// 旧的 `Permission` 只剩给事件层用，两者不能混。
#[test]
fn permission_与_decision_不混用() {
    let p = Permission::Allow;
    let d = Decision::Allow;
    // 编译期就能区分：Decision 没有 as_str
    assert_eq!(p, Permission::Allow);
    assert_eq!(d, Decision::Allow);
}

#[test]
fn 默认执行参数有超时兜底() {
    let o = ExecOptions::default();
    assert!(o.timeout.is_some(), "默认必须有超时，否则挂死的工具会拖住整个循环");
    assert!(o.concurrency >= 1);
}

#[test]
fn agent_config_默认有工具超时() {
    let c = AgentConfig::default();
    assert!(c.tool_timeout.is_some());
}
