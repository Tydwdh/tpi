//! WorkspaceMutationBoundary — trait abstracting all mutation producers.
//!
//! Every tool that may modify the workspace goes through this boundary.
//! The boundary ensures:
//! - Preimage is stored in CAS before execution begins
//! - Post-execution reconciliation detects all changes
//! - Transaction semantics are maintained
//! - Reversibility is evaluated correctly

use super::manager::{ManagerError, WorkspaceManager};
use super::mutation::WorkspaceMutation;
use super::policy::Reversibility;
use super::transaction::MutationCause;
use std::path::Path;

/// A pre-prepared transaction boundary.
///
/// Created before mutation execution, ensures preimages are captured.
/// After execution, the caller must reconcile and commit.
pub struct MutationBoundary<'a> {
    manager: &'a mut WorkspaceManager,
    cause: MutationCause,
    tx_id: super::types::TransactionId,
    /// Preimage blob ids captured before execution (path → blob_id).
    preimages: Vec<(String, super::types::BlobId)>,
}

impl<'a> MutationBoundary<'a> {
    /// Begin a mutation boundary. Captures preimages for files that
    /// may be affected.
    pub fn begin(
        manager: &'a mut WorkspaceManager,
        cause: MutationCause,
        potentially_affected: &[&Path],
    ) -> Result<Self, ManagerError> {
        let handle = manager.begin_transaction(cause.clone())?;
        let tx_id = handle.tx_id().clone();

        // Capture preimages for all potentially affected files.
        let root = manager.root().to_path_buf();
        let mut preimages = Vec::new();
        for path in potentially_affected {
            let norm = super::types::NormalizedPath::new(path, &root);
            if let Some(blob_id) = manager.index().get_blob_id(&norm) {
                preimages.push((norm.to_string(), blob_id.clone()));
            } else {
                // File doesn't exist yet or isn't tracked.
                // Store empty-blob reference for "not exists".
                preimages.push((norm.to_string(), super::types::BlobId::new("")));
            }
        }

        Ok(Self {
            manager,
            cause,
            tx_id,
            preimages,
        })
    }

    /// Record a single mutation.
    pub fn record(&mut self, mutation: WorkspaceMutation) -> Result<(), ManagerError> {
        self.manager.record_mutation(&self.tx_id, mutation)
    }

    /// Commit the transaction with the given mutations.
    /// Returns reversibility assessment.
    pub fn commit(self, mutations: Vec<WorkspaceMutation>) -> Result<Reversibility, ManagerError> {
        self.manager.commit_transaction(&self.tx_id, mutations)
    }

    /// Get the transaction id.
    pub fn tx_id(&self) -> &super::types::TransactionId {
        &self.tx_id
    }

    /// Get the mutation cause that created this boundary.
    pub fn cause(&self) -> &MutationCause {
        &self.cause
    }

    /// Get the preimages captured at boundary creation.
    pub fn preimages(&self) -> &[(String, super::types::BlobId)] {
        &self.preimages
    }
}

/// Helper: capture preimages for a set of files before a mutation.
///
/// This is the key invariant: "preimage must exist in CAS before mutation
/// begins, not after."
pub fn capture_preimages(
    root: &Path,
    blob_store: &super::blob::BlobStore,
    files: &[&Path],
) -> Result<Vec<(String, super::types::BlobId, bool)>, ManagerError> {
    let mut result = Vec::new();
    for path in files {
        let norm = super::types::NormalizedPath::new(path, root);
        if path.exists() {
            let content = std::fs::read(path).map_err(|e| ManagerError::Io(e.to_string()))?;
            let blob_id = blob_store
                .put(&content)
                .map_err(|e| ManagerError::Blob(e.to_string()))?;
            result.push((norm.to_string(), blob_id, true));
        } else {
            result.push((norm.to_string(), super::types::BlobId::new(""), false));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::manager::{WorkspaceConfig, WorkspaceManager};
    use super::super::types::{NormalizedPath, TransactionId};
    use super::*;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempdir().unwrap();
        let ws = root.path().to_path_buf();
        let art = root.path().join("artifacts");
        std::fs::create_dir_all(&art).unwrap();
        std::fs::write(ws.join("hello.txt"), b"hello").unwrap();
        (root, ws, art)
    }

    #[test]
    fn boundary_begin_and_commit() {
        let (_root, ws, art) = setup();
        let mut mgr = WorkspaceManager::open(&ws, "s1", &art, WorkspaceConfig::default()).unwrap();

        let before = std::fs::read(ws.join("hello.txt")).unwrap();
        let bb = mgr.blob_store().put(&before).unwrap();
        let ab = mgr.blob_store().put(b"HELLO").unwrap();
        let norm = NormalizedPath::new(&ws.join("hello.txt"), &ws);

        let mut boundary = MutationBoundary::begin(
            &mut mgr,
            MutationCause::Edit {
                tool_call_id: "tc-1".into(),
            },
            &[&ws.join("hello.txt")],
        )
        .unwrap();

        assert!(!boundary.tx_id().as_str().is_empty());
        assert_eq!(boundary.preimages().len(), 1);

        let mutation = WorkspaceMutation::Modify {
            path: norm,
            before: bb,
            after: ab,
        };

        boundary.record(mutation.clone()).unwrap();

        let rev = boundary.commit(vec![mutation]).unwrap();

        assert!(rev.is_exact());
    }

    #[test]
    fn capture_preimages_works() {
        let (_root, ws, _art) = setup();
        let blob = super::super::blob::BlobStore::new(ws.join("blob"));

        let preimages = capture_preimages(&ws, &blob, &[&ws.join("hello.txt")]).unwrap();
        assert_eq!(preimages.len(), 1);
        assert!(preimages[0].2); // exists
        assert!(!preimages[0].1.as_str().is_empty()); // has blob id
    }

    #[test]
    fn capture_preimage_nonexistent() {
        let (_root, ws, _art) = setup();
        let blob = super::super::blob::BlobStore::new(ws.join("blob"));

        let preimages = capture_preimages(&ws, &blob, &[&ws.join("nope.txt")]).unwrap();
        assert_eq!(preimages.len(), 1);
        assert!(!preimages[0].2); // does not exist
    }
}
