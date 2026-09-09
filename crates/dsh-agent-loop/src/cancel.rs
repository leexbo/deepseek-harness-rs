//! 合作取消令牌(WIT `dsh:host/cancel` 的 native 形态)。
//!
//! 语义(wasmCloud host/cancel 模式):
//! - 软取消是**合作式**的——持有者在安全点检查(引擎:step 边界、
//!   工具执行前后;工具:执行中 select);
//! - 硬停兜底是 epoch/fuel(E4,见 engine.rs 的竞技场销毁),不在此层;
//! - WASI 0.3 cancellation 发布后评估替换。
//!
//! 令牌可克隆共享(REPL Ctrl-C 处理器、网关 cancel 方法、engine、
//! 工具各持一份);`reset` 支持同一令牌跨 turn 复用(REPL 每回合复位)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// 合作取消令牌
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl CancelToken {
    /// 新令牌(未取消)
    pub fn new() -> Self {
        Self::default()
    }

    /// 触发取消:置位并唤醒全部等待者
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// 是否已取消
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// 复位(下一 turn 复用同一令牌;REPL 每回合开始时调用)
    pub fn reset(&self) {
        self.inner.cancelled.store(false, Ordering::SeqCst);
    }

    /// 等待取消(用于 `tokio::select!`;已取消则立即完成)
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            // notify_waiters 只唤醒注册后的等待者:循环检查防丢失
            let notified = self.inner.notify.notified();
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

    #[tokio::test]
    async fn cancel_wakes_waiters_and_reset_reuses() {
        let token = CancelToken::new();
        let waiter = {
            let t = token.clone();
            tokio::spawn(async move {
                t.cancelled().await;
                "woke".to_string()
            })
        };
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
        assert_eq!(waiter.await.unwrap(), "woke");

        // 已取消的令牌:cancelled() 立即完成
        token.cancelled().await;

        // 复位后可复用
        token.reset();
        assert!(!token.is_cancelled());
    }
}
