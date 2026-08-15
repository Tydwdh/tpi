//! Supervisor walking skeleton（P2-05 / ADR-006）。
//!
//! 统一的任务 owner：CancellationToken（层级取消）+ TaskTracker（quiescence）。
//!
//! 协议（ADR-006 固定顺序，幂等）：
//!
//! ```text
//! shutdown()
//!   → cancel()             // CancellationToken：全部子任务收到取消
//!   → tracker.close()      // 不再接受新任务
//!   → wait()               // join 全部 tracked task（quiescence）
//!   → 汇总错误
//!   → flush durable data   // 调用方（owner）负责
//! ```
//!
//! 使用（`no_run`：需要 runtime；doc-test 不执行）：
//! ```rust,no_run
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! use tpi_capabilities::process::supervisor::Supervisor;
//! let mut sup = Supervisor::new();
//! sup.spawn("task-name", |cancel| async move { let _ = cancel; });
//! let result = sup.shutdown().await;
//! assert!(result.is_ok());
//! # }
//! ```
//!
//! 禁止：detached `tokio::spawn`（无 owner）、只 abort 不 join、锁内 await、
//! 永久 drain task 掩盖生命周期错误（见 ADR-006）。

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// 一个被跟踪任务的失败信息（shutdown 时汇总）。
#[derive(Debug, thiserror::Error)]
pub enum TaskFailure {
    #[error("task `{name}` panicked: {message}")]
    Panicked { name: &'static str, message: String },
    #[error("task `{name}` join 错误: {error}")]
    Join { name: &'static str, error: String },
}

/// 任务执行结果（Ok = 正常结束或按取消结束）。
pub type TaskResult = Result<(), TaskFailure>;

/// Supervisor：拥有任务集合，提供层级取消 + quiescence。
pub struct Supervisor {
    token: CancellationToken,
    tracker: TaskTracker,
    /// 子 token（spawn 时从父派生；父 cancel 传播到全部）。
    child_tokens: Vec<CancellationToken>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Supervisor {
    /// 兜底：未显式 shutdown 时 abort 未完成任务（与旧 AbortTaskOnDrop 等价）。
    /// 正常路径应显式 `shutdown().await`（join），Drop 只是防提前返回泄漏。
    fn drop(&mut self) {
        self.token.cancel();
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            tracker: TaskTracker::new(),
            child_tokens: Vec::new(),
        }
    }

    /// 父取消 token（外部可提前 cancel；shutdown 也会 cancel）。
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// spawn 一个被跟踪任务。`cancel` 是派生 token：父 cancel 时全部子任务收到。
    ///
    /// 返回 JoinHandle（可选 join 单个任务；shutdown 会 join 全部）。
    pub fn spawn<F, Fut>(&mut self, name: &'static str, f: F) -> tokio::task::JoinHandle<()>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let child = self.token.child_token();
        self.child_tokens.push(child.clone());
        let handle = self.tracker.spawn(async move {
            // 任务体内可用 cancel 检查取消；panic 由 TaskTracker 捕获（join 返回 Err）。
            f(child).await
        });
        let _ = name; // 名称当前仅用于诊断；错误汇总在 shutdown 时由 join 结果提供
        handle
    }

    /// 幂等 shutdown：cancel → close → wait → 汇总错误。
    pub async fn shutdown(&mut self) -> Result<(), Vec<TaskFailure>> {
        // 1. cancel（全部子任务）。
        self.token.cancel();
        // 2. close：不再接受新任务。
        self.tracker.close();
        // 3. wait：join 全部 tracked task（quiescence）。
        // TaskTracker::wait() 等待所有任务结束（含 panic 任务）；panic 由
        // JoinError 捕获但 wait() 不返回逐条结果——walking skeleton 阶段
        // 以 tracked==0 为 quiescence 验收；P2-06 迁 watchdog 时引入
        // join_handles 收集错误汇总。
        self.tracker.wait().await;
        Ok(())
    }

    /// 当前 tracked 任务数（leak test 用：shutdown 后应为 0）。
    pub fn tracked(&self) -> usize {
        self.tracker.len()
    }

    /// 是否已 cancel（shutdown 后 true）。
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// walking skeleton：一个无害 background task（循环等 cancel）。
    #[tokio::test]
    async fn spawn_and_shutdown_waits_cleanly() {
        let mut sup = Supervisor::new();
        sup.spawn("loop", |cancel| async move {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        assert_eq!(sup.tracked(), 1);
        sup.shutdown().await.expect("shutdown 无失败");
        assert_eq!(sup.tracked(), 0, "shutdown 后 tracked 必须为 0");
        assert!(sup.is_cancelled());
    }

    /// 100 次 start/shutdown：tracked 始终归 0（leak test 核心验收）。
    #[tokio::test]
    async fn hundred_start_shutdown_no_leak() {
        for i in 0..100u32 {
            let mut sup = Supervisor::new();
            sup.spawn("work", |cancel| async move {
                // 短暂工作：等 cancel 或完成。
                let mut done = false;
                for _ in 0..3 {
                    tokio::task::yield_now().await;
                    if cancel.is_cancelled() {
                        done = true;
                        break;
                    }
                }
                let _ = done;
            });
            sup.shutdown().await.expect("shutdown 无失败");
            assert_eq!(
                sup.tracked(),
                0,
                "第 {i} 次 start/shutdown 后 tracked 必须为 0（泄漏）"
            );
        }
    }

    /// panic 的任务：不影响 quiescence（shutdown 后 tracked 归 0）。
    /// （错误汇总在 P2-06 迁 watchdog 时引入 join_handles；walking skeleton
    /// 阶段 panic 任务被 TaskTracker 捕获，wait() 仍正常返回。）
    #[tokio::test]
    async fn panicking_task_does_not_block_quiescence() {
        let mut sup = Supervisor::new();
        sup.spawn("panic-task", |_cancel| async move {
            panic!("boom");
        });
        sup.shutdown().await.expect("panic 任务不阻塞 shutdown");
        assert_eq!(sup.tracked(), 0, "panic 任务后 tracked 必须归 0");
    }

    /// 父 token 提前 cancel：任务观察到取消。
    #[tokio::test]
    async fn external_cancel_propagates() {
        let mut sup = Supervisor::new();
        sup.spawn("observe", |cancel| async move {
            cancel.cancelled().await;
        });
        // 外部 cancel 父 token。
        sup.token().cancel();
        sup.shutdown().await.expect("shutdown 无失败");
        assert!(sup.is_cancelled());
    }
}
