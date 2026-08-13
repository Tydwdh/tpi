//! Remote Workspace（任务书 §28、§31-§40）。
//!
//! - `ssh`：R0 SSH transport（connect/exec/file primitives + host key 校验）；
//! - `RemoteWorkspace`：R1 起的 logical 形态（host + root + shell + connection），
//!   ShellSessionState 与 LocalWorkspace 共享同一套语义（§36）。

pub mod ssh;

use std::sync::{Arc, Mutex};

use crate::shell::ShellSessionState;
use crate::workspace::WorkspaceId;

/// 远端 Workspace（§28/§37：logical state 与 transport 分离）。
#[derive(Debug, Clone)]
pub struct RemoteWorkspace {
    pub host: ssh::RemoteHost,
    pub root: camino::Utf8PathBuf,
    /// 远端逻辑 shell 状态（cwd/env；与 Local 共享同一语义，§36）。
    pub shell: Arc<Mutex<ShellSessionState>>,
    /// SSH transport 状态（socket/channel，§50：断开不等于 logical state 消失）。
    pub connection: Arc<Mutex<ssh::ConnectionState>>,
}

impl RemoteWorkspace {
    pub fn new(host: ssh::RemoteHost, root: camino::Utf8PathBuf) -> Self {
        Self {
            shell: Arc::new(Mutex::new(ShellSessionState::new(root.clone()))),
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
