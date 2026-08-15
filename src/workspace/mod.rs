//! ActiveWorkspace（任务书 §26-§30）。
//!
//! 架构分层：Tool Protocol → ActiveWorkspace → Local/Remote Workspace。
//! 模型始终只面对 read/list/search/edit/write/bash 等工具；"当前在哪个
//! workspace 执行"由 harness 决定（§48 transport 不泄漏给模型）。
//!
//! 一个 Agent Session 默认只有一个 ActiveWorkspace（§29）：工具调用不携带
//! host 参数，切换 workspace 是显式的会话级操作。
//!
//! 设计约束（§27）：不要巨大 trait / VFS / 20 个 async trait；先找真正变化
//! 的 seam（filesystem access、command execution、workspace identity、
//! shell state），enum 足够就用 enum。

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;

use crate::shell::ShellSessionState;

/// Workspace 类型标识（§30）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceKind {
    Local,
    Remote,
}

/// Workspace Identity（§30）：`local:C:\project`、`ssh:gpu:/home/dev/project`。
/// 用于 TUI 状态栏（§51）、agent context（§53）与 session resume（§49）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceId {
    Local(Utf8PathBuf),
    /// host 别名/地址 + 远端 root（R 阶段启用）。
    Remote {
        host: String,
        root: String,
    },
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceId::Local(root) => write!(f, "local:{}", root),
            WorkspaceId::Remote { host, root } => write!(f, "ssh:{host}:{root}"),
        }
    }
}

/// 本地 Workspace（§28）：root + 属于该 workspace 的 shell 状态 + 沙箱策略。
#[derive(Debug, Clone)]
pub struct LocalWorkspace {
    pub root: Utf8PathBuf,
    /// Logical Shell Session（§9：每个 workspace 独立 cwd/env，互不污染）。
    pub shell: Arc<Mutex<ShellSessionState>>,
    /// §9.1 自由模式（默认 true）。
    pub allow_outside_workspace: bool,
}

impl LocalWorkspace {
    pub fn new(root: Utf8PathBuf, allow_outside_workspace: bool) -> Self {
        Self {
            shell: Arc::new(Mutex::new(ShellSessionState::new(root.clone()))),
            root,
            allow_outside_workspace,
        }
    }

    pub fn id(&self) -> WorkspaceId {
        WorkspaceId::Local(self.root.clone())
    }
}

/// Workspace 变体（§28：enum 优先；Remote 在 R 阶段加入）。
#[derive(Debug, Clone)]
pub enum Workspace {
    Local(LocalWorkspace),
    Remote(crate::remote::RemoteWorkspace),
}

/// 当前激活的 Workspace + runtime 状态（§50 ConnectionState 后续加入）。
#[derive(Debug, Clone)]
pub struct ActiveWorkspace {
    pub workspace: Workspace,
}

impl ActiveWorkspace {
    pub fn local(local: LocalWorkspace) -> Self {
        Self {
            workspace: Workspace::Local(local),
        }
    }

    pub fn remote(remote: crate::remote::RemoteWorkspace) -> Self {
        Self {
            workspace: Workspace::Remote(remote),
        }
    }

    pub fn kind(&self) -> WorkspaceKind {
        match &self.workspace {
            Workspace::Local(_) => WorkspaceKind::Local,
            Workspace::Remote(_) => WorkspaceKind::Remote,
        }
    }

    pub fn id(&self) -> WorkspaceId {
        match &self.workspace {
            Workspace::Local(local) => local.id(),
            Workspace::Remote(remote) => remote.id(),
        }
    }

    /// 当前 workspace 的 shell 状态（各 workspace 独立）。
    pub fn shell(&self) -> &Arc<Mutex<ShellSessionState>> {
        match &self.workspace {
            Workspace::Local(local) => &local.shell,
            Workspace::Remote(remote) => &remote.shell,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_workspace_has_own_shell_state() {
        let ws = LocalWorkspace::new(Utf8PathBuf::from("C:/proj"), true);
        let active = ActiveWorkspace::local(ws);
        assert_eq!(active.kind(), WorkspaceKind::Local);
        assert_eq!(active.id().to_string(), "local:C:/proj");
        let shell = active.shell().lock().unwrap();
        assert_eq!(shell.cwd.as_str(), "C:/proj");
        assert_eq!(shell.version, 0);
    }

    #[test]
    fn two_workspaces_do_not_share_shell_state() {
        let a = LocalWorkspace::new(Utf8PathBuf::from("C:/a"), true);
        let b = LocalWorkspace::new(Utf8PathBuf::from("C:/b"), true);
        a.shell.lock().unwrap().cwd = Utf8PathBuf::from("C:/a/src");
        let a_cwd = a.shell.lock().unwrap().cwd.clone();
        let b_cwd = b.shell.lock().unwrap().cwd.clone();
        assert_eq!(a_cwd.as_str(), "C:/a/src");
        assert_eq!(b_cwd.as_str(), "C:/b", "workspace B 不得被 A 污染（§9）");
    }
}

// ---------------------------------------------------------------------------
// P5-05：Workspace ports——窄接口（Local/Remote 同一 contract，不建 mega VFS）。
//
// read/write/bash/process 的真实 consumer 只需要：root + shell + kind。
// [`WorkspacePort`] 是它们的窄依赖；Local/Remote 实现并跑同一 contract。

/// 工具消费的窄 workspace 接口（P5-05）。
pub trait WorkspacePort {
    /// workspace root（路径解析边界）。
    fn root(&self) -> &camino::Utf8PathBuf;
    /// logical shell（cwd/env；各 workspace 独立）。
    fn shell(&self) -> &Arc<std::sync::Mutex<crate::shell::ShellSessionState>>;
    /// workspace 种类（Local/Remote；metadata，不分支执行）。
    fn kind(&self) -> WorkspaceKind;
}

impl WorkspacePort for LocalWorkspace {
    fn root(&self) -> &camino::Utf8PathBuf {
        &self.root
    }
    fn shell(&self) -> &Arc<std::sync::Mutex<crate::shell::ShellSessionState>> {
        &self.shell
    }
    fn kind(&self) -> WorkspaceKind {
        WorkspaceKind::Local
    }
}

impl WorkspacePort for crate::remote::RemoteWorkspace {
    fn root(&self) -> &camino::Utf8PathBuf {
        &self.root
    }
    fn shell(&self) -> &Arc<std::sync::Mutex<crate::shell::ShellSessionState>> {
        &self.shell
    }
    fn kind(&self) -> WorkspaceKind {
        WorkspaceKind::Remote
    }
}

/// ActiveWorkspace 上的 port 视图（consumer 用，不感知变体分发）。
impl ActiveWorkspace {
    pub fn port(&self) -> &dyn WorkspacePort {
        match &self.workspace {
            Workspace::Local(local) => local,
            Workspace::Remote(remote) => remote,
        }
    }
}

#[cfg(test)]
mod port_tests {
    use super::*;

    /// 同一 contract 对 Local 与 Remote 运行（P5-05：接口非单实现）。
    fn assert_workspace_port(ws: &dyn WorkspacePort, expected_kind: WorkspaceKind) {
        assert!(!ws.root().as_str().is_empty(), "root 必须存在");
        assert_eq!(ws.kind(), expected_kind);
        assert!(
            !ws.shell().lock().unwrap().cwd.as_str().is_empty(),
            "shell cwd 必须存在"
        );
    }

    #[test]
    fn local_port_contract() {
        let ws = LocalWorkspace::new(Utf8PathBuf::from("C:/proj"), true);
        assert_workspace_port(&ws, WorkspaceKind::Local);
        assert_eq!(ws.root().as_str(), "C:/proj");
    }

    #[test]
    fn remote_port_contract() {
        // RemoteWorkspace 需要 ssh host/client；用 minimal 构造验证 port 视图
        // （root/shell/kind 不依赖传输就绪）。
        let ws = crate::remote::RemoteWorkspace::new(
            crate::remote::ssh::RemoteHost {
                alias: "test".into(),
                hostname: "localhost".into(),
                port: 22,
                user: "u".into(),
                identity_file: None,
                known_hosts_path: std::path::PathBuf::from("~/.ssh/known_hosts"),
                strict_host_key_checking: true,
                password: None,
            },
            Utf8PathBuf::from("/remote/proj"),
        );
        assert_workspace_port(&ws, WorkspaceKind::Remote);
        assert_eq!(ws.root().as_str(), "/remote/proj");
    }

    /// ActiveWorkspace::port() 统一视图。
    #[test]
    fn active_workspace_port_view() {
        let active = ActiveWorkspace::local(LocalWorkspace::new(Utf8PathBuf::from("C:/p"), true));
        assert_workspace_port(active.port(), WorkspaceKind::Local);
    }
}
