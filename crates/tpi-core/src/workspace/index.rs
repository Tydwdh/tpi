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

/// A modified path: `before`（旧 entry，含旧 blob_id）驱动 undo preimage。
#[derive(Debug, Clone)]
pub struct ModifiedEntry {
    pub path: NormalizedPath,
    pub before: FileEntry,
    pub after: FileEntry,
}

/// A deleted path: `before`（最后已知 entry，含旧 blob_id）驱动 undo preimage。
/// 只含 tracked 文件与目录；无 untracked/未知条目 —— 从定义上排除
/// "deleted but no before" impossible state。
#[derive(Debug, Clone)]
pub struct DeletedEntry {
    pub path: NormalizedPath,
    pub before: FileEntry,
}

/// Summary of changes detected during reconciliation.
///
/// 语义（§workspace）：
/// - created  无 before（新建）
/// - modified 必有 before（undo 恢复旧内容）
/// - deleted  必有 before（undo 恢复旧内容）
/// 因此这里不使用 `Option<FileEntry>`，通过类型让不可能状态不可表达。
#[derive(Debug, Clone)]
pub struct IndexDelta {
    pub created: Vec<(NormalizedPath, FileEntry)>,
    pub modified: Vec<ModifiedEntry>,
    pub deleted: Vec<DeletedEntry>,
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

    /// 递归发现 workspace 路径与 metadata（path/kind/size/mtime），**不读取文件正文、
    /// 不 hash**。返回 path → metadata。这是 P1 拆分出的第一阶段：metadata-only scan。
    fn scan_metadata_recursive(
        current: &Path,
        root: &Path,
        out: &mut HashMap<NormalizedPath, std::fs::Metadata>,
        exclude: &[String],
    ) -> Result<(), IndexError> {
        let dir_entries = std::fs::read_dir(current).map_err(|e| IndexError::Io {
            context: format!("read_dir {}", current.display()),
            source: e,
        })?;

        for entry in dir_entries.flatten() {
            let path = entry.path();
            let name_str = entry.file_name().to_string_lossy().to_string();

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
                out.insert(norm, meta);
                Self::scan_metadata_recursive(&path, root, out, exclude)?;
            } else if meta.is_file() {
                out.insert(norm, meta);
            }
            // Symlinks: skip for now.
        }
        Ok(())
    }

    /// 读取单个**绝对路径**文件的内容并写入 blob store，返回 blob id。
    /// 只在文件确实 changed/new 时调用（P1：metadata 比对后按需 hash）。
    fn hash_file_into_blob(abs_path: &Path, blob_store: &BlobStore) -> Result<BlobId, IndexError> {
        let content = std::fs::read(abs_path).map_err(|e| IndexError::Io {
            context: format!("read {}", abs_path.display()),
            source: e,
        })?;
        blob_store
            .put(&content)
            .map_err(|e| IndexError::Blob(e.to_string()))
    }

    /// metadata + (对每个文件) read+hash 一次写入 entries。用于 initial_scan
    /// （没有 previous index 可比对，必须全量 hash）。
    fn scan_recursive(
        current: &Path,
        root: &Path,
        entries: &mut HashMap<NormalizedPath, FileEntry>,
        exclude: &[String],
        blob_store: &BlobStore,
        count: &mut usize,
    ) -> Result<(), IndexError> {
        let mut metas = HashMap::new();
        Self::scan_metadata_recursive(current, root, &mut metas, exclude)?;
        for (path, meta) in metas {
            if meta.is_dir() {
                entries.insert(
                    path,
                    FileEntry {
                        kind: EntryKind::Directory,
                        size: 0,
                        mtime_secs: 0,
                        mtime_nanos: 0,
                        blob_id: None,
                        tracked: true,
                    },
                );
            } else if meta.is_file() {
                *count += 1;
                let abs = root.join(path.as_str());
                let blob_id = Self::hash_file_into_blob(&abs, blob_store)?;
                let (mtime_secs, mtime_nanos) = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| (d.as_secs(), d.subsec_nanos()))
                    .unwrap_or((0, 0));
                entries.insert(
                    path,
                    FileEntry {
                        kind: EntryKind::File,
                        size: meta.len(),
                        mtime_secs,
                        mtime_nanos,
                        blob_id: Some(blob_id),
                        tracked: true,
                    },
                );
            }
        }
        Ok(())
    }

    /// Incremental reconciliation: compare current filesystem against index.
    /// P1：先 metadata-only scan（不读正文、不 hash），再对 created/modified 的
    /// **文件**按需 read+hash。未变化的文件（mtime+size 匹配）完全不读内容。
    /// 复杂度：O(全部 metadata + changed bytes)，而不是 O(全部 file bytes)。
    pub fn reconcile(
        &mut self,
        exclude: &[String],
        blob_store: &BlobStore,
    ) -> Result<IndexDelta, IndexError> {
        let mut metas = HashMap::new();
        Self::scan_metadata_recursive(&self.root, &self.root, &mut metas, exclude)?;
        self.reconcile_with_metadata(&metas, blob_store)
    }

    /// 以 metadata map 驱动 reconcile：只对 created/modified 的**文件**读正文+hash。
    fn reconcile_with_metadata(
        &mut self,
        metas: &HashMap<NormalizedPath, std::fs::Metadata>,
        blob_store: &BlobStore,
    ) -> Result<IndexDelta, IndexError> {
        let mut delta = IndexDelta {
            created: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        };

        for (path, meta) in metas {
            if meta.is_dir() {
                // 目录：只保证 index 里有记录，不产出 change 事件（也不 hash）。
                self.entries
                    .entry(path.clone())
                    .or_insert_with(|| FileEntry {
                        kind: EntryKind::Directory,
                        size: 0,
                        mtime_secs: 0,
                        mtime_nanos: 0,
                        blob_id: None,
                        tracked: true,
                    });
                continue;
            }
            if !meta.is_file() {
                continue; // symlink / other：跳过。
            }

            let (mtime_secs, mtime_nanos) = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs(), d.subsec_nanos()))
                .unwrap_or((0, 0));
            let size = meta.len();

            match self.entries.get(path) {
                Some(existing) => {
                    // Fast path: size + mtime 匹配 → 内容可认为未变，跳过 hash。
                    if existing.size == size
                        && existing.mtime_secs == mtime_secs
                        && existing.mtime_nanos == mtime_nanos
                        && existing.kind == EntryKind::File
                    {
                        continue;
                    }
                    // Changed file → 读取正文 + hash（绝对路径）。
                    let abs = self.root.join(path.as_str());
                    let blob_id = Self::hash_file_into_blob(&abs, blob_store)?;
                    let updated = FileEntry {
                        kind: EntryKind::File,
                        size,
                        mtime_secs,
                        mtime_nanos,
                        blob_id: Some(blob_id.clone()),
                        tracked: true,
                    };
                    delta.modified.push(ModifiedEntry {
                        path: path.clone(),
                        before: existing.clone(),
                        after: updated.clone(),
                    });
                    self.entries.insert(path.clone(), updated);
                }
                None => {
                    // New file → 读取正文 + hash（绝对路径）。
                    let abs = self.root.join(path.as_str());
                    let blob_id = Self::hash_file_into_blob(&abs, blob_store)?;
                    let new_entry = FileEntry {
                        kind: EntryKind::File,
                        size,
                        mtime_secs,
                        mtime_nanos,
                        blob_id: Some(blob_id),
                        tracked: true,
                    };
                    self.entries.insert(path.clone(), new_entry.clone());
                    delta.created.push((path.clone(), new_entry));
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
            if metas.contains_key(&old_path) {
                continue;
            }
            let before = self
                .entries
                .remove(&old_path)
                .expect("tracked entry present");
            // 只为**文件**产出 Delete mutation：文件 has blob_id = preimage 供 undo。
            // 目录不产出 Delete（其内部文件各自带 preimage；目录删除 undo = mkdir，无
            // blob 需要），从而在类型+数据上都消灭 BlobId("") sentinel。
            if before.kind == EntryKind::File {
                delta.deleted.push(DeletedEntry {
                    path: old_path,
                    before,
                });
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
        assert_eq!(delta.deleted[0].path.as_str(), "a.txt");
        // preimage 必须携带删除前的 entry（undo 恢复内容的前提）。
        assert!(
            delta.deleted[0].before.blob_id.is_some(),
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
        assert_eq!(delta.modified[0].path.as_str(), "a.txt");
        // preimage 必须携带修改前的旧 entry（含旧 blob_id）。
        let m = &delta.modified[0];
        assert!(m.after.blob_id.is_some(), "after 必须有 blob");
        assert!(
            m.before.blob_id.is_some(),
            "modified delta 必须携带 before blob"
        );
        assert_ne!(
            m.before.blob_id, m.after.blob_id,
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

        let m = &delta.modified[0];
        let after_id = m.after.blob_id.clone().unwrap();
        let before_id = m.before.blob_id.clone().unwrap();

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

        let before_id = delta.deleted[0].before.blob_id.clone().unwrap();
        // before blob 取回原始 "hello" 字节（undo create 恢复的内容）。
        let restored = blob.get(&before_id).unwrap();
        assert_eq!(restored, b"hello");
    }

    /// rm -rf 目录：deleted delta 中每个条目都必须带 before blob，
    /// 不产出无 preimage 的目录删除（BlobId("") sentinel 被结构性消灭）。
    #[test]
    fn reconcile_rm_rf_directory_emits_only_preimage_files() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 删除整个 sub/ 目录（内含 sub/c.txt）。
        std::fs::remove_dir_all(dir.path().join("sub")).unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        // 只有带 preimage 的**文件**删除被产出；目录本身不产出 Delete。
        assert!(!delta.deleted.is_empty(), "应有文件删除");
        for d in &delta.deleted {
            assert_eq!(
                d.before.kind,
                super::EntryKind::File,
                "deleted 只应含文件（path={}, kind={:?}）",
                d.path,
                d.before.kind
            );
            assert!(
                d.before.blob_id.is_some(),
                "deleted 每个文件都必须有 before blob: {}",
                d.path
            );
        }
        // 目录本身（sub/）不应出现在 deleted（无 preimage 可携）。
        assert!(
            delta.deleted.iter().all(|d| d.path.as_str() != "sub"),
            "目录不应出现在 deleted delta"
        );
    }

    /// 被删文件 preimage 必须取回原始字节（rm -rf 场景）。
    #[test]
    fn reconcile_rm_rf_directory_preimage_roundtrips() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::remove_dir_all(dir.path().join("sub")).unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        // sub/c.txt 的 before blob 取回 "nested"。
        let c = delta
            .deleted
            .iter()
            .find(|d| d.path.as_str().ends_with("c.txt"))
            .unwrap();
        let restored = blob.get(&c.before.blob_id.clone().unwrap()).unwrap();
        assert_eq!(restored, b"nested");
    }

    /// P1：无变化 reconcile 不重新 hash / 写 blob——blob store 条目数不变。
    /// 这是 metadata-scan 与 content-hash 拆分的直接收益。
    #[test]
    fn reconcile_unchanged_reuses_blobs_without_rehash() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();
        let blobs_after_initial = blob.count().unwrap();
        assert!(blobs_after_initial >= 3, "初始扫描至少 hash 3 个文件");

        // 无任何修改 → reconcile 不应产生 change，也绝不应新增 blob。
        let delta = index.reconcile(&[], &blob).unwrap();
        assert!(delta.created.is_empty());
        assert!(delta.modified.is_empty());
        assert!(delta.deleted.is_empty());
        assert_eq!(
            blob.count().unwrap(),
            blobs_after_initial,
            "无变化时不得重新 hash 文件（metadata-scan fast path 应跳过）"
        );
    }

    /// P1：只改 1 个文件时，blob store 只新增 1 个 after blob（不改的文件不重 hash）。
    /// 说明 reconcile 从 O(all bytes) 收敛为 O(changed bytes)。
    #[test]
    fn reconcile_only_hashes_changed_file() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();
        let before = blob.count().unwrap();

        // 只改 b.txt；a.txt / sub/c.txt 不动。
        std::fs::write(dir.path().join("b.txt"), b"WORLD-CHANGED").unwrap();
        let delta = index.reconcile(&[], &blob).unwrap();

        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.modified[0].path.as_str(), "b.txt");
        // 仅新增 1 个 blob（b.txt 的 after）；a.txt 与 sub/c.txt 未重 hash。
        assert_eq!(
            blob.count().unwrap(),
            before + 1,
            "只应新增被改文件的 1 个 after blob"
        );
    }
}
