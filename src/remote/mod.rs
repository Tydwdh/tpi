//! Remote Workspace（任务书 §28、§31-§40）。
//!
//! - `ssh`：R0 SSH transport（connect/exec/file primitives + host key 校验）；
//! - `RemoteWorkspace`：R1 起的 logical 形态（host + root + shell + connection），
//!   ShellSessionState 与 LocalWorkspace 共享同一套语义（§36）。

pub mod executor;
pub mod ssh;

use std::sync::{Arc, Mutex};

use crate::shell::ShellSessionState;
use crate::workspace::WorkspaceId;

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
