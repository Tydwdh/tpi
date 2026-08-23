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

/// 判断 `child` 是否等于 `ancestor` 或位于其子树内（path component 边界匹配，
/// 兼容 `/` 与 `\` 两种 separator）。用于 dirty scope 展开/prefix collapse。
fn is_same_or_under(child: &str, ancestor: &str) -> bool {
    if child == ancestor {
        return true;
    }
    let direct = format!("{ancestor}/");
    let win = format!("{ancestor}\\");
    child.starts_with(&direct) || child.starts_with(&win)
}

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

    /// 增量 reconcile：只处理给定的一批 dirty path（K 个），不扫描整个 workspace。
    ///
    /// 语义：每个 dirty path 都是“可能变化”而非“一定变化”——存在性/元数据以
    /// 文件系统为准。对每个 path：
    /// - 存在且为目录：确保 index 有目录 entry（不产 change，不 hash）
    /// - 存在且为文件：与 index 比对，mtime+size 相同则跳过，否则 read+hash
    ///   → Created / Modified
    /// - 不存在：index 若有 tracked 文件 → Deleted（带 preimage）；否则忽略
    ///
    /// DirtySet 只缩小候选范围；无误报/漏报记录差异，复杂度 O(K metadata + changed bytes)。
    /// 增量 reconcile：只处理给定的一批 dirty path（可能含目录），并做 prefix
    /// collapse——若 dirty 同时含 `src/` 与其 descendants，subtree reconcile 一次
    /// 覆盖全部，避免重复扫描。复杂度 O(dirty scopes metadata + changed bytes)。
    pub fn reconcile_paths(
        &mut self,
        paths: &[NormalizedPath],
        blob_store: &BlobStore,
    ) -> Result<IndexDelta, IndexError> {
        let mut delta = IndexDelta {
            created: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        };

        // 按 path 字符串排序，便于 prefix collapse（目录 scope 覆盖其 descendants）。
        let mut unique = paths.to_vec();
        unique.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        unique.dedup();

        let mut processed: Vec<String> = Vec::new();
        for path in unique {
            // 若某个已处理 path（通常是目录 scope）是本 path 的祖先，本 path 由
            // 那次 subtree reconcile 覆盖，跳过（prefix collapse）。边界用
            // `prefix/`，避免把 `src` 误当 `src2/x` 的祖先。
            let s = path.as_str();
            let covered = processed.iter().any(|anc| is_same_or_under(s, anc));
            if covered {
                continue;
            }
            self.reconcile_one_path(&path, blob_store, &mut delta)?;
            processed.push(path.as_str().to_string());
        }
        Ok(delta)
    }

    /// 单个 dirty path 的 reconcile。
    /// 语义（P2.6）：dirty path 代表“此 filesystem scope 必须重新验证”——
    /// - 存在文件 → reconcile_file（mtime+size fast path，只 hash changed/new）
    /// - 存在目录 → reconcile_subtree（递归 subtree，与 full reconcile 在最终状态
    ///   语义上等价，但只扫 subtree，复杂度 O(subtree metadata + changed bytes)）
    /// - 不存在 → 若 index 有该 path 的 descendants，全部产出 Delete（用已存 before
    ///   blob，无需 scan filesystem）；否则单文件 Delete
    fn reconcile_one_path(
        &mut self,
        path: &NormalizedPath,
        blob_store: &BlobStore,
        delta: &mut IndexDelta,
    ) -> Result<(), IndexError> {
        let abs = self.root.join(path.as_str());
        match std::fs::metadata(&abs) {
            Ok(meta) if meta.is_dir() => self.reconcile_subtree(path, blob_store, delta),
            Ok(meta) if meta.is_file() => self.reconcile_file(path, &meta, blob_store, delta),
            Ok(_) => Ok(()), // symlink / other：跳过。
            Err(_) => {
                // 不存在（或权限错误）。
                // 目录：删除 index 下所有 descendants（带 before preimage），无需 scan。
                // 文件：单删。
                let descendants: Vec<NormalizedPath> = self
                    .entries
                    .keys()
                    .filter(|k| is_same_or_under(k.as_str(), path.as_str()))
                    .cloned()
                    .collect();
                for d in descendants {
                    let before = self.entries.remove(&d);
                    if let Some(before) = before {
                        if before.kind == EntryKind::File {
                            delta.deleted.push(DeletedEntry { path: d, before });
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// 单文件 reconcile：mtime+size 匹配则跳过 hash，否则 read+hash → Modified/Created。
    fn reconcile_file(
        &mut self,
        path: &NormalizedPath,
        meta: &std::fs::Metadata,
        blob_store: &BlobStore,
        delta: &mut IndexDelta,
    ) -> Result<(), IndexError> {
        let (mtime_secs, mtime_nanos) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        let size = meta.len();
        let abs = self.root.join(path.as_str());

        match self.entries.get(path) {
            Some(existing) => {
                if existing.size == size
                    && existing.mtime_secs == mtime_secs
                    && existing.mtime_nanos == mtime_nanos
                    && existing.kind == EntryKind::File
                {
                    return Ok(());
                }
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
        Ok(())
    }

    /// 子树 reconcile：递归扫描 `path` 下的 metadata，与 index 中该 prefix 的
    /// entries 比对，产出 created/modified/deleted。删除的 descendants 携带
    /// before preimage。只读 changed/new file bytes。
    fn reconcile_subtree(
        &mut self,
        path: &NormalizedPath,
        blob_store: &BlobStore,
        delta: &mut IndexDelta,
    ) -> Result<(), IndexError> {
        // path 保证是存在目录；先确保目录 entry 本身存在（不产 change）。
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

        // metadata-only scan subtree。
        let abs = self.root.join(path.as_str());
        let mut subtree_metas = HashMap::new();
        Self::scan_metadata_recursive(&abs, &self.root, &mut subtree_metas, &[])?;

        // 1) created / modified：subtree 内现有条目。目录只确保 entry 存在（不产 change，
        //   与 full reconcile 语义一致）；文件走 reconcile_file。
        for (p, meta) in &subtree_metas {
            if meta.is_dir() {
                self.entries.entry(p.clone()).or_insert_with(|| FileEntry {
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_secs: 0,
                    mtime_nanos: 0,
                    blob_id: None,
                    tracked: true,
                });
            } else if meta.is_file() {
                self.reconcile_file(p, meta, blob_store, delta)?;
            }
        }

        // 2) deleted：index 中该 path 子树内（不含树根自身——树根 entry 由上面 entry()
        //    保证存在）、但 filesystem 已不存在的 tracked 文件。
        let stale: Vec<NormalizedPath> = self
            .entries
            .keys()
            .filter(|k| {
                k.as_str() != path.as_str()
                    && is_same_or_under(k.as_str(), path.as_str())
                    && !subtree_metas.contains_key(*k)
            })
            .cloned()
            .collect();
        for d in stale {
            let before = self.entries.remove(&d);
            if let Some(before) = before {
                if before.kind == EntryKind::File {
                    delta.deleted.push(DeletedEntry { path: d, before });
                }
            }
        }
        Ok(())
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

    // ── P2.1: reconcile_paths ──────────────────────────────────────

    fn npath(root: &Path, rel: &str) -> NormalizedPath {
        // 走绝对路径 + NormalizedPath::new（会 normalize separator / 大小写），
        // 与 scan_metadata_recursive 产出的 index key 一致。
        NormalizedPath::new(&root.join(rel), root)
    }

    #[test]
    fn reconcile_paths_detects_modify() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::write(dir.path().join("a.txt"), b"CHANGED").unwrap();
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "a.txt")], &blob)
            .unwrap();
        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.modified[0].path.as_str(), "a.txt");
        // 只有 a.txt 被 hash：不改的 b.txt / sub/c.txt 不动。
        assert!(delta.created.is_empty() && delta.deleted.is_empty());
    }

    #[test]
    fn reconcile_paths_detects_create() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::write(dir.path().join("new.txt"), b"new").unwrap();
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "new.txt")], &blob)
            .unwrap();
        assert_eq!(delta.created.len(), 1);
        assert_eq!(delta.created[0].0.as_str(), "new.txt");
    }

    #[test]
    fn reconcile_paths_detects_delete_with_preimage() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "a.txt")], &blob)
            .unwrap();
        assert_eq!(delta.deleted.len(), 1);
        assert_eq!(delta.deleted[0].path.as_str(), "a.txt");
        // preimage 取回原始 "hello"。
        let restored = blob
            .get(&delta.deleted[0].before.blob_id.clone().unwrap())
            .unwrap();
        assert_eq!(restored, b"hello");
    }

    #[test]
    fn reconcile_paths_directory_subtree_delete_keeps_preimages() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // rm -rf sub/。watcher 可能只报 sub/ 一个 path，也可能报子文件。
        std::fs::remove_dir_all(dir.path().join("sub")).unwrap();
        // 只传 dirty 目录自身（树删除的常见 watcher 通知）。
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "sub")], &blob)
            .unwrap();
        // 目录自身不产出 Delete；其内文件在 index 中仍是 stale（本次调用不递归）。
        // reconcile_paths 是逐 path 的补齐信号，不隐式递归——由调用方决定
        // 是否也用全量 fallback。这里只验证：目录 path 不产生无 preimage 的 Delete。
        assert!(
            delta
                .deleted
                .iter()
                .all(|d| d.before.kind == EntryKind::File),
            "目录删除不应产生无 preimage 的 Delete"
        );
    }

    #[test]
    fn reconcile_paths_ignores_nonexistent_and_duplicates() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 不存在的 path + 重复的 dirty path。
        let delta = index
            .reconcile_paths(
                &[
                    npath(dir.path(), "no-such.txt"),
                    npath(dir.path(), "a.txt"),
                    npath(dir.path(), "a.txt"), // duplicate
                ],
                &blob,
            )
            .unwrap();
        assert!(delta.created.is_empty() && delta.deleted.is_empty());
        assert!(
            delta.modified.is_empty(),
            "未变化的 a.txt 因重复也不重 hash"
        );
    }

    #[test]
    fn reconcile_paths_unchanged_skips_hash() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();
        let before = blob.count().unwrap();

        // 传一个未变化的 path → 不产 change、不新增 blob。
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "a.txt")], &blob)
            .unwrap();
        assert!(delta.modified.is_empty());
        assert_eq!(blob.count().unwrap(), before, "未变化不重 hash");
    }

    #[test]
    fn reconcile_paths_rename_as_delete_plus_create() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::rename(dir.path().join("a.txt"), dir.path().join("moved.txt")).unwrap();
        // watcher 通常发出 old 删除 + new 创建两个 dirty event。
        let delta = index
            .reconcile_paths(
                &[npath(dir.path(), "a.txt"), npath(dir.path(), "moved.txt")],
                &blob,
            )
            .unwrap();
        assert_eq!(delta.deleted.len(), 1, "a.txt 删除（带 preimage）");
        assert_eq!(delta.created.len(), 1, "moved.txt 新建");
        assert_eq!(delta.created[0].0.as_str(), "moved.txt");
    }

    // ── P2.4: adversarial ─────────────────────────────────────────

    /// 批量变更：多个文件同时改/增/删（git checkout / git reset 式）。
    #[test]
    fn reconcile_paths_bulk_changes() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 同一次“批量 revert”：改 a.txt，删 b.txt，新建 c2.txt。
        std::fs::write(dir.path().join("a.txt"), b"A-CHANGED").unwrap();
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();
        std::fs::write(dir.path().join("c2.txt"), b"nested-content").unwrap();

        let delta = index
            .reconcile_paths(
                &[
                    npath(dir.path(), "a.txt"),
                    npath(dir.path(), "b.txt"),
                    npath(dir.path(), "c2.txt"),
                ],
                &blob,
            )
            .unwrap();

        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.modified[0].path.as_str(), "a.txt");
        assert_eq!(delta.deleted.len(), 1);
        assert_eq!(delta.deleted[0].path.as_str(), "b.txt");
        assert!(
            delta.deleted[0].before.blob_id.is_some(),
            "b 删除带 preimage"
        );
        assert_eq!(delta.created.len(), 1);
        assert_eq!(delta.created[0].0.as_str(), "c2.txt");
    }

    /// atomic-save：编辑器写临时文件再 rename 覆盖（.tmp -> a.txt）。
    /// 即使 temp 也进 dirty，reconcile 仍正确识别 a.txt 修改 + tmp 新建。
    #[test]
    fn reconcile_paths_atomic_save_rename() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // atomic save：写 .a.txt.tmp，再 rename 到 a.txt。
        std::fs::write(dir.path().join(".a.txt.tmp"), b"atomic new").unwrap();
        std::fs::rename(dir.path().join(".a.txt.tmp"), dir.path().join("a.txt")).unwrap();

        // watcher 分别提醒 tmp 与 a.txt。
        let delta = index
            .reconcile_paths(
                &[npath(dir.path(), ".a.txt.tmp"), npath(dir.path(), "a.txt")],
                &blob,
            )
            .unwrap();

        // a.txt 内容变了（Modify，before=原 content）；tmp 不再存在（无 entry→忽略）。
        assert_eq!(delta.modified.len(), 1);
        assert_eq!(delta.modified[0].path.as_str(), "a.txt");
        let restored = blob
            .get(&delta.modified[0].before.blob_id.clone().unwrap())
            .unwrap();
        assert_eq!(restored, b"hello", "before 仍是旧 hello（可 undo）");
    }

    /// rapid create/delete/create：目录不存在→创建→删除→再创建。
    /// dirty 去重且以最终文件系统为准。
    #[test]
    fn reconcile_paths_rapid_create_delete_create() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 快速：删 b → 建 b（不同内容）→ 再删。最终 b 不存在 → Deleted(before 原内容)。
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b-final").unwrap();
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();

        // 多次 mark 同一 path（去重），只影响最终状态判断。
        let delta = index
            .reconcile_paths(
                &[
                    npath(dir.path(), "b.txt"),
                    npath(dir.path(), "b.txt"),
                    npath(dir.path(), "b.txt"),
                ],
                &blob,
            )
            .unwrap();

        assert_eq!(delta.deleted.len(), 1, "最终 b 删除");
        // preimage 是**最初的** world（index 记录的第一个 b 内容）。
        let restored = blob
            .get(&delta.deleted[0].before.blob_id.clone().unwrap())
            .unwrap();
        assert_eq!(restored, b"world");
    }

    /// 目录内批量增删 + 目录被删：及时传入被删目录下所有文件时都带 preimage。
    #[test]
    fn reconcile_paths_deep_subtree_delete_all_files() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 造一层深层目录。
        std::fs::create_dir_all(dir.path().join("deep").join("nested")).unwrap();
        std::fs::write(dir.path().join("deep").join("nested").join("x.txt"), b"x").unwrap();
        index
            .reconcile_paths(&[npath(dir.path(), "deep/nested/x.txt")], &blob)
            .unwrap();

        // 删整个 deep/ 下的文件（watcher 可能逐个报）→ 每个都带 preimage。
        std::fs::remove_file(dir.path().join("deep").join("nested").join("x.txt")).unwrap();
        let delta = index
            .reconcile_paths(
                &[
                    npath(dir.path(), "deep/nested/x.txt"),
                    npath(dir.path(), "deep"),
                    npath(dir.path(), "deep/nested"),
                ],
                &blob,
            )
            .unwrap();
        assert_eq!(delta.deleted.len(), 1);
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            delta.deleted[0].path.as_str(),
            &format!("deep{sep}nested{sep}x.txt")
        );
        assert!(delta.deleted[0].before.blob_id.is_some());
    }

    /// P2.5 benchmark（#[ignore]，手动/CI 按需跑）：对比全量 `reconcile`
    /// 与增量 `reconcile_paths` 在 N=2000 文件、改 1 个文件时的成本。
    /// 不断言绝对时间（CI 脆弱），只断言：两类 reconcile 结果一致，且增量
    /// 显著更快（宽松阈值 ×10）。
    #[test]
    #[ignore]
    fn bench_reconcile_vs_reconcile_paths() {
        use std::time::Instant;

        let dir = tempdir().unwrap();
        let blob = BlobStore::new(dir.path().join("blob"));
        // 构造 N=2000 个小文件 workspace。
        for i in 0..2000 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), format!("content-{i}")).unwrap();
        }
        let mut idx = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 改 1 个文件。
        std::fs::write(dir.path().join("f42.txt"), b"changed-42").unwrap();

        let t0 = Instant::now();
        let full = WorkspaceIndex::reconcile(&mut idx, &[], &blob).unwrap();
        let full_elapsed = t0.elapsed();

        // 再改 1 个文件，用增量路径。
        std::fs::write(dir.path().join("f43.txt"), b"changed-43").unwrap();
        let t1 = Instant::now();
        let incr = idx
            .reconcile_paths(&[npath(dir.path(), "f43.txt")], &blob)
            .unwrap();
        let incr_elapsed = t1.elapsed();

        // 两个路径都只记录 1 个 modify（正确性一致）。
        assert_eq!(full.modified.len(), 1);
        assert_eq!(incr.modified.len(), 1);
        assert_eq!(incr.modified[0].path.as_str(), "f43.txt");

        eprintln!(
            "bench: full={full_elapsed:?} (2000 files, 1 changed); incremental={incr_elapsed:?} (1 dirty)"
        );
        // 宽松阈值：增量比全量至少快 10%（实际通常快 10-100x）。
        if full_elapsed.as_nanos() > 0 {
            assert!(
                incr_elapsed < full_elapsed,
                "增量 reconcile 应比全量快: full={full_elapsed:?} incr={incr_elapsed:?}"
            );
        }
    }

    // ── P2.6: directory dirty = subtree invalidation ──────────────

    /// 1) rm -rf tree，只提供 `tree/` 一个 dirty event：所有 tracked descendants
    ///    都必须产生 Delete + preimage，且 index 不再残留任何 stale entries。
    #[test]
    fn reconcile_paths_rm_rf_tree_single_dir_event_clears_index() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        // 构造一个多级子树。
        std::fs::create_dir_all(dir.path().join("tree").join("inner")).unwrap();
        std::fs::write(dir.path().join("tree").join("a.rs"), b"a").unwrap();
        std::fs::write(dir.path().join("tree").join("b.rs"), b"b").unwrap();
        std::fs::write(dir.path().join("tree").join("inner").join("c.rs"), b"c").unwrap();
        index
            .reconcile_paths(&[npath(dir.path(), "tree")], &blob)
            .unwrap();

        // rm -rf tree/，只报一个 dirty：`tree`。
        std::fs::remove_dir_all(dir.path().join("tree")).unwrap();
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "tree")], &blob)
            .unwrap();

        // 所有 descendants 都产生 Delete 且带 preimage。
        let mut deleted = delta
            .deleted
            .iter()
            .map(|d| d.path.as_str().to_string())
            .collect::<Vec<_>>();
        deleted.sort();
        assert_eq!(
            deleted,
            vec!["tree\\a.rs", "tree\\b.rs", "tree\\inner\\c.rs"],
            "整个 subtree 的文件都必须 Deleted (normalized path)"
        );
        for d in &delta.deleted {
            assert!(d.before.blob_id.is_some(), "{} preimage", d.path);
        }
        // index 不再残留任何 tree/ descendants。
        let leftover = index
            .paths()
            .map(|p| p.as_str().to_string())
            .filter(|p| p.starts_with("tree"))
            .collect::<Vec<_>>();
        assert!(leftover.is_empty(), "index 不应残留 tree /*: {leftover:?}");
    }

    /// 2) 目录 rename，只提供 old/new 两个顶层 path：old subtree 无 stale entries，
    ///    new subtree 完整进入 index。
    #[test]
    fn reconcile_paths_directory_rename_old_removed_new_added() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::create_dir_all(dir.path().join("old")).unwrap();
        std::fs::write(dir.path().join("old").join("a.rs"), b"aa").unwrap();
        std::fs::write(dir.path().join("old").join("b.rs"), b"bb").unwrap();
        index
            .reconcile_paths(&[npath(dir.path(), "old")], &blob)
            .unwrap();

        // mv old new
        std::fs::rename(dir.path().join("old"), dir.path().join("new")).unwrap();
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "old"), npath(dir.path(), "new")], &blob)
            .unwrap();

        // old 的两个文件 Deleted（preimage），new 的两个 Created。
        let deleted = delta
            .deleted
            .iter()
            .map(|d| d.path.as_str().to_string())
            .collect::<Vec<_>>();
        let created = delta
            .created
            .iter()
            .map(|(p, _)| p.as_str().to_string())
            .collect::<Vec<_>>();
        let sep = std::path::MAIN_SEPARATOR;
        assert!(
            deleted.contains(&format!("old{sep}a.rs"))
                && deleted.contains(&format!("old{sep}b.rs"))
        );
        assert!(
            created.contains(&format!("new{sep}a.rs"))
                && created.contains(&format!("new{sep}b.rs"))
        );

        // index 无 old/ descendants 残留，且 new/ 完整收录。
        let leftover_old = index
            .paths()
            .map(|p| p.as_str().to_string())
            .filter(|p| p.starts_with("old"))
            .collect::<Vec<_>>();
        assert!(leftover_old.is_empty(), "old 不应残留: {leftover_old:?}");
        for f in ["new/a.rs", "new/b.rs"] {
            let e = index.get_entry(&npath(dir.path(), f)).expect(f);
            assert_eq!(e.kind, super::EntryKind::File);
        }
    }

    /// 3) 新建整个 directory tree，只提供 parent dirty：所有 files 进入 index。
    #[test]
    fn reconcile_paths_create_tree_single_parent_event_indexes_all() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();

        std::fs::create_dir_all(dir.path().join("gen").join("sub")).unwrap();
        std::fs::write(dir.path().join("gen").join("x.rs"), b"x").unwrap();
        std::fs::write(dir.path().join("gen").join("sub").join("y.rs"), b"y").unwrap();

        // 只报 parent `gen`。
        let delta = index
            .reconcile_paths(&[npath(dir.path(), "gen")], &blob)
            .unwrap();

        let created = delta
            .created
            .iter()
            .map(|(p, _)| p.as_str().to_string())
            .collect::<Vec<_>>();
        let sep = std::path::MAIN_SEPARATOR;
        assert!(created.contains(&format!("gen{sep}x.rs")), "x.rs created");
        assert!(
            created.contains(&format!("gen{sep}sub{sep}y.rs")),
            "y.rs created"
        );
        for f in ["gen/x.rs", "gen/sub/y.rs"] {
            assert!(index.get_entry(&npath(dir.path(), f)).is_some());
        }
    }

    /// 4) dirty 同时含 `src/`、`src/a.rs`、`src/sub/b.rs`：prefix collapse，
    ///    只 hash 一次子树，不重复。
    #[test]
    fn reconcile_paths_prefix_collapse_no_double_scan() {
        let dir = make_root();
        let blob = BlobStore::new(dir.path().join("blob"));
        let mut index = WorkspaceIndex::initial_scan(dir.path(), &[], 100_000, &blob).unwrap();
        std::fs::create_dir_all(dir.path().join("src").join("sub")).unwrap();
        std::fs::write(dir.path().join("src").join("a.rs"), b"A1").unwrap();
        std::fs::write(dir.path().join("src").join("sub").join("b.rs"), b"B1").unwrap();
        index
            .reconcile_paths(&[npath(dir.path(), "src")], &blob)
            .unwrap();
        let before = blob.count().unwrap();

        // 改 a.rs，同时 dirty 报 src/ + src/a.rs + src/sub/b.rs（冗余）。
        std::fs::write(dir.path().join("src").join("a.rs"), b"A2").unwrap();
        let delta = index
            .reconcile_paths(
                &[
                    npath(dir.path(), "src"),
                    npath(dir.path(), "src/a.rs"),
                    npath(dir.path(), "src/sub/b.rs"),
                ],
                &blob,
            )
            .unwrap();

        // prefix collapse 后只新增 1 个 blob（a.rs 的 after）；b.rs 未变不重 hash。
        assert_eq!(delta.modified.len(), 1);
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(delta.modified[0].path.as_str(), &format!("src{sep}a.rs"));
        assert_eq!(
            blob.count().unwrap(),
            before + 1,
            "只 hash 一个 changed file"
        );
    }

    /// 5) INVARIANT：同一 final FS 上，incremental reconcile_paths(dirty scopes)
    ///    与 full reconcile() 在最终 index 语义上等价。
    fn assert_index_equivalent(a: &WorkspaceIndex, b: &WorkspaceIndex) {
        let pa = index_snapshot(a);
        let pb = index_snapshot(b);
        assert_eq!(pa.len(), pb.len(), "index 条目数应一致");
        for (p, (k, b)) in pa {
            let (kb, bb) = pb.get(&p).unwrap_or_else(|| panic!("missing {p}"));
            assert_eq!(k, *kb, "kind {p}");
            // BlobId 无 PartialEq，比 as_str。
            assert_eq!(
                b.as_ref().map(|x| x.as_str()),
                bb.as_ref().map(|x| x.as_str()),
                "blob {p}"
            );
        }
    }

    fn index_snapshot(
        idx: &WorkspaceIndex,
    ) -> std::collections::BTreeMap<String, (super::EntryKind, Option<BlobId>)> {
        let mut m = std::collections::BTreeMap::new();
        for p in idx.paths() {
            let e = idx.get_entry(p).unwrap();
            m.insert(p.as_str().to_string(), (e.kind.clone(), e.blob_id.clone()));
        }
        m
    }

    #[test]
    fn reconcile_incremental_equals_full_invariant() {
        // blob store 放在 workspace 之外（否则会被扫进 index，污染 invariant）。
        let dir = tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), b"hello").unwrap();
        std::fs::write(ws.join("b.txt"), b"world").unwrap();
        std::fs::create_dir(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/c.txt"), b"nested").unwrap();

        let blob_full = BlobStore::new(dir.path().join("blob-full"));
        let blob_incr = BlobStore::new(dir.path().join("blob-incr"));
        let mut full = WorkspaceIndex::initial_scan(&ws, &[], 100_000, &blob_full).unwrap();
        let mut incr = WorkspaceIndex::initial_scan(&ws, &[], 100_000, &blob_incr).unwrap();

        // 同一个未来 FS：改、删、增混合。
        std::fs::create_dir_all(ws.join("mix")).unwrap();
        std::fs::write(ws.join("a.txt"), b"A-CHANGED").unwrap();
        std::fs::write(ws.join("mix/new.rs"), b"n").unwrap();
        std::fs::remove_file(ws.join("b.txt")).unwrap();

        // full reconcile 一个 index。
        let _ = full.reconcile(&[], &blob_full).unwrap();
        // incremental reconcile 另一个。
        let _ = incr
            .reconcile_paths(
                &[npath(&ws, "a.txt"), npath(&ws, "b.txt"), npath(&ws, "mix")],
                &blob_incr,
            )
            .unwrap();

        assert_index_equivalent(&full, &incr);
    }
}
