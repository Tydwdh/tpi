//! Checkpoint — logical workspace state snapshot with delta chain.

use super::mutation::WorkspaceMutation;
use super::types::{CheckpointId, Timestamp};
use serde::{Deserialize, Serialize};

/// A checkpoint represents a logical workspace state.
/// Checkpoints form a chain: each checkpoint N records deltas from N-1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub parent: Option<CheckpointId>,
    pub created_at: Timestamp,
    /// Mutations applied between parent checkpoint and this one.
    pub mutations: Vec<WorkspaceMutation>,
    /// Human-readable summary.
    pub summary: CheckpointSummary,
}

/// Compact summary for display and diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub affected_paths: Vec<String>,
    pub files_created: usize,
    pub files_modified: usize,
    pub files_deleted: usize,
    pub files_renamed: usize,
}

impl CheckpointSummary {
    pub fn from_mutations(mutations: &[WorkspaceMutation]) -> Self {
        let mut created = 0;
        let mut modified = 0;
        let mut deleted = 0;
        let mut renamed = 0;
        let mut paths = Vec::new();

        for m in mutations {
            for p in m.affected_paths() {
                paths.push(p.to_string());
            }
            match m {
                WorkspaceMutation::Create { .. } => created += 1,
                WorkspaceMutation::Modify { .. } => modified += 1,
                WorkspaceMutation::Delete { .. } => deleted += 1,
                WorkspaceMutation::Rename { .. } => renamed += 1,
            }
        }

        paths.sort();
        paths.dedup();

        Self {
            affected_paths: paths,
            files_created: created,
            files_modified: modified,
            files_deleted: deleted,
            files_renamed: renamed,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files_created == 0
            && self.files_modified == 0
            && self.files_deleted == 0
            && self.files_renamed == 0
    }
}

impl Checkpoint {
    pub fn new(parent: Option<CheckpointId>, mutations: Vec<WorkspaceMutation>) -> Self {
        let summary = CheckpointSummary::from_mutations(&mutations);
        Self {
            id: CheckpointId::new(&uuid::Uuid::now_v7().to_string()),
            parent,
            created_at: Timestamp::now(),
            mutations,
            summary,
        }
    }

    /// Whether this is a baseline checkpoint (no parent = full state).
    pub fn is_baseline(&self) -> bool {
        self.parent.is_none()
    }

    /// Total number of file-level mutations in this checkpoint.
    pub fn mutation_count(&self) -> usize {
        self.mutations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{BlobId, NormalizedPath};
    use super::*;
    use std::path::Path;

    fn bp(s: &str) -> NormalizedPath {
        NormalizedPath::new(Path::new(s), Path::new("."))
    }

    #[test]
    fn summary_from_empty() {
        let s = CheckpointSummary::from_mutations(&[]);
        assert!(s.is_empty());
    }

    #[test]
    fn summary_counts() {
        let mutations = vec![
            WorkspaceMutation::Create {
                path: bp("new.txt"),
                content: BlobId::new("aaa"),
            },
            WorkspaceMutation::Modify {
                path: bp("mod.txt"),
                before: BlobId::new("a"),
                after: BlobId::new("b"),
            },
        ];
        let s = CheckpointSummary::from_mutations(&mutations);
        assert_eq!(s.files_created, 1);
        assert_eq!(s.files_modified, 1);
        assert_eq!(s.files_deleted, 0);
    }

    #[test]
    fn new_checkpoint_has_id() {
        let cp = Checkpoint::new(None, vec![]);
        assert!(!cp.id.as_str().is_empty());
        assert!(cp.is_baseline());
    }

    #[test]
    fn non_baseline_checkpoint() {
        let parent = CheckpointId::new("parent-1");
        let cp = Checkpoint::new(Some(parent.clone()), vec![]);
        assert!(!cp.is_baseline());
        assert_eq!(cp.parent.unwrap(), parent);
    }
}
