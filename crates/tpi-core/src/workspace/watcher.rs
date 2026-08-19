//! Filesystem watcher for workspace change detection.
//!
//! Uses the `notify` crate to detect filesystem changes and mark paths dirty
//! for reconciliation. Watcher events are invalidation signals, NOT truth source.
//!
//! ```text
//! watch event → mark dirty → transaction reconcile → filesystem + index comparison
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Set of dirty paths that need reconciliation.
#[derive(Debug, Clone, Default)]
pub struct DirtySet {
    dirty: HashSet<String>,
}

impl DirtySet {
    pub fn new() -> Self {
        Self {
            dirty: HashSet::new(),
        }
    }

    /// Mark a path as dirty (needs content check).
    pub fn mark_dirty(&mut self, path: &str) {
        self.dirty.insert(path.to_string());
    }

    /// Take all dirty paths and clear the set.
    pub fn take_dirty(&mut self) -> HashSet<String> {
        std::mem::take(&mut self.dirty)
    }

    /// Check if a specific path is dirty.
    pub fn is_dirty(&self, path: &str) -> bool {
        self.dirty.contains(path)
    }

    /// Number of dirty paths.
    pub fn len(&self) -> usize {
        self.dirty.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }
}

/// Shared dirty set for cross-thread access.
pub type SharedDirtySet = Arc<Mutex<DirtySet>>;

/// Create a new shared dirty set.
pub fn new_shared_dirty_set() -> SharedDirtySet {
    Arc::new(Mutex::new(DirtySet::default()))
}

/// Workspace watcher configuration.
pub struct WatcherConfig {
    /// Paths to exclude from watching (e.g., `.git`, `target`, `node_modules`).
    pub exclude_patterns: Vec<String>,
    /// Debounce interval in milliseconds.
    pub debounce_ms: u64,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            exclude_patterns: vec![
                ".git".into(),
                "target".into(),
                "node_modules".into(),
                ".tpi".into(),
                ".tpi-workspace".into(),
            ],
            debounce_ms: 100,
        }
    }
}

/// Check if a path should be excluded from watching.
/// Checks all path components (not just the final file_name).
pub fn is_excluded(path: &Path, exclude: &[String]) -> bool {
    let well_known = [".git", "target", "node_modules", ".tpi", ".tpi-workspace"];
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        if well_known.contains(&name.as_ref()) {
            return true;
        }
        if exclude.iter().any(|pat| name.contains(pat.as_str())) {
            return true;
        }
    }
    false
}

/// Normalize a filesystem event path to a relative workspace path.
pub fn normalize_watch_path(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let normalized = relative.to_string_lossy().replace('\\', "/");
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn dirty_set_mark_and_take() {
        let mut ds = DirtySet::new();
        assert!(ds.is_empty());

        ds.mark_dirty("src/main.rs");
        ds.mark_dirty("src/lib.rs");
        assert_eq!(ds.len(), 2);
        assert!(ds.is_dirty("src/main.rs"));

        let taken = ds.take_dirty();
        assert_eq!(taken.len(), 2);
        assert!(ds.is_empty());
    }

    #[test]
    fn dirty_set_deduplicates() {
        let mut ds = DirtySet::new();
        ds.mark_dirty("src/main.rs");
        ds.mark_dirty("src/main.rs");
        assert_eq!(ds.len(), 1);
    }

    #[test]
    fn is_excluded_well_known() {
        assert!(is_excluded(Path::new("/proj/.git/HEAD"), &[]));
        assert!(is_excluded(Path::new("/proj/target/debug"), &[]));
        assert!(is_excluded(Path::new("/proj/node_modules/pkg"), &[]));
        assert!(!is_excluded(Path::new("/proj/src/main.rs"), &[]));
    }

    #[test]
    fn is_excluded_custom_patterns() {
        let patterns = vec!["dist".into(), "build".into()];
        assert!(is_excluded(Path::new("/proj/dist/index.js"), &patterns));
        assert!(is_excluded(Path::new("/proj/build/output"), &patterns));
        assert!(!is_excluded(Path::new("/proj/src/main.rs"), &patterns));
    }

    #[test]
    fn normalize_watch_path_works() {
        let root = Path::new("/workspace");
        let path = Path::new("/workspace/src/main.rs");
        assert_eq!(normalize_watch_path(path, root), Some("src/main.rs".into()));
    }

    #[test]
    fn normalize_watch_path_outside_root() {
        let root = Path::new("/workspace");
        let path = Path::new("/other/file.txt");
        assert_eq!(normalize_watch_path(path, root), None);
    }
}
