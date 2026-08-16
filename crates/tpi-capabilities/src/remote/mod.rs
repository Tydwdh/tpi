//! Remote Workspace（任务书 §28、§31-§40）。
//!
//! - `ssh`：R0 SSH transport（connect/exec/file primitives + host key 校验）；
//! - `RemoteWorkspace`：R1 起的 logical 形态（host + root + shell + connection），
//!   ShellSessionState 与 LocalWorkspace 共享同一套语义（§36）。

pub mod executor;
pub mod files;
pub mod ssh;
pub mod traverse;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::shell::ShellSessionState;
use crate::tool::ToolContext;
use crate::workspace::WorkspaceId;

/// Default boundary for SFTP/list/search operations. Command execution carries
/// its caller-selected timeout separately in `remote::executor`.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_IO_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 每个远程操作必须携带的资源边界。
///
/// deadline、取消和大小限制属于同一操作，而不是散落在调用方的可选参数里。
/// SSH transport 负责在读流时执行 `max_output_bytes` 硬上限；该值仍随 budget
/// 传递，避免未来的 SFTP/list 实现遗忘其资源契约。
#[derive(Clone, Debug)]
pub struct IoBudget {
    deadline: Instant,
    pub max_output_bytes: usize,
    cancel: CancellationToken,
}

#[derive(Debug)]
pub enum IoError<E> {
    Cancelled,
    DeadlineExceeded,
    Operation(E),
}

impl IoBudget {
    pub fn new(timeout: Duration, max_output_bytes: usize, cancel: CancellationToken) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            max_output_bytes: max_output_bytes.max(1),
            cancel,
        }
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 执行一次远程 future，严格在同一个 deadline/cancel 边界内完成。
    pub async fn run<T, E>(
        &self,
        operation: impl Future<Output = Result<T, E>>,
    ) -> Result<T, IoError<E>> {
        if self.cancel.is_cancelled() {
            return Err(IoError::Cancelled);
        }
        if self.remaining().is_zero() {
            return Err(IoError::DeadlineExceeded);
        }
        tokio::select! {
            _ = self.cancel.cancelled() => Err(IoError::Cancelled),
            result = tokio::time::timeout(self.remaining(), operation) => {
                result.map_err(|_| IoError::DeadlineExceeded)?.map_err(IoError::Operation)
            }
        }
    }
}

/// Apply the mandatory deadline/cancellation/size contract to one remote
/// transport operation. Error mapping stays fail-closed: timeout/cancel are
/// transport failures, never interpreted as a missing file or empty listing.
pub async fn run_with_budget<T>(
    ctx: &ToolContext,
    operation: impl Future<Output = Result<T, ssh::SshError>>,
) -> Result<T, ssh::SshError> {
    match IoBudget::new(DEFAULT_IO_TIMEOUT, DEFAULT_IO_MAX_BYTES, ctx.cancel.clone())
        .run(operation)
        .await
    {
        Ok(value) => Ok(value),
        Err(IoError::Operation(error)) => Err(error),
        Err(IoError::Cancelled) => Err(ssh::SshError::Exec("remote IO cancelled".into())),
        Err(IoError::DeadlineExceeded) => Err(ssh::SshError::Exec(format!(
            "remote IO deadline exceeded ({}s)",
            DEFAULT_IO_TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(test)]
mod io_budget_tests {
    use super::{IoBudget, IoError};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn io_budget_cancels_and_bounds_deadline() {
        let cancel = CancellationToken::new();
        let budget = IoBudget::new(Duration::from_millis(5), 1024, cancel.clone());
        assert!(matches!(
            budget
                .run::<(), ()>(async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(())
                })
                .await,
            Err(IoError::DeadlineExceeded)
        ));
        cancel.cancel();
        assert!(matches!(
            budget.run::<(), ()>(async { Ok(()) }).await,
            Err(IoError::Cancelled)
        ));
    }
}

/// 远端 Workspace（§28/§37：logical state 与 transport 分离）。
///
/// 不 derive Debug：`SshClient` 持有非 Debug 的运行时句柄（session channel）。
#[derive(Clone)]
pub struct RemoteWorkspace {
    pub host: ssh::RemoteHost,
    pub root: camino::Utf8PathBuf,
    /// 远端逻辑 shell 状态（cwd/env；与 Local 共享同一语义，§36）。
    pub shell: Arc<Mutex<ShellSessionState>>,
    /// SSH transport 状态（socket/channel，§50：断开不等于 logical state 消失）。
    pub connection: Arc<Mutex<ssh::ConnectionState>>,
    /// SSH transport 客户端（exec/SFTP；bash/file 工具经此执行）。
    /// tokio Mutex：锁跨 await（async 锁，guard Send）。
    pub client: Arc<tokio::sync::Mutex<ssh::SshClient>>,
}

impl RemoteWorkspace {
    pub fn new(host: ssh::RemoteHost, root: camino::Utf8PathBuf) -> Self {
        Self {
            shell: Arc::new(Mutex::new(ShellSessionState::new(root.clone()))),
            client: Arc::new(tokio::sync::Mutex::new(ssh::SshClient::new(host.clone()))),
            host,
            root,
            connection: Arc::new(Mutex::new(ssh::ConnectionState::Disconnected)),
        }
    }

    pub fn id(&self) -> WorkspaceId {
        WorkspaceId::Remote {
            host: self.host.alias.clone(),
            root: self.root.to_string(),
        }
    }
}

impl std::fmt::Debug for RemoteWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // client 持有非 Debug 的运行时句柄，只打印 logical + connection 状态。
        f.debug_struct("RemoteWorkspace")
            .field("host", &self.host)
            .field("root", &self.root)
            .field("shell", &self.shell)
            .field("connection", &self.connection)
            .finish_non_exhaustive()
    }
}
