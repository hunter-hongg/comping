//! 取消令牌。
//!
//! 存在的理由：工具并行执行时，用户按 Esc 应该能**立刻**停下手上的活，
//! 而不是等当前 step 跑完。agent 循环里没有"能 drop 掉一个 await 中
//! 的 future"的自然位置，需要一个能被多处观察的取消信号。
//!
//! 刻意不引入 `tokio_util::sync::CancellationToken`：就这二十行，
//! 而 agent-core 是最底层的 crate，少一个依赖少一份约束。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// 克隆即共享。取消是全局的、一次性的。
#[derive(Clone)]
pub struct Cancel(Arc<Inner>);

struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Default for Cancel {
    fn default() -> Self {
        Self(Arc::new(Inner {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }))
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求取消。可重复调用。
    pub fn cancel(&self) {
        // 先置位再通知：等待者醒来时一定能读到 true，
        // 避免"通知先到、标志后置"的竞态导致漏唤醒。
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// 等到被取消为止。已取消则立刻返回。
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.0.notify.notified();
            // 注册完监听再复查一次：避免在这两行之间发生取消，
            // 导致 notified() 永远挂着。
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn 初始未取消() {
        let c = Cancel::new();
        assert!(!c.is_cancelled());
    }

    #[test]
    fn 取消后可观察到() {
        let c = Cancel::new();
        c.cancel();
        assert!(c.is_cancelled());
        // 重复取消也无所谓
        c.cancel();
        assert!(c.is_cancelled());
    }

    #[test]
    fn 克隆共享同一个状态() {
        let a = Cancel::new();
        let b = a.clone();
        b.cancel();
        assert!(a.is_cancelled());
    }

    #[tokio::test]
    async fn 已取消时等待立即返回() {
        let c = Cancel::new();
        c.cancel();
        // 不该挂住
        tokio::time::timeout(Duration::from_millis(200), c.cancelled())
            .await
            .expect("已取消的令牌不该让等待者挂住");
    }

    #[tokio::test]
    async fn 等待者会被唤醒() {
        let c = Cancel::new();
        let w = c.clone();
        let handle = tokio::spawn(async move { w.cancelled().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        c.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("取消后等待者应当被唤醒")
            .unwrap();
    }

    #[tokio::test]
    async fn 多个等待者都被唤醒() {
        let c = Cancel::new();
        let handles: Vec<_> = (0..5)
            .map(|_| {
                let w = c.clone();
                tokio::spawn(async move { w.cancelled().await })
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(50)).await;
        c.cancel();

        for h in handles {
            tokio::time::timeout(Duration::from_secs(2), h)
                .await
                .expect("所有等待者都应被唤醒")
                .unwrap();
        }
    }
}
