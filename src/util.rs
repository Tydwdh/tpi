//! 通用错误处理辅助（§错误处理纪律：生产代码不允许 unwrap/expect/panic）。
//!
//! 唯一的例外是 `unreachable` 语义的 match 分支——用显式分支消除，
//! 不保留 `unreachable!()` 宏。所有不可恢复的错误都记录日志并降级。

use std::sync::{Mutex, MutexGuard};

/// 获取互斥锁；遇 poison（持锁线程 panic 后遗留）时记录告警并恢复，
/// 不 panic、不丢弃数据（`PoisonError::into_inner` 取回内部数据）。
///
/// `what` 用于日志标识（如 `"current_plan"`）。
pub fn lock_mutex<'a, T>(mutex: &'a Mutex<T>, what: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(what, "mutex poisoned; recovering guard");
            poisoned.into_inner()
        }
    }
}
