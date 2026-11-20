//! 锁的小工具。
//!
//! 全项目有十几处 `mutex.lock()`，中毒之后怎么处理必须一致 —— 各写各的
//! 迟早会出现"这一处硬撑、那一处 panic"的分裂。

use std::sync::{Mutex, MutexGuard};

/// 取一把 std 锁；中毒就 panic。
///
/// 中毒意味着有线程在持锁期间挂了，锁保护的东西可能是半截状态（名册里
/// 一半有任务一半没有、音频缓冲里半段采样）。带着它继续干活会看到自相
/// 矛盾的数据，所以宁可响亮地失败。
///
/// 只有"丢了也无所谓"的数据才适合 `into_inner()` 硬撑 —— 比如日志缓冲
/// （见 `zex-app` 的 `ext_logs`）。
///
/// `what` 用来拼 panic 信息，例如 `lock_or_panic(&map, "子智能体表")`
/// 得到 `子智能体表中毒`。
pub fn lock_or_panic<'a, T>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(_) => panic!("{what}中毒"),
    }
}
