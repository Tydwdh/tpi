//! Policy types: reversibility guarantees, safety policies, tracking scope.

use serde::{Deserialize, Serialize};

/// Whether a transaction can be undone, partially, or not at all.
///
/// 契约（§workspace）：`Exact` 表示 **tracked regular files 的路径与字节内容**
/// 可以精确恢复——不保证空目录、或目录的权限/mtime 等 metadata。TPI 通过恢复
/// 文件内容顺带重建父目录（create_dir_all），但空目录与目录 metadata 不会被
/// 快照/恢复。刻意不做目录级 snapshot（`CreateDir/DeleteDir`），保持模型简单
/// （代码任务关心的是文件内容）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Reversibility {
    /// All affected tracked regular files have preimages in CAS — their paths
    /// and byte contents can be restored exactly. Empty dirs and dir metadata
    /// are NOT guaranteed (by design).
    Exact,
    /// Some files cannot be undone (e.g. untracked paths changed).
    Partial { reasons: Vec<ReversibilityIssue> },
    /// Undo is not possible for this transaction.
    Unavailable { reasons: Vec<ReversibilityIssue> },
}

impl Reversibility {
    pub fn is_exact(&self) -> bool {
        matches!(self, Reversibility::Exact)
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Reversibility::Unavailable { .. })
    }
}

/// A specific reason why undo may be incomplete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReversibilityIssue {
    /// The path was not tracked by the workspace index.
    UntrackedPath { path: String },
    /// Workspace budget was exceeded during tracking.
    BudgetExceeded { tracked: usize, limit: usize },
    /// The file was externally modified after the transaction.
    ExternalMutation { path: String },
    /// The preimage content was lost (blob missing from CAS).
    ContentLost { path: String },
    /// Command produced output in an untracked location.
    UnknownOutput { path: String },
}

impl std::fmt::Display for ReversibilityIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReversibilityIssue::UntrackedPath { path } => {
                write!(f, "untracked path: {path}")
            }
            ReversibilityIssue::BudgetExceeded { tracked, limit } => {
                write!(f, "budget exceeded: {tracked} files tracked, limit {limit}")
            }
            ReversibilityIssue::ExternalMutation { path } => {
                write!(f, "external modification: {path}")
            }
            ReversibilityIssue::ContentLost { path } => {
                write!(f, "content lost: {path}")
            }
            ReversibilityIssue::UnknownOutput { path } => {
                write!(f, "unknown output: {path}")
            }
        }
    }
}

/// Safety policy for workspace mutations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum MutationSafetyPolicy {
    /// Default: try to track everything, but never block execution because
    /// of tracking failures. Reversibility status is reported explicitly.
    #[default]
    BestEffort,
    /// Strict: refuse to execute commands that may produce workspace mutations
    /// if exact undo cannot be guaranteed.
    RequireExactUndo,
}

/// Controls which paths are tracked for undo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackingPolicy {
    /// Glob patterns for paths to include (empty = all).
    pub include: Vec<String>,
    /// Glob patterns for paths to exclude.
    pub exclude: Vec<String>,
    /// Whether to use the project's .gitignore for exclusions.
    pub use_project_gitignore: bool,
    /// Whether to use the global gitignore for exclusions.
    pub use_global_gitignore: bool,
    /// Maximum number of files to index (budget).
    pub max_tracked_files: usize,
    /// Maximum total bytes of file content to track.
    pub max_index_bytes: u64,
    /// Maximum size of a single file to track.
    pub max_single_file_bytes: u64,
    /// Create a full baseline snapshot every N checkpoints.
    pub snapshot_interval: usize,
    /// Well-known directories always excluded from tracking.
    pub always_excluded: Vec<String>,
}

impl Default for TrackingPolicy {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            use_project_gitignore: true,
            use_global_gitignore: false,
            max_tracked_files: 100_000,
            max_index_bytes: 512 * 1024 * 1024,      // 512 MiB
            max_single_file_bytes: 64 * 1024 * 1024, // 64 MiB
            snapshot_interval: 20,
            always_excluded: vec![".git".into(), ".tpi".into(), ".tpi-workspace".into()],
        }
    }
}

impl TrackingPolicy {
    /// Effective exclusion list: always_excluded + user exclude patterns.
    pub fn effective_exclude(&self) -> Vec<String> {
        let mut patterns = self.always_excluded.clone();
        patterns.extend(self.exclude.clone());
        patterns
    }
}

/// Result of an undo or redo operation.
#[derive(Debug, Clone)]
pub enum UndoResult {
    /// Transaction was undone/redone successfully.
    Applied {
        transaction_id: String,
        affected_paths: Vec<String>,
    },
    /// Conflict detected — no files were modified.
    Conflict {
        transaction_id: String,
        conflicts: Vec<WorkspaceConflict>,
    },
    /// Undo/redo cannot be performed.
    Unavailable {
        transaction_id: String,
        reasons: Vec<ReversibilityIssue>,
    },
}

/// A specific conflict between expected and actual file state.
#[derive(Debug, Clone)]
pub struct WorkspaceConflict {
    pub path: String,
    pub expected_blob: String,
    pub actual_blob: String,
    pub modified_by: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reversibility_exact() {
        let r = Reversibility::Exact;
        assert!(r.is_exact());
        assert!(!r.is_unavailable());
    }

    #[test]
    fn reversibility_partial() {
        let r = Reversibility::Partial {
            reasons: vec![ReversibilityIssue::UntrackedPath {
                path: "temp.out".into(),
            }],
        };
        assert!(!r.is_exact());
        assert!(!r.is_unavailable());
    }

    #[test]
    fn default_policy_is_best_effort() {
        assert_eq!(
            MutationSafetyPolicy::default(),
            MutationSafetyPolicy::BestEffort
        );
    }

    #[test]
    fn tracking_policy_defaults() {
        let p = TrackingPolicy::default();
        assert!(p.use_project_gitignore);
        assert_eq!(p.max_tracked_files, 100_000);
        assert!(p.always_excluded.contains(&".git".into()));
    }

    #[test]
    fn effective_exclude_merges() {
        let p = TrackingPolicy {
            exclude: vec!["dist".into()],
            ..Default::default()
        };
        let eff = p.effective_exclude();
        assert!(eff.contains(&".git".into()));
        assert!(eff.contains(&"dist".into()));
    }
}
