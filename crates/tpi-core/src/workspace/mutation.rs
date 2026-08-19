//! Unified workspace mutation data model.

use super::types::{BlobId, NormalizedPath};
use serde::{Deserialize, Serialize};

/// A single file-level mutation within a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkspaceMutation {
    /// A new file was created.
    Create {
        path: NormalizedPath,
        content: BlobId,
    },
    /// An existing file was modified.
    Modify {
        path: NormalizedPath,
        before: BlobId,
        after: BlobId,
    },
    /// A file was deleted.
    Delete {
        path: NormalizedPath,
        content: BlobId,
    },
    /// A file was renamed. If rename detection is unreliable, this is
    /// decomposed into Delete + Create internally.
    Rename {
        from: NormalizedPath,
        to: NormalizedPath,
        content: BlobId,
    },
}

impl WorkspaceMutation {
    /// All paths affected by this mutation.
    pub fn affected_paths(&self) -> Vec<&NormalizedPath> {
        match self {
            WorkspaceMutation::Create { path, .. } | WorkspaceMutation::Delete { path, .. } => {
                vec![path]
            }
            WorkspaceMutation::Modify { path, .. } => vec![path],
            WorkspaceMutation::Rename { from, to, .. } => vec![from, to],
        }
    }

    /// Reverse this mutation (for undo).
    pub fn reverse(&self) -> WorkspaceMutation {
        match self.clone() {
            WorkspaceMutation::Create { path, content } => {
                WorkspaceMutation::Delete { path, content }
            }
            WorkspaceMutation::Delete { path, content } => {
                WorkspaceMutation::Create { path, content }
            }
            WorkspaceMutation::Modify {
                path,
                before,
                after,
            } => WorkspaceMutation::Modify {
                path,
                before: after,
                after: before,
            },
            WorkspaceMutation::Rename { from, to, content } => WorkspaceMutation::Rename {
                from: to,
                to: from,
                content,
            },
        }
    }

    /// Estimate the size in bytes of the affected content (for diagnostics).
    pub fn estimated_size_hint(&self) -> &'static str {
        match self {
            WorkspaceMutation::Create { .. } => "new file",
            WorkspaceMutation::Delete { .. } => "deleted file",
            WorkspaceMutation::Modify { .. } => "modified file",
            WorkspaceMutation::Rename { .. } => "renamed file",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bp(s: &str) -> NormalizedPath {
        NormalizedPath::new(std::path::Path::new(s), std::path::Path::new("."))
    }

    fn bid(s: &str) -> BlobId {
        BlobId::new(s)
    }

    #[test]
    fn create_reverse_is_delete() {
        let m = WorkspaceMutation::Create {
            path: bp("new.txt"),
            content: bid("aaa"),
        };
        let r = m.reverse();
        assert!(matches!(r, WorkspaceMutation::Delete { .. }));
        assert_eq!(r.affected_paths(), vec![&bp("new.txt")]);
    }

    #[test]
    fn modify_reverse_swaps() {
        let m = WorkspaceMutation::Modify {
            path: bp("f.rs"),
            before: bid("a"),
            after: bid("b"),
        };
        let r = m.reverse();
        match r {
            WorkspaceMutation::Modify { before, after, .. } => {
                assert_eq!(before, bid("b"));
                assert_eq!(after, bid("a"));
            }
            _ => panic!("expected Modify"),
        }
    }

    #[test]
    fn rename_reverse_swaps_from_to() {
        let m = WorkspaceMutation::Rename {
            from: bp("old.txt"),
            to: bp("new.txt"),
            content: bid("c"),
        };
        let r = m.reverse();
        match r {
            WorkspaceMutation::Rename { from, to, .. } => {
                assert_eq!(from, bp("new.txt"));
                assert_eq!(to, bp("old.txt"));
            }
            _ => panic!("expected Rename"),
        }
    }
}
