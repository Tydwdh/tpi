//! Run budgets 与 watchdog（文档 §12.4）。
//!
//! 预算由 watchdog 实时检查，wall time 到达时主动取消，不是等下一次工具调用才发现；
//! 接近预算时状态栏提示；达到硬限制后产生明确完成原因并保留可恢复 session。

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::LimitsConfig;

/// 接近预算的提示阈值（剩余 10%）。
const WARN_REMAINING_RATIO: f64 = 0.1;

/// watchdog 到期原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetEnd {
    WallTimeExceeded,
    None,
}

/// 启动 wall-clock watchdog（§12.4：到达 deadline 主动取消）。
pub fn spawn_watchdog(
    limits: &LimitsConfig,
    cancel: CancellationToken,
    on_warn: impl Fn() + Send + 'static,
) -> (tokio::task::JoinHandle<BudgetEnd>, Duration) {
    let wall = Duration::from_secs(limits.max_wall_time_minutes.max(1) * 60);
    spawn_watchdog_with_wall(wall, cancel, on_warn)
}

/// 可测版本：显式 wall duration。
pub fn spawn_watchdog_with_wall(
    wall: Duration,
    cancel: CancellationToken,
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
        // 硬限制：主动取消。
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                cancel.cancel();
                BudgetEnd::WallTimeExceeded
            }
            _ = cancel.cancelled() => BudgetEnd::None,
        }
    });
    (handle, wall)
}
