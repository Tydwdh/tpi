//! Run budgets 与 watchdog（文档 §12.4）。
//!
//! 预算由 watchdog 实时检查，wall time 到达时主动取消，不是等下一次工具调用才发现；
//! 接近预算时状态栏提示；达到硬限制后产生明确完成原因并保留可恢复 session。

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::LimitsConfig;

/// 接近预算的提示阈值（剩余 10%）。
const WARN_REMAINING_RATIO: f64 = 0.1;

/// §16：取消来源 → run 结束原因（watchdog 超时 ≠ 用户取消）。
pub fn cancel_reason_for_cause(cause: u8) -> crate::session::CompletionReason {
    if cause == CANCEL_CAUSE_WALL_TIME {
        crate::session::CompletionReason::WallTimeExceeded
    } else {
        crate::session::CompletionReason::Cancelled
    }
}
/// watchdog 到期原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetEnd {
    WallTimeExceeded,
    None,
}

/// 取消来源（§16：必须区分用户取消与系统预算超时，否则 UI/session 会把
/// 系统超时显示成用户取消）。
pub const CANCEL_CAUSE_USER: u8 = 0;
pub const CANCEL_CAUSE_WALL_TIME: u8 = 1;

/// 启动 wall-clock watchdog（§12.4：到达 deadline 主动取消）。
///
/// `on_deadline` 在硬限制取消前调用（§16：写入取消来源，区分用户取消与超时）。
pub fn spawn_watchdog(
    limits: &LimitsConfig,
    cancel: CancellationToken,
    on_deadline: impl Fn() + Send + 'static,
    on_warn: impl Fn() + Send + 'static,
) -> (tokio::task::JoinHandle<BudgetEnd>, Duration) {
    let wall = Duration::from_secs(limits.max_wall_time_minutes.max(1) * 60);
    spawn_watchdog_with_wall(wall, cancel, on_deadline, on_warn)
}

/// 可测版本：显式 wall duration。
pub fn spawn_watchdog_with_wall(
    wall: Duration,
    cancel: CancellationToken,
    on_deadline: impl Fn() + Send + 'static,
    on_warn: impl Fn() + Send + 'static,
) -> (tokio::task::JoinHandle<BudgetEnd>, Duration) {
    let deadline = tokio::time::Instant::now() + wall;
    let warn_at =
        deadline - Duration::from_secs((wall.as_secs() as f64 * WARN_REMAINING_RATIO) as u64);

    let handle = tokio::spawn(async move {
        // 接近预算时提示（状态栏由调用方渲染）。
        tokio::select! {
            _ = tokio::time::sleep_until(warn_at) => {
                on_warn();
            }
            _ = cancel.cancelled() => return BudgetEnd::None,
        }
        // 硬限制：先标记取消来源，再主动取消（§16）。
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                on_deadline();
                cancel.cancel();
                BudgetEnd::WallTimeExceeded
            }
            _ = cancel.cancelled() => BudgetEnd::None,
        }
    });
    (handle, wall)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::CompletionReason;

    /// §16：wall-time 来源映射为 WallTimeExceeded，用户来源映射为 Cancelled。
    #[test]
    fn cancel_reason_distinguishes_wall_time_from_user() {
        assert_eq!(
            cancel_reason_for_cause(CANCEL_CAUSE_WALL_TIME),
            CompletionReason::WallTimeExceeded
        );
        assert_eq!(
            cancel_reason_for_cause(CANCEL_CAUSE_USER),
            CompletionReason::Cancelled
        );
    }

    /// §16：watchdog 硬限制到期时先写取消来源，再取消 token（时序保证）。
    #[tokio::test]
    async fn on_deadline_sets_cause_before_cancel() {
        let cancel = CancellationToken::new();
        let cause = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(CANCEL_CAUSE_USER));
        let cause_for_watchdog = cause.clone();
        let (watchdog, _) = spawn_watchdog_with_wall(
            std::time::Duration::from_millis(100),
            cancel.clone(),
            move || {
                cause_for_watchdog
                    .store(CANCEL_CAUSE_WALL_TIME, std::sync::atomic::Ordering::SeqCst);
            },
            || {},
        );
        let end = watchdog.await.unwrap();
        assert_eq!(end, BudgetEnd::WallTimeExceeded);
        assert!(cancel.is_cancelled(), "watchdog 必须取消 token");
        assert_eq!(
            cause.load(std::sync::atomic::Ordering::SeqCst),
            CANCEL_CAUSE_WALL_TIME,
            "取消来源必须在取消前写入（§16）"
        );
    }
}
