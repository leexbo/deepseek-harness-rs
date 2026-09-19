//! 锁中毒的显式恢复策略。
//!
//! 标准库互斥锁中毒 = 持锁线程在临界区内 panic;对 GUI 宿主进程而言,
//! 单个后台任务的 panic 不应连坐整个进程(状态完整性由结构自身校验兜底,
//! 如 [`crate::EventLog`] 的 seq 连续性守卫,不靠进程崩溃)。因此获取锁
//! 一律恢复式:中毒即取回原值继续。全仓禁止 `.lock().expect("锁中毒")`。
//!
//! 用法:`use crate::lock::{LockRecover as _, RwLockRecover as _};` 后以
//! `g.lock_recover()` 或 `g.read_recover()` / `g.write_recover()` 取守卫。

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// [`Mutex`] 的恢复式获取:中毒时取回原值,不 panic。
pub trait LockRecover {
    /// 被守护的值类型。
    type Guarded: ?Sized;

    /// 恢复式 `Mutex::lock`。
    fn lock_recover(&self) -> MutexGuard<'_, Self::Guarded>;
}

impl<T: ?Sized> LockRecover for Mutex<T> {
    type Guarded = T;

    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// [`RwLock`] 的恢复式获取:中毒时取回原值,不 panic。
pub trait RwLockRecover {
    /// 被守护的值类型。
    type Guarded: ?Sized;

    /// 恢复式 `RwLock::read`。
    fn read_recover(&self) -> RwLockReadGuard<'_, Self::Guarded>;
    /// 恢复式 `RwLock::write`。
    fn write_recover(&self) -> RwLockWriteGuard<'_, Self::Guarded>;
}

impl<T: ?Sized> RwLockRecover for RwLock<T> {
    type Guarded = T;

    fn read_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|p| p.into_inner())
    }
}
