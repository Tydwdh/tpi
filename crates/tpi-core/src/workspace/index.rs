//! Persistent workspace file index.
//!
//! Tracks every file in the workspace with metadata (size, mtime, blob_id)
//! to enable incremental change detection without full-content scanning.

use super::blob::BlobStore;
use super::types::{BlobId, EntryKind, NormalizedPath};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A single tracked file entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub blob_id: Option<BlobId>,
    /// Whether this entry is within the tracking policy scope.
    pub tracked: bool,
}

impl FileEntry {
    pub fn mtime(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::new(self.mtime_secs, self.mtime_nanos)
    }

    /// Fast path: if mtime and size match, content is likely unchanged.
    pub fn metadata_matches(&self, meta: &std::fs::Metadata) -> bool {
        let size = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()));
        let current = (self.mtime_secs, self.mtime_nanos);
        self.size == size && (mtime == Some(current))
    }
}

/// Workspace file index — the persistent source of truth for what files exist
/// and what their content hashes are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceIndex {
    entries: HashMap<NormalizedPath, FileEntry>,
    #[serde(skip)]
    root: PathBuf,
}

/// Summary of changes detected during reconciliation.
#[derive(Debug, Clone)]
pub struct IndexDelta {
    pub created: Vec<(NormalizedPath, FileEntry)>,
    /// Modified paths with (before, after) — before 是修改前的旧 entry（含旧
    /// blob_id），供 undo 恢复 preimage；before 无 blob 时 blob_id 为 None。
    pub modified: Vec<(NormalizedPath, FileEntry, Option<FileEntry>)>,
    /// Deleted paths with their last known entry（含旧 blob_id，undo 恢复 preimage）。
    pub deleted: Vec<(NormalizedPath, Option<FileEntry>)>,
}

impl WorkspaceIndex {
    /// Create a new empty index rooted at the given path.
    pub fn new(root: PathBuf) -> Self {
        Self {
            entries: HashMap::new(),
            root,
        }
    }

    /// Full initial scan of the workspace, respecting the given exclusion patterns.
    /// Files matching `.git/` or patterns in `exclude` are skipped.
    pub fn initial_scan(
        root: &Path,
        exclude: &[String],
        max_files: usize,
        blob_store: &BlobStore,
    ) -> Result<Self, IndexError> {
        let mut index = Self::new(root.to_path_buf());
        let mut count = 0usize;

        Self::scan_recursive(
            root,
            root,
            &mut index.entries,
            exclude,
            blob_store,
            &mut count,
        )?;

        if count > max_files {
            return Err(IndexError::BudgetExceeded {
                tracked: count,
                limit: max_files,
            });
        }

        Ok(index)
    }

    fn scan_recursive(
        current: &Path,
        root: &Path,
        entries: &mut HashMap<NormalizedPath, FileEntry>,
        exclude: &[String],
        blob_store: &BlobStore,
        count: &mut usize,
    ) -> Result<(), IndexError> {
        let dir_entries = std::fs::read_dir(current).map_err(|e| IndexError::Io {
            context: format!("read_dir {}", current.display()),
            source: e,
        })?;

        for entry in dir_entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Skip well-known internal directories.
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                && (name_str == ".git"
                    || name_str == ".tpi"
                    || name_str == ".tpi-workspace"
                    || name_str == "blob")
            {
                continue;
            }

            // Check exclusion patterns (simple substring match for now).
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_str = rel.to_string_lossy();
            if exclude.iter().any(|pat| rel_str.contains(pat.as_str())) {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue, // Permission error or disappeared.
            };

            let norm = NormalizedPath::new(&path, root);

            if meta.is_dir() {
                entries.insert(
                    norm,
                    FileEntry {
                        kind: EntryKind::Directory,
                        size: 0,
                        mtime_secs: 0,
                        mtime_nanos: 0,
                        blob_id: None,
                        tracked: true,
                    },
                );
                Self::scan_recursive(&path, root, entries, exclude, blob_store, count)?;
            } else if meta.is_file() {
                *count += 1;
                let size = meta.len();

                // Hash content into blob store.
                let content = std::fs::read(&path).map_err(|e| IndexError::Io {
                    context: format!("read {}", path.display()),
                    source: e,
                })?;
                let blob_id = blob_store
                    .put(&content)
                    .map_err(|e| IndexError::Blob(e.to_string()))?;

                let (mtime_secs, mtime_nanos) = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| (d.as_secs(), d.subsec_nanos()))
                    .unwrap_or((0, 0));

                entries.insert(
                    norm,
                    FileEntry {
                        kind: EntryKind::File,
                        size,
                        mtime_secs,
                        mtime_nanos,
                        blob_id: Some(blob_id),
                        tracked: true,
                    },
                );
            }
            // Symlinks: skip for now (EntryKind::Symlink support is reserved).
        }

        Ok(())
    }

    /// Incremental reconciliation: compare current filesystem against index.
    /// Uses mtime+size fast path where possible, hashes only changed files.
    pub fn reconcile(
        &mut self,
        exclude: &[String],
        blob_store: &BlobStore,
    ) -> Result<IndexDelta, IndexError> {
        let mut current_files = HashMap::new();
        Self::scan_recursive(
            &self.root,
            &self.root,
            &mut current_files,
            exclude,
            blob_store,
            &mut 0,
        )?;
        self.reconcile_with(&current_files, blob_store)
    }

    /// Reconcile against a pre-scanned set of current files.
    fn reconcile_with(
        &mut self,
        current: &HashMap<NormalizedPath, FileEntry>,
        blob_store: &BlobStore,
    ) -> Result<IndexDelta, IndexError> {
        let mut delta = IndexDelta {
            created: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        };

        // Detect created and modified entries.
        for (path, new_entry) in current {
            match self.entries.get(path) {
                Some(existing) => {
                    // Fast path: metadata unchanged → skip content check.
                    // We can't do the fast path here because we don't have the
                    // raw SystemTime from the index (we stored split secs/nanos).
                    // Instead compare size + mtime fields directly.
                    if existing.size == new_entry.size
                        && existing.mtime_secs == new_entry.mtime_secs
                        && existing.mtime_nanos == new_entry.mtime_nanos
                    {
                        // Metadata identical → assume content unchanged.
                        continue;
                    }
                    // Metadata changed → re-hash.
                    let content = std::fs::read(self.root.join(path.as_str())).map_err(|e| {
                        IndexError::Io {
                            context: format!("read changed file {}", path),
                            source: e,
                        }
                    })?;
                    let blob_id = blob_store
                        .put(&content)
                        .map_err(|e| IndexError::Blob(e.to_string()))?;

                    let mut updated = new_entry.clone();
                    updated.blob_id = Some(blob_id);
                    // before = 修改前的旧 entry（携带旧 blob_id，preimage 供 undo）。
                    delta
                        .modified
                        .push((path.clone(), updated.clone(), Some(existing.clone())));
                    self.entries.insert(path.clone(), updated);
                }
                None => {
                    // New file.
                    self.entries.insert(path.clone(), new_entry.clone());
                    delta.created.push((path.clone(), new_entry.clone()));
                }
            }
        }

        // Detect deleted entries.
        let tracked_old: Vec<NormalizedPath> = self
            .entries
            .iter()
            .filter(|(_, e)| e.tracked)
            .map(|(p, _)| p.clone())
            .collect();

        for old_path in tracked_old {
            if !current.contains_key(&old_path) {
                // before = 删除前的最后一个 entry（旧 blob_id，preimage 供 undo）。
                let before = self.entries.get(&old_path).cloned();
                self.entries.remove(&old_path);
                delta.deleted.push((old_path, before));
            }
        }

        Ok(delta)
    }

    /// Get the blob_id for a file's current content.
    pub fn get_blob_id(&self, path: &NormalizedPath) -> Option<&BlobId> {
        self.entries.get(path).and_then(|e| e.blob_id.as_ref())
    }

    /// Get the entry for a path.
    pub fn get_entry(&self, path: &NormalizedPath) -> Option<&FileEntry> {
        self.entries.get(path)
    }

    /// Insert or update an entry from filesystem metadata.
    pub fn upsert_file(
        &mut self,
        path: &NormalizedPath,
        meta: std::fs::Metadata,
        blob_id: Option<BlobId>,
    ) {
        let (mtime_secs, mtime_nanos) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        self.entries.insert(
            path.clone(),
            FileEntry {
                kind: EntryKind::File,
                size: meta.len(),
                mtime_secs,
                mtime_nanos,
                blob_id,
                tracked: true,
            },
        );
    }

    /// Remove an entry from the index.
    pub fn remove_file(&mut self, path: &NormalizedPath) {
        self.entries.remove(path);
    }

    /// Number of tracked entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All tracked paths.
    pub fn paths(&self) -> impl Iterator<Item = &NormalizedPath> {
        self.entries.keys()
    }

    /// Persist index to disk as JSON.
    pub fn persist(&self, path: &Path) -> Result<(), IndexError> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| IndexError::Serialize(e.to_string()))?;
        let parent = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| IndexError::Io {
            context: "create index dir".into(),
            source: e,
        })?;
        std::fs::write(path, json).map_err(|e| IndexError::Io {
            context: format!("write index to {}", path.display()),
            source: e,
        })
    }

    /// Load index from disk.
    pub fn load(path: &Path, root: PathBuf) -> Result<Option<Self>, IndexError> {
        if !path.exists() {
            return Ok(None);
        }
        let json = std::fs::read_to_string(path).map_err(|e| IndexError::Io {
            context: format!("read index from {}", path.display()),
            source: e,
        })?;
        let mut index: Self =
            serde_json::from_str(&json).map_err(|e| IndexError::Deserialize(e.to_string()))?;
        index.root = root;
        Ok(Some(index))
    }
}

/// Errors from workspace index operations.
#[derive(Debug)]
pub enum IndexError {
    Io {
        context: String,
        source: std::io::Error,
    },
    Blob(String),
    BudgetExceeded {
        tracked: usize,
        limit: usize,
    },
    Serialize(String),
    Deserialize(String),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Io { context, source } => write!(f, "{context}: {source}"),
            IndexError::Blob(msg) => write!(f, "blob store: {msg}"),
            IndexError::BudgetExceeded { tracked, limit } => {
                write!(
                    f,
                    "Workspace indexing incomplete. Tracked files: {tracked}, limit: {limit}. \
                     Exact workspace undo may be unavailable for unindexed paths."
                )
            }
            IndexError::Serialize(msg) => write!(f, "serialize index: {msg}"),
            IndexError::Deserialize(msg) => write!(f, "deserialize index: {msg}"),
        }
    }
}

impl std::error::Error for IndexError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_root() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"world").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), b"nested").unwrap();
        dir
    }

    #[test]
    fn initial_scan_finds_files() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();
        assert!(index.len() >= 3);
    }

    #[test]
    fn budget_exceeded() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let result = WorkspaceIndex::initial_scan(dir.path(), &[], 2, &blob);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::BudgetExceeded { tracked, limit } => {
                assert!(tracked > limit);
            }
            other => panic!("expected BudgetExceeded, got {other}"),
        }
    }

    #[test]
    fn exclude_pattern() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let index =
            WorkspaceIndex::initial_scan(dir.path(), &["sub".into()], 100_000, &blob).unwrap();
        // sub/c.txt should be excluded.
        let norm = NormalizedPath::new(&dir.path().join("sub/c.txt"), dir.path());
        assert!(index.get_entry(&norm).is_none());
    }

    #[test]
    fn persist_and_load() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        let idx_path = dir.path().join("index.json");
        index.persist(&idx_path).unwrap();

        let loaded = WorkspaceIndex::load(&idx_path, dir.path().to_path_buf())
            .unwrap()
            .unwrap();
        assert_eq!(loaded.len(), index.len());
    }

    #[test]
    fn reconcile_detects_new_file() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();
        let before = index.len();

        // Add a new file.
        std::fs::write(dir.path().join("new.txt"), b"new").unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        assert_eq!(index.len(), before + 1);
        assert_eq!(delta.created.len(), 1);
        assert_eq!(delta.created[0].0.as_str(), "new.txt");
    }

    #[test]
    fn reconcile_detects_deleted_file() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        assert_eq!(delta.deleted.len(), 1);
        assert_eq!(delta.deleted[0].0.as_str(), "a.txt");
        // preimage 必须携带删除前的 entry（undo 恢复内容的前提）。
        let (_, before) = &delta.deleted[0];
        assert!(
            before.as_ref().and_then(|e| e.blob_id.clone()).is_some(),
            "deleted delta 必须携带 before blob (preimage)"
        );
    }

    #[test]
    fn reconcile_detects_modified_file() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::write(dir.path().join("a.txt"), b"CHANGED").unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.modified[0].0.as_str(), "a.txt");
        // preimage 必须携带修改前的旧 entry（含旧 blob_id）。
        let (_, after, before) = &delta.modified[0];
        assert!(after.blob_id.is_some(), "after 必须有 blob");
        assert!(
            before.as_ref().and_then(|e| e.blob_id.clone()).is_some(),
            "modified delta 必须携带 before blob (preimage)"
        );
        assert_ne!(
            before.as_ref().and_then(|e| e.blob_id.clone()),
            after.blob_id,
            "before 与 after blob 必须不同（内容确实变了）"
        );
    }

    #[test]
    fn reconcile_no_changes() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        let delta = index.reconcile(&[], &blob).unwrap();
        assert!(delta.created.is_empty());
        assert!(delta.modified.is_empty());
        assert!(delta.deleted.is_empty());
    }

    /// 端到端：bash 式外部覆盖 a.txt 后 reconcile，before blob 必须 round-trip
    /// 回原始字节（undo 依赖它恢复 preimage）。这是审查断言的闭环验证。
    #[test]
    fn reconcile_modified_preimage_roundtrips_original_bytes() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 外部覆盖（模拟 bash echo BBB > a.txt）。
        std::fs::write(dir.path().join("a.txt"), b"HELLO-CHANGED").unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        let (_, after, before) = &delta.modified[0];
        let after_id = after.blob_id.clone().unwrap();
        let before_id = before.clone().unwrap().blob_id.clone().unwrap();

        // before blob 必须能取回原始 "hello" 字节（undo 恢复的内容）。
        let restored = blob.get(&before_id).unwrap();
        assert_eq!(restored, b"hello");
        // after blob 是新内容。
        let after_bytes = blob.get(&after_id).unwrap();
        assert_eq!(after_bytes, b"HELLO-CHANGED");
        // before != after。
        assert_ne!(before_id, after_id);
    }

    /// 端到端：外部删除 a.txt 后 reconcile，before blob 取回原始字节。
    #[test]
    fn reconcile_deleted_preimage_roundtrips_original_bytes() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 外部删除（模拟 bash rm a.txt）。
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        let (_, before) = &delta.deleted[0];
        let before_id = before.clone().unwrap().blob_id.clone().unwrap();
        // before blob 取回原始 "hello" 字节（undo create 恢复的内容）。
        let restored = blob.get(&before_id).unwrap();
        assert_eq!(restored, b"hello");
    }
}
