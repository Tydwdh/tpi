//! WorkspaceSession — bridge between WorkspaceManager and tool execution.
//!
//! Wraps `tpi_core::workspace::WorkspaceManager` with a tool-friendly API.
//! Every tool that may mutate the workspace goes through this session.

use std::path::Path;
use std::sync::{Arc, Mutex};
use tpi_core::workspace::manager::{WorkspaceConfig, WorkspaceManager};
use tpi_core::workspace::mutation::WorkspaceMutation;
use tpi_core::workspace::policy::{MutationSafetyPolicy, Reversibility, TrackingPolicy};
use tpi_core::workspace::transaction::MutationCause;
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
pub struct WorkspaceSession {
    shared: SharedWorkspaceManager,
}

impl WorkspaceSession {
    pub fn new(shared: SharedWorkspaceManager) -> Self {
        Self { shared }
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
        let handle = mgr.begin_transaction(cause).map_err(|e| e.to_string())?;
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
        let handle = mgr.begin_transaction(cause).map_err(|e| e.to_string())?;
        let tx_id = handle.tx_id().clone();
        drop(handle);
        Ok(TransactionGuard {
            shared: self.shared.clone(),
            tx_id,
        })
    }

    /// Get a preimage blob id for a file (for pre-execution capture).
    pub fn get_preimage(&self, path: &Path) -> Option<BlobId> {
        let mgr = self.shared.lock().ok()?;
        let root = mgr.root().to_path_buf();
        let norm = tpi_core::workspace::types::NormalizedPath::new(path, &root);
        mgr.index().get_blob_id(&norm).cloned()
    }

    /// Store file content as preimage and return its blob id.
    pub fn capture_preimage(&self, path: &Path) -> Result<Option<BlobId>, String> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read(path).map_err(|e| e.to_string())?;
        let blob_id = self.store_blob(&content)?;
        Ok(Some(blob_id))
    }

    /// Access the underlying shared manager.
    pub fn shared(&self) -> &SharedWorkspaceManager {
        &self.shared
    }

    /// Reconcile workspace after a command/bash execution.
    /// Detects which files changed since the last state, records mutations
    /// in the journal, and updates the persistent index.
    pub fn reconcile_after_execution(
        &self,
        cause: MutationCause,
        _workspace_root: &std::path::Path,
    ) -> Result<Reversibility, String> {
        let mut mgr = self.shared.lock().map_err(|e| e.to_string())?;

        // Reconcile the index against current filesystem state.
        let exclude: Vec<String> = Vec::new();
        let delta = mgr.reconcile_index(&exclude).map_err(|e| e.to_string())?;

        let total_changes = delta.created.len() + delta.modified.len() + delta.deleted.len();
        if total_changes == 0 {
            return Ok(Reversibility::Exact);
        }

        // Build WorkspaceMutations from the delta.
        let mut mutations = Vec::new();
        for (path, entry) in &delta.created {
            if let Some(blob_id) = &entry.blob_id {
                mutations.push(WorkspaceMutation::Create {
                    path: path.clone(),
                    content: blob_id.clone(),
                });
            }
        }
        for (path, after, before) in &delta.modified {
            if let Some(blob_id) = &after.blob_id {
                mutations.push(WorkspaceMutation::Modify {
                    path: path.clone(),
                    // preimage = 修改前的旧 blob（来自 index 中已保存的旧 entry）。
                    before: before
                        .as_ref()
                        .and_then(|e| e.blob_id.clone())
                        .unwrap_or_else(|| tpi_core::workspace::types::BlobId::new("")),
                    after: blob_id.clone(),
                });
            }
        }
        for (path, before) in &delta.deleted {
            mutations.push(WorkspaceMutation::Delete {
                path: path.clone(),
                // preimage = 删除前的旧 blob（undo 恢复原内容）。
                content: before
                    .as_ref()
                    .and_then(|e| e.blob_id.clone())
                    .unwrap_or_else(|| tpi_core::workspace::types::BlobId::new("")),
            });
        }

        // Begin and commit a transaction.
        let handle = mgr.begin_transaction(cause).map_err(|e| e.to_string())?;
        let tx_id = handle.tx_id().clone();
        drop(handle);
        mgr.commit_transaction(&tx_id, mutations)
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
        assert!(session.get_preimage(&ws.join("hello.txt")).is_some());
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

    #[test]
    fn capture_preimage_existing() {
        let (_root, ws, art) = setup();
        let shared = create_workspace_manager(&ws, "s1", &art).unwrap();
        let session = WorkspaceSession::new(shared);

        let blob = session.capture_preimage(&ws.join("hello.txt")).unwrap();
        assert!(blob.is_some());
        assert!(!blob.unwrap().as_str().is_empty());
    }

    #[test]
    fn capture_preimage_nonexistent() {
        let (_root, ws, art) = setup();
        let shared = create_workspace_manager(&ws, "s1", &art).unwrap();
        let session = WorkspaceSession::new(shared);

        let blob = session.capture_preimage(&ws.join("nope.txt")).unwrap();
        assert!(blob.is_none());
    }
}
