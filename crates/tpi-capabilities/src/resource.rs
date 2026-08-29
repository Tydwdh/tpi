//! Session-scoped managed resources.
//!
//! `ResourceManager` is the capability boundary for processes and terminals.
//! The underlying registries remain intentionally small state containers; this
//! type supplies shared ownership, authorization, and lifecycle cleanup.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tpi_core::resource::{AgentIdentity, ResourceLifetime, ResourceMeta};

use crate::process::managed::{ManagedProcess, ManagedProcessState, ProcessId, ProcessRegistry};
use crate::terminal::{TerminalRead, TerminalRegistry};

pub type SharedResourceManager = Arc<ResourceManager>;

#[derive(Clone, Copy)]
enum CleanupScope {
    Agent(tpi_core::ids::AgentId),
    Delegation(tpi_core::ids::DelegationId),
}

/// The single session-level owner of managed process and terminal registries.
pub struct ResourceManager {
    processes: Arc<Mutex<ProcessRegistry>>,
    terminals: Arc<Mutex<TerminalRegistry>>,
}

impl ResourceManager {
    pub fn new() -> Self {
        Self::from_registries(
            Arc::new(Mutex::new(ProcessRegistry::new())),
            Arc::new(Mutex::new(TerminalRegistry::default())),
        )
    }

    /// Compatibility constructor for existing composition roots. The returned
    /// manager still becomes the sole access boundary; both registries remain
    /// shared by every runtime that receives it.
    pub fn from_registries(
        processes: Arc<Mutex<ProcessRegistry>>,
        terminals: Arc<Mutex<TerminalRegistry>>,
    ) -> Self {
        Self {
            processes,
            terminals,
        }
    }

    pub fn processes(&self) -> Arc<Mutex<ProcessRegistry>> {
        self.processes.clone()
    }

    pub fn terminals(&self) -> Arc<Mutex<TerminalRegistry>> {
        self.terminals.clone()
    }

    fn can_access(meta: Option<ResourceMeta>, caller: &AgentIdentity) -> bool {
        match meta {
            // Entries created by low-level compatibility tests predate the
            // metadata contract. They remain visible so those tests exercise
            // registry mechanics, while all production-created resources are
            // metadata-bearing and owner checked.
            None => true,
            Some(meta) => caller.can_manage(meta.owner, meta.lifetime),
        }
    }

    fn matches_scope(scope: CleanupScope, meta: ResourceMeta) -> bool {
        match scope {
            CleanupScope::Agent(agent_id) => {
                meta.owner.agent_id == agent_id && meta.lifetime == ResourceLifetime::Agent
            }
            CleanupScope::Delegation(delegation_id) => {
                meta.owner.delegation_id == Some(delegation_id)
                    && meta.lifetime == ResourceLifetime::Delegation
            }
        }
    }

    pub fn list_processes(&self, caller: &AgentIdentity) -> Vec<ManagedProcess> {
        let registry = tpi_core::util::lock_mutex(&self.processes, "process_registry");
        registry
            .iter()
            .filter(|process| Self::can_access(process.resource_meta, caller))
            .cloned()
            .collect()
    }

    pub fn process(&self, caller: &AgentIdentity, id: ProcessId) -> Option<ManagedProcess> {
        let registry = tpi_core::util::lock_mutex(&self.processes, "process_registry");
        registry
            .get(id)
            .filter(|process| Self::can_access(process.resource_meta, caller))
            .cloned()
    }

    pub fn cancel_process(&self, caller: &AgentIdentity, id: ProcessId) -> bool {
        let registry = tpi_core::util::lock_mutex(&self.processes, "process_registry");
        let Some(process) = registry.get(id) else {
            return false;
        };
        if !Self::can_access(process.resource_meta, caller) {
            return false;
        }
        registry.cancel(id)
    }

    /// Wait without retaining the registry mutex across an await.
    pub async fn wait_process(
        &self,
        caller: &AgentIdentity,
        id: ProcessId,
        timeout: Duration,
    ) -> Option<ManagedProcessState> {
        self.process(caller, id)?;
        crate::process::managed::wait_process(&self.processes, id, timeout).await
    }

    pub fn list_terminals(&self, caller: &AgentIdentity) -> Vec<String> {
        let registry = tpi_core::util::lock_mutex(&self.terminals, "terminal_registry");
        registry
            .resource_ids()
            .into_iter()
            .filter(|id| {
                registry
                    .resource_meta(id)
                    .is_none_or(|meta| Self::can_access(Some(meta), caller))
            })
            .collect()
    }

    pub fn open_terminal(
        &self,
        program: &str,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
        workspace_session: Option<std::sync::Arc<crate::workspace::session::WorkspaceSession>>,
        resource_meta: ResourceMeta,
    ) -> Result<String, String> {
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry")
            .open_with_workspace_session_meta(
                program,
                cwd,
                rows,
                cols,
                workspace_session,
                resource_meta,
            )
    }

    /// Compatibility boundary for embedded/low-level tool contexts that do
    /// not own a persistent `WorkspaceSession`. Normal runtimes should use
    /// `open_terminal` so PTY creation never requires another full snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn open_tracked_terminal(
        &self,
        program: &str,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
        workspace: crate::workspace::tracked::TrackedWorkspace,
        artifacts_root: std::path::PathBuf,
        session_id: String,
        resource_meta: ResourceMeta,
    ) -> Result<String, String> {
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry").open_tracked_with_meta(
            program,
            cwd,
            rows,
            cols,
            workspace,
            artifacts_root,
            session_id,
            resource_meta,
        )
    }

    fn authorize_terminal(&self, caller: &AgentIdentity, id: &str) -> Result<(), String> {
        let registry = tpi_core::util::lock_mutex(&self.terminals, "terminal_registry");
        if !registry.contains(id) {
            return Err("terminal not found or not authorized".into());
        }
        if registry
            .resource_meta(id)
            .is_none_or(|meta| Self::can_access(Some(meta), caller))
        {
            Ok(())
        } else {
            Err("terminal not found or not authorized".into())
        }
    }

    pub fn write_terminal(
        &self,
        caller: &AgentIdentity,
        id: &str,
        data: &[u8],
    ) -> Result<(), String> {
        self.authorize_terminal(caller, id)?;
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry").write(id, data)
    }

    pub fn checkpoint_terminal(
        &self,
        caller: &AgentIdentity,
        id: &str,
        artifacts_root: &std::path::Path,
        session_id: &str,
    ) -> Result<usize, String> {
        self.authorize_terminal(caller, id)?;
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry").checkpoint_workspace(
            id,
            artifacts_root,
            session_id,
        )
    }

    pub fn read_terminal(
        &self,
        caller: &AgentIdentity,
        id: &str,
        after: u64,
    ) -> Result<TerminalRead, String> {
        self.authorize_terminal(caller, id)?;
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry").read(id, after)
    }

    pub fn resize_terminal(
        &self,
        caller: &AgentIdentity,
        id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), String> {
        self.authorize_terminal(caller, id)?;
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry").resize(id, rows, cols)
    }

    pub fn signal_terminal(&self, caller: &AgentIdentity, id: &str) -> Result<(), String> {
        self.authorize_terminal(caller, id)?;
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry").signal(id)
    }

    pub fn close_terminal(&self, caller: &AgentIdentity, id: &str) -> Result<(), String> {
        self.authorize_terminal(caller, id)?;
        tpi_core::util::lock_mutex(&self.terminals, "terminal_registry").close(id)
    }

    /// Cancel all Agent-lifetime resources owned by one graph node. Every
    /// process wait happens after the mutex guard is dropped; terminal close is
    /// synchronous and idempotent at the registry boundary.
    pub async fn cleanup_agent(&self, agent_id: tpi_core::ids::AgentId) -> Result<(), String> {
        self.cleanup_scope(CleanupScope::Agent(agent_id)).await
    }

    /// Clean resources that belong to one delegation edge after its worker
    /// settles. This is separate from Agent cleanup because delegation-lived
    /// resources intentionally outlive one run of the child agent.
    pub async fn cleanup_delegation(
        &self,
        delegation_id: tpi_core::ids::DelegationId,
    ) -> Result<(), String> {
        self.cleanup_scope(CleanupScope::Delegation(delegation_id))
            .await
    }

    async fn cleanup_scope(&self, scope: CleanupScope) -> Result<(), String> {
        let process_ids = {
            let registry = tpi_core::util::lock_mutex(&self.processes, "process_registry");
            registry
                .iter()
                .filter(|process| {
                    process.resource_meta.is_some_and(|meta| {
                        Self::matches_scope(scope, meta) && !process.state.is_terminal()
                    })
                })
                .map(|process| process.id)
                .collect::<Vec<_>>()
        };
        for id in &process_ids {
            let _ = tpi_core::util::lock_mutex(&self.processes, "process_registry").cancel(*id);
        }
        for id in process_ids {
            let state =
                crate::process::managed::wait_process(&self.processes, id, Duration::from_secs(3))
                    .await;
            if state.is_some_and(|state| !state.is_terminal()) {
                return Err(format!("resource {id} did not reach a terminal state"));
            }
        }

        let terminal_ids = {
            let registry = tpi_core::util::lock_mutex(&self.terminals, "terminal_registry");
            registry
                .resource_ids()
                .into_iter()
                .filter(|id| {
                    registry
                        .resource_meta(id)
                        .is_some_and(|meta| Self::matches_scope(scope, meta))
                })
                .collect::<Vec<_>>()
        };
        let mut registry = tpi_core::util::lock_mutex(&self.terminals, "terminal_registry");
        for id in terminal_ids {
            // `close` kills the PTY child and removes the entry. Repeating
            // cleanup therefore naturally becomes a no-op for that id.
            registry.close_with_checkpoint(&id)?;
        }
        Ok(())
    }

    /// Session shutdown boundary: cancel every active process and close every
    /// terminal after the runtime has stopped accepting new work.
    pub async fn shutdown(&self) -> Result<(), String> {
        let mut errors = Vec::new();
        let process_ids = {
            let registry = tpi_core::util::lock_mutex(&self.processes, "process_registry");
            registry
                .iter()
                .filter(|process| !process.state.is_terminal())
                .map(|process| process.id)
                .collect::<Vec<_>>()
        };
        for id in &process_ids {
            // `false` can mean the process won the race and already became
            // terminal; the wait below is the authoritative outcome check.
            let _ = tpi_core::util::lock_mutex(&self.processes, "process_registry").cancel(*id);
        }
        for id in process_ids {
            match crate::process::managed::wait_process(&self.processes, id, Duration::from_secs(5))
                .await
            {
                Some(state) if state.is_terminal() => {}
                Some(state) => errors.push(format!("resource {id} did not stop: {state:?}")),
                None => errors.push(format!("resource {id} disappeared before shutdown")),
            }
        }
        let terminal_ids = {
            let registry = tpi_core::util::lock_mutex(&self.terminals, "terminal_registry");
            registry.resource_ids()
        };
        let mut registry = tpi_core::util::lock_mutex(&self.terminals, "terminal_registry");
        for id in terminal_ids {
            if let Err(error) = registry.close_with_checkpoint(&id) {
                errors.push(format!("terminal {id} cleanup failed: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Default for ResourceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tpi_core::ids::{AgentId, ToolCallId};
    use tpi_core::resource::{ResourceOwner, WorkspaceAccess};

    fn identity(agent_id: AgentId) -> AgentIdentity {
        AgentIdentity {
            agent_id,
            parent_agent_id: None,
            delegation_id: None,
            managed_agent_ids: Vec::new(),
        }
    }

    fn meta(agent_id: AgentId, lifetime: ResourceLifetime) -> ResourceMeta {
        ResourceMeta {
            owner: ResourceOwner {
                agent_id,
                delegation_id: None,
            },
            lifetime,
            created_by: ToolCallId::new_v7(),
            workspace_access: WorkspaceAccess::ExternallyMutable,
        }
    }

    #[test]
    fn process_visibility_is_owner_and_graph_scoped() {
        let manager = ResourceManager::new();
        let owner = AgentId::new_v7();
        let sibling = AgentId::new_v7();
        let process = ManagedProcess::new(
            ProcessId::next(),
            "local:test".into(),
            "sleep 10".into(),
            ".".into(),
            HashMap::new(),
        )
        .with_resource_meta(meta(owner, ResourceLifetime::Agent));
        let id = process.id;
        tpi_core::util::lock_mutex(&manager.processes, "process_registry")
            .insert(process)
            .unwrap();

        assert!(manager.process(&identity(owner), id).is_some());
        assert!(manager.process(&identity(sibling), id).is_none());

        let mut parent = identity(sibling);
        parent.managed_agent_ids.push(owner);
        assert!(manager.process(&parent, id).is_some());
    }

    #[test]
    fn session_lifetime_is_visible_to_all_callers_in_the_session() {
        let manager = ResourceManager::new();
        let owner = AgentId::new_v7();
        let other = AgentId::new_v7();
        let process = ManagedProcess::new(
            ProcessId::next(),
            "local:test".into(),
            "session job".into(),
            ".".into(),
            HashMap::new(),
        )
        .with_resource_meta(meta(owner, ResourceLifetime::Session));
        let id = process.id;
        tpi_core::util::lock_mutex(&manager.processes, "process_registry")
            .insert(process)
            .unwrap();

        assert!(manager.process(&identity(other), id).is_some());
    }

    #[test]
    fn terminal_open_does_not_take_a_legacy_full_workspace_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        // A sparse file over the retired 64 MiB snapshot cap. Opening a
        // terminal must inspect neither its contents nor its size; mutation
        // tracking is delegated to WorkspaceSession checkpoints instead.
        std::fs::File::create(workspace.path().join("large-generated.bin"))
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        let shell = if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            "/bin/sh".into()
        };
        let manager = ResourceManager::new();
        let owner = AgentId::new_v7();
        let id = manager
            .open_terminal(
                &shell,
                workspace.path(),
                24,
                80,
                None,
                meta(owner, ResourceLifetime::Agent),
            )
            .expect("large workspace must not prevent terminal creation");

        manager.close_terminal(&identity(owner), &id).unwrap();
    }

    #[tokio::test]
    async fn tracked_terminal_cleanup_commits_final_workspace_delta() {
        let workspace = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let workspace_root = camino::Utf8PathBuf::from_path_buf(workspace.path().to_path_buf())
            .expect("temporary workspace path must be UTF-8");
        let tracked = crate::workspace::tracked::TrackedWorkspace::capture(workspace_root).unwrap();
        let shell = if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            "/bin/sh".into()
        };
        let manager = ResourceManager::new();
        let owner = AgentId::new_v7();
        manager
            .open_tracked_terminal(
                &shell,
                workspace.path(),
                24,
                80,
                tracked,
                artifacts.path().to_path_buf(),
                "terminal-session".into(),
                meta(owner, ResourceLifetime::Agent),
            )
            .unwrap();

        std::fs::write(workspace.path().join("created-by-terminal.txt"), "tracked").unwrap();
        manager.cleanup_agent(owner).await.unwrap();

        let journal = tpi_session::journal::load_journal(&tpi_session::journal::journal_path(
            artifacts.path(),
            "terminal-session",
        ))
        .unwrap();
        assert!(journal.mutations.iter().any(|mutation| {
            mutation.files.iter().any(|file| {
                file.path.ends_with("created-by-terminal.txt")
                    && !file.before_exists
                    && file.after_exists
                    && file.after_content == b"tracked"
            })
        }));
    }
}
