//! WorkspaceSession — bridge between WorkspaceManager and tool execution.
//!
//! Wraps `tpi_core::workspace::WorkspaceManager` with a tool-friendly API.
//! Every tool that may mutate the workspace goes through this session.

use std::path::Path;
use std::sync::{Arc, Mutex};
use tpi_core::workspace::manager::{WorkspaceConfig, WorkspaceManager};
use tpi_core::workspace::mutation::WorkspaceMutation;
use tpi_core::workspace::policy::{MutationSafetyPolicy, Reversibility, TrackingPolicy};
use tpi_core::workspace::transaction::{MutationCause, MutationProvenance};
use tpi_core::workspace::types::{BlobId, TransactionId};

/// Shared handle to the workspace manager — passed through ToolContext.
pub type SharedWorkspaceManager = Arc<Mutex<WorkspaceManager>>;

/// Create a new shared workspace manager.
pub fn create_workspace_manager(
    workspace_root: &Path,
    session_id: &str,
    artifacts_root: &Path,
) -> Result<SharedWorkspaceManager, String> {
    let config = WorkspaceConfig {
        tracking_policy: TrackingPolicy::default(),
        safety_policy: MutationSafetyPolicy::BestEffort,
    };
    let mgr = WorkspaceManager::open(workspace_root, session_id, artifacts_root, config)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(Mutex::new(mgr)))
}

/// High-level session API for tool integration.
#[derive(Clone)]
pub struct WorkspaceSession {
    shared: SharedWorkspaceManager,
    provenance: Option<MutationProvenance>,
}

impl WorkspaceSession {
    pub fn new(shared: SharedWorkspaceManager) -> Self {
        Self {
            shared,
            provenance: None,
        }
    }

    /// Bind this lightweight view to one AgentRuntime. The underlying manager
    /// and journal remain global; only transaction provenance is per runtime.
    pub fn with_provenance(mut self, provenance: MutationProvenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    fn cause(&self, cause: MutationCause) -> MutationCause {
        let tool_call_id = match &cause {
            MutationCause::Edit { tool_call_id } | MutationCause::Write { tool_call_id } => {
                Some(tool_call_id.clone())
            }
            MutationCause::Command { command_id } => Some(command_id.clone()),
            MutationCause::Terminal { terminal_id } => Some(terminal_id.clone()),
            MutationCause::Job { process_id } => Some(process_id.clone()),
            _ => None,
        };
        self.provenance.clone().map_or(cause.clone(), |provenance| {
            cause.with_provenance(provenance, tool_call_id)
        })
    }

    /// Store content in the blob store and return its id.
    pub fn store_blob(&self, content: &[u8]) -> Result<BlobId, String> {
        let mgr = self.shared.lock().map_err(|e| e.to_string())?;
        mgr.blob_store().put(content).map_err(|e| e.to_string())
    }

    /// Begin a transaction, record mutations, and commit — all in one call.
    /// This is the primary API for single-file tools (edit/write).
    pub fn record_single_mutation(
        &self,
        cause: MutationCause,
        mutation: WorkspaceMutation,
    ) -> Result<Reversibility, String> {
        let mut mgr = self.shared.lock().map_err(|e| e.to_string())?;
        let handle = mgr
            .begin_transaction(self.cause(cause))
            .map_err(|e| e.to_string())?;
        let tx_id = handle.tx_id().clone();
        drop(handle);
        mgr.record_mutation(&tx_id, mutation.clone())
            .map_err(|e| e.to_string())?;
        mgr.commit_transaction(&tx_id, vec![mutation])
            .map_err(|e| e.to_string())
    }

    /// Begin a multi-mutation transaction (for bash/terminal).
    /// Returns a TransactionGuard that must be committed or aborted.
    pub fn begin_transaction(&self, cause: MutationCause) -> Result<TransactionGuard, String> {
        let mut mgr = self.shared.lock().map_err(|e| e.to_string())?;
        let handle = mgr
            .begin_transaction(self.cause(cause))
            .map_err(|e| e.to_string())?;
        let tx_id = handle.tx_id().clone();
        drop(handle);
        Ok(TransactionGuard {
            shared: self.shared.clone(),
            tx_id,
        })
    }

    /// Access the underlying shared manager.
    pub fn shared(&self) -> &SharedWorkspaceManager {
        &self.shared
    }

    /// Reconcile workspace after a command/bash execution.
    /// Detects which files changed since the last state, records mutations
    /// in the journal, and updates the persistent index.
    ///
    /// P2.3：唯一 delta→journal 转换点已下沉到
    /// [`WorkspaceManager::reconcile_unknown_effect`]（watcher Healthy→增量
    /// `reconcile_paths`，Uncertain/无 watcher→全量 `reconcile`）。此处仅委托。
    pub fn reconcile_after_execution(
        &self,
        cause: MutationCause,
        _workspace_root: &std::path::Path,
    ) -> Result<Reversibility, String> {
        let mut mgr = self.shared.lock().map_err(|e| e.to_string())?;
        mgr.reconcile_unknown_effect(self.cause(cause))
            .map_err(|e| e.to_string())
    }
}

/// Guard for an active transaction. Must be committed or dropped.
pub struct TransactionGuard {
    shared: SharedWorkspaceManager,
    tx_id: TransactionId,
}

impl TransactionGuard {
    pub fn tx_id(&self) -> &TransactionId {
        &self.tx_id
    }

    /// Record a single mutation within this transaction.
    pub fn record(&self, mutation: WorkspaceMutation) -> Result<(), String> {
        let mut mgr = self.shared.lock().map_err(|e| e.to_string())?;
        mgr.record_mutation(&self.tx_id, mutation)
            .map_err(|e| e.to_string())
    }

    /// Commit the transaction with all accumulated mutations.
    pub fn commit(self, mutations: Vec<WorkspaceMutation>) -> Result<Reversibility, String> {
        let mut mgr = self.shared.lock().map_err(|e| e.to_string())?;
        mgr.commit_transaction(&self.tx_id, mutations)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempdir().unwrap();
        let ws = root.path().to_path_buf();
        let art = root.path().join("artifacts");
        std::fs::create_dir_all(&art).unwrap();
        std::fs::write(ws.join("hello.txt"), b"hello").unwrap();
        (root, ws, art)
    }

    #[test]
    fn session_store_and_retrieve_blob() {
        let (_root, ws, art) = setup();
        let shared = create_workspace_manager(&ws, "s1", &art).unwrap();
        let session = WorkspaceSession::new(shared);

        let blob_id = session.store_blob(b"test content").unwrap();
        assert!(!blob_id.as_str().is_empty());
    }

    #[test]
    fn session_record_single_mutation() {
        let (_root, ws, art) = setup();
        let shared = create_workspace_manager(&ws, "s1", &art).unwrap();
        let session = WorkspaceSession::new(shared);

        let before_content = std::fs::read(ws.join("hello.txt")).unwrap();
        let before_blob = session.store_blob(&before_content).unwrap();
        let after_blob = session.store_blob(b"HELLO").unwrap();

        let norm = tpi_core::workspace::types::NormalizedPath::new(&ws.join("hello.txt"), &ws);

        let rev = session
            .record_single_mutation(
                MutationCause::Edit {
                    tool_call_id: "tc-1".into(),
                },
                WorkspaceMutation::Modify {
                    path: norm,
                    before: before_blob,
                    after: after_blob,
                },
            )
            .unwrap();

        assert!(rev.is_exact());
    }

    #[test]
    fn session_journal_records_agent_provenance() {
        let (_root, ws, art) = setup();
        let shared = create_workspace_manager(&ws, "s-agent", &art).unwrap();
        let session = WorkspaceSession::new(shared).with_provenance(MutationProvenance {
            agent_id: "agent-a".into(),
            parent_agent_id: Some("agent-root".into()),
            delegation_id: Some("delegation-1".into()),
        });
        let before = session.store_blob(b"hello").unwrap();
        let after = session.store_blob(b"HELLO").unwrap();
        let path = tpi_core::workspace::types::NormalizedPath::new(&ws.join("hello.txt"), &ws);
        session
            .record_single_mutation(
                MutationCause::Edit {
                    tool_call_id: "call-1".into(),
                },
                WorkspaceMutation::Modify {
                    path,
                    before,
                    after,
                },
            )
            .unwrap();
        let state = tpi_core::workspace::journal::MutationJournal::new(&art, "s-agent")
            .load()
            .unwrap();
        assert!(matches!(
            state.entries[0].cause,
            MutationCause::Agent {
                ref agent_id,
                ref parent_agent_id,
                ref delegation_id,
                ref tool_call_id,
                ..
            } if agent_id == "agent-a"
                && parent_agent_id.as_deref() == Some("agent-root")
                && delegation_id.as_deref() == Some("delegation-1")
                && tool_call_id.as_deref() == Some("call-1")
        ));
    }

    #[test]
    fn session_begin_and_commit_transaction() {
        let (_root, ws, art) = setup();
        let shared = create_workspace_manager(&ws, "s1", &art).unwrap();
        let session = WorkspaceSession::new(shared);

        let before_content = std::fs::read(ws.join("hello.txt")).unwrap();
        let before_blob = session.store_blob(&before_content).unwrap();
        let after_blob = session.store_blob(b"MODIFIED").unwrap();

        let norm = tpi_core::workspace::types::NormalizedPath::new(&ws.join("hello.txt"), &ws);

        let guard = session
            .begin_transaction(MutationCause::Command {
                command_id: "cmd-1".into(),
            })
            .unwrap();

        let mutation = WorkspaceMutation::Modify {
            path: norm,
            before: before_blob,
            after: after_blob,
        };
        guard.record(mutation.clone()).unwrap();
        let rev = guard.commit(vec![mutation]).unwrap();
        assert!(rev.is_exact());
    }
}
