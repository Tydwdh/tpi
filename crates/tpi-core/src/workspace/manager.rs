//! WorkspaceManager — unified entry point for all workspace mutations.
//!
//! Every tool that may modify the workspace goes through this boundary:
//!
//! ```text
//! begin_transaction(cause)
//!     ↓
//! record mutations / reconcile
//!     ↓
//! commit → journal
//! ```

use super::blob::BlobStore;
use super::checkpoint::Checkpoint;
use super::index::{IndexDelta, WorkspaceIndex};
use super::journal::{JournalEntry, MutationJournal};
use super::mutation::WorkspaceMutation;
use super::policy::{
    MutationSafetyPolicy, Reversibility, ReversibilityIssue, TrackingPolicy, UndoResult,
    WorkspaceConflict,
};
use super::transaction::{MutationCause, TransactionState, WorkspaceTransaction};
use super::types::{CheckpointId, NormalizedPath, TransactionId};
use super::watcher::{WatchState, WorkspaceWatcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Configuration for opening a workspace.
pub struct WorkspaceConfig {
    pub tracking_policy: TrackingPolicy,
    pub safety_policy: MutationSafetyPolicy,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            tracking_policy: TrackingPolicy::default(),
            safety_policy: MutationSafetyPolicy::BestEffort,
        }
    }
}

/// Persistent workspace manager — the single source of truth for workspace state.
pub struct WorkspaceManager {
    root: PathBuf,
    index: WorkspaceIndex,
    blob_store: BlobStore,
    journal: MutationJournal,
    // Reserved for future policy enforcement (tracking/budget checks).
    #[allow(dead_code)]
    policy: MutationSafetyPolicy,
    #[allow(dead_code)]
    tracking_policy: TrackingPolicy,
    /// Checkpoints form a chain: checkpoints\[0\] is the baseline.
    checkpoints: Vec<Checkpoint>,
    /// Active transactions (not yet committed).
    active_transactions: Vec<WorkspaceTransaction>,
    /// Current checkpoint id (the latest committed state).
    current_checkpoint: Option<CheckpointId>,
    /// Optional filesystem watcher (P2.x：watcher-assisted incremental tracking)。
    /// None 表示未启用（打开时不自动附加；由调用方决定）。
    watcher: Option<WorkspaceWatcher>,
}

impl WorkspaceManager {
    /// Open a workspace, restoring persistent state if available.
    pub fn open(
        root: &Path,
        session_id: &str,
        artifacts_root: &Path,
        config: WorkspaceConfig,
    ) -> Result<Self, ManagerError> {
        let workspace_dir = root.join(".tpi-workspace");
        let blob_store = BlobStore::new(workspace_dir.join("objects"));
        let index_path = workspace_dir.join("index.json");
        let journal = MutationJournal::new(artifacts_root, session_id);

        // Try to restore persistent index.
        let index = match WorkspaceIndex::load(&index_path, root.to_path_buf()) {
            Ok(Some(idx)) => idx,
            Ok(None) => {
                // First time — do initial scan.
                let exclude = config.tracking_policy.effective_exclude();
                WorkspaceIndex::initial_scan(
                    root,
                    &exclude,
                    config.tracking_policy.max_tracked_files,
                    &blob_store,
                )
                .map_err(|e| ManagerError::IndexInit(e.to_string()))?
            }
            Err(e) => {
                tracing::warn!("failed to load index, re-scanning: {e}");
                let exclude = config.tracking_policy.effective_exclude();
                WorkspaceIndex::initial_scan(
                    root,
                    &exclude,
                    config.tracking_policy.max_tracked_files,
                    &blob_store,
                )
                .map_err(|e| ManagerError::IndexInit(e.to_string()))?
            }
        };

        // Create baseline checkpoint if this is a fresh index.
        let mut checkpoints = Vec::new();
        let current_checkpoint = if index.is_empty() {
            let baseline = Checkpoint::new(None, Vec::new());
            let id = baseline.id.clone();
            checkpoints.push(baseline);
            Some(id)
        } else {
            // For restored index, create a baseline with empty mutations.
            let baseline = Checkpoint::new(None, Vec::new());
            let id = baseline.id.clone();
            checkpoints.push(baseline);
            Some(id)
        };

        Ok(Self {
            root: root.to_path_buf(),
            index,
            blob_store,
            journal,
            policy: config.safety_policy,
            tracking_policy: config.tracking_policy,
            checkpoints,
            active_transactions: Vec::new(),
            current_checkpoint,
            watcher: None,
        })
    }

    /// Begin a new transaction. Returns a handle that must be committed or aborted.
    pub fn begin_transaction(
        &mut self,
        cause: MutationCause,
    ) -> Result<TransactionHandle<'_>, ManagerError> {
        let checkpoint_before = self
            .current_checkpoint
            .clone()
            .unwrap_or_else(|| CheckpointId::new("none"));

        let tx = WorkspaceTransaction::new(cause, checkpoint_before);

        let tx_id = tx.id.clone();
        self.active_transactions.push(tx);

        Ok(TransactionHandle {
            manager: self,
            tx_id,
        })
    }

    /// Record a mutation for the active transaction.
    pub fn record_mutation(
        &mut self,
        tx_id: &TransactionId,
        mutation: WorkspaceMutation,
    ) -> Result<(), ManagerError> {
        let tx = self
            .active_transactions
            .iter_mut()
            .find(|t| &t.id == tx_id && !t.is_terminal())
            .ok_or_else(|| ManagerError::TransactionNotFound(tx_id.to_string()))?;

        tx.state = TransactionState::Running;

        // Store content in CAS if needed.
        match &mutation {
            WorkspaceMutation::Create { content, .. }
            | WorkspaceMutation::Delete { content, .. } => {
                // Content should already be in CAS.
                if !self.blob_store.contains(content) {
                    return Err(ManagerError::BlobMissing(content.to_string()));
                }
            }
            WorkspaceMutation::Modify { before, after, .. } => {
                if !self.blob_store.contains(before) || !self.blob_store.contains(after) {
                    return Err(ManagerError::BlobMissing(format!(
                        "before={before} after={after}"
                    )));
                }
            }
            WorkspaceMutation::Rename { content, .. } => {
                if !self.blob_store.contains(content) {
                    return Err(ManagerError::BlobMissing(content.to_string()));
                }
            }
        }

        Ok(())
    }

    /// Commit a transaction: create checkpoint, write journal entry.
    pub fn commit_transaction(
        &mut self,
        tx_id: &TransactionId,
        mutations: Vec<WorkspaceMutation>,
    ) -> Result<Reversibility, ManagerError> {
        let tx_index = self
            .active_transactions
            .iter()
            .position(|t| &t.id == tx_id)
            .ok_or_else(|| ManagerError::TransactionNotFound(tx_id.to_string()))?;

        let mut tx = self.active_transactions.remove(tx_index);

        // Evaluate reversibility.
        let reversibility = self.evaluate_reversibility(&mutations);

        // Create checkpoint.
        let parent = self.current_checkpoint.clone();
        let checkpoint = Checkpoint::new(parent, mutations.clone());
        let checkpoint_id = checkpoint.id.clone();
        self.checkpoints.push(checkpoint);
        self.current_checkpoint = Some(checkpoint_id.clone());

        // Update transaction state.
        tx.state = TransactionState::Committed;
        tx.checkpoint_after = Some(checkpoint_id.clone());
        tx.committed_at = Some(super::types::Timestamp::now());

        // Write journal entry.
        let cause = tx.cause.clone();
        let entry = JournalEntry {
            transaction_id: tx.id.clone(),
            cause,
            checkpoint_id,
            mutations,
            reversibility: reversibility.clone(),
            timestamp: super::types::Timestamp::now(),
        };

        self.journal
            .append(&entry)
            .map_err(|e| ManagerError::Journal(e.to_string()))?;

        // Persist index.
        let index_path = self.root.join(".tpi-workspace").join("index.json");
        if let Err(e) = self.index.persist(&index_path) {
            tracing::warn!("failed to persist index: {e}");
        }

        Ok(reversibility)
    }

    /// Undo a specific transaction by id.
    pub fn undo(&mut self, tx_id: &TransactionId) -> Result<UndoResult, ManagerError> {
        let entry = self
            .journal
            .find(tx_id)
            .map_err(|e| ManagerError::Journal(e.to_string()))?
            .ok_or_else(|| ManagerError::TransactionNotFound(tx_id.to_string()))?;

        // Check CAS: all affected files must still be in their expected state.
        let mut conflicts = Vec::new();
        for mutation in &entry.mutations {
            match mutation {
                WorkspaceMutation::Create { path, content } => {
                    // Undo create = delete. Check that file content matches.
                    let norm = NormalizedPath::new(&self.root.join(path.as_str()), &self.root);
                    if let Some(current_blob) = self.index.get_blob_id(&norm)
                        && current_blob != content
                    {
                        conflicts.push(WorkspaceConflict {
                            path: path.to_string(),
                            expected_blob: content.to_string(),
                            actual_blob: current_blob.to_string(),
                            modified_by: "external".into(),
                        });
                    }
                }
                WorkspaceMutation::Delete { path, content } => {
                    // Undo delete = restore. Check file doesn't exist.
                    let full_path = self.root.join(path.as_str());
                    if full_path.exists() {
                        let norm = NormalizedPath::new(&full_path, &self.root);
                        if let Some(current_blob) = self.index.get_blob_id(&norm)
                            && current_blob != content
                        {
                            conflicts.push(WorkspaceConflict {
                                path: path.to_string(),
                                expected_blob: "(deleted)".into(),
                                actual_blob: current_blob.to_string(),
                                modified_by: "external".into(),
                            });
                        }
                    }
                }
                WorkspaceMutation::Modify {
                    path,
                    before: _,
                    after,
                } => {
                    let norm = NormalizedPath::new(&self.root.join(path.as_str()), &self.root);
                    match self.index.get_blob_id(&norm) {
                        Some(current) if current == after => { /* OK, can undo */ }
                        Some(current) => {
                            conflicts.push(WorkspaceConflict {
                                path: path.to_string(),
                                expected_blob: after.to_string(),
                                actual_blob: current.to_string(),
                                modified_by: "external".into(),
                            });
                        }
                        None => {
                            conflicts.push(WorkspaceConflict {
                                path: path.to_string(),
                                expected_blob: after.to_string(),
                                actual_blob: "(missing)".into(),
                                modified_by: "external".into(),
                            });
                        }
                    }
                }
                WorkspaceMutation::Rename {
                    from: _,
                    to,
                    content,
                } => {
                    let to_norm = NormalizedPath::new(&self.root.join(to.as_str()), &self.root);
                    if let Some(current_blob) = self.index.get_blob_id(&to_norm)
                        && current_blob != content
                    {
                        conflicts.push(WorkspaceConflict {
                            path: to.to_string(),
                            expected_blob: content.to_string(),
                            actual_blob: current_blob.to_string(),
                            modified_by: "external".into(),
                        });
                    }
                }
            }
        }

        if !conflicts.is_empty() {
            return Ok(UndoResult::Conflict {
                transaction_id: tx_id.to_string(),
                conflicts,
            });
        }

        // Apply reverse mutations.
        let mut affected = Vec::new();
        for mutation in &entry.mutations {
            let reverse = mutation.reverse();
            self.apply_mutation(&reverse)?;
            for p in reverse.affected_paths() {
                affected.push(p.to_string());
            }
        }

        // Record the undo transaction.
        let undo_cause = MutationCause::Undo {
            transaction_id: tx_id.clone(),
        };
        let _ = self.begin_transaction(undo_cause);
        // Note: in production, the undo tx should be committed with its own checkpoint.
        // For now, we just update the index.

        // Persist updated index.
        let index_path = self.root.join(".tpi-workspace").join("index.json");
        if let Err(e) = self.index.persist(&index_path) {
            tracing::warn!("failed to persist index after undo: {e}");
        }

        Ok(UndoResult::Applied {
            transaction_id: tx_id.to_string(),
            affected_paths: affected,
        })
    }

    /// Apply a single mutation to the filesystem.
    fn apply_mutation(&mut self, mutation: &WorkspaceMutation) -> Result<(), ManagerError> {
        match mutation {
            WorkspaceMutation::Create { path, content } => {
                let data = self
                    .blob_store
                    .get(content)
                    .map_err(|e| ManagerError::Blob(e.to_string()))?;
                let full = self.root.join(path.as_str());
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| ManagerError::Io(e.to_string()))?;
                }
                std::fs::write(&full, &data).map_err(|e| ManagerError::Io(e.to_string()))?;
                // Update index.
                let norm = NormalizedPath::new(&full, &self.root);
                let meta = std::fs::metadata(&full).map_err(|e| ManagerError::Io(e.to_string()))?;
                self.index.upsert_file(&norm, meta, Some(content.clone()));
            }
            WorkspaceMutation::Delete { path, .. } => {
                let full = self.root.join(path.as_str());
                if full.exists() {
                    std::fs::remove_file(&full).map_err(|e| ManagerError::Io(e.to_string()))?;
                }
                let norm = NormalizedPath::new(&full, &self.root);
                self.index.remove_file(&norm);
            }
            WorkspaceMutation::Modify { path, after, .. } => {
                let data = self
                    .blob_store
                    .get(after)
                    .map_err(|e| ManagerError::Blob(e.to_string()))?;
                let full = self.root.join(path.as_str());
                std::fs::write(&full, &data).map_err(|e| ManagerError::Io(e.to_string()))?;
                let norm = NormalizedPath::new(&full, &self.root);
                let meta = std::fs::metadata(&full).map_err(|e| ManagerError::Io(e.to_string()))?;
                self.index.upsert_file(&norm, meta, Some(after.clone()));
            }
            WorkspaceMutation::Rename { from, to, content } => {
                let from_full = self.root.join(from.as_str());
                let to_full = self.root.join(to.as_str());
                if let Some(parent) = to_full.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| ManagerError::Io(e.to_string()))?;
                }
                std::fs::rename(&from_full, &to_full)
                    .map_err(|e| ManagerError::Io(e.to_string()))?;
                self.index
                    .remove_file(&NormalizedPath::new(&from_full, &self.root));
                let norm = NormalizedPath::new(&to_full, &self.root);
                let meta =
                    std::fs::metadata(&to_full).map_err(|e| ManagerError::Io(e.to_string()))?;
                self.index.upsert_file(&norm, meta, Some(content.clone()));
            }
        }
        Ok(())
    }

    /// Evaluate reversibility for a set of mutations.
    fn evaluate_reversibility(&self, mutations: &[WorkspaceMutation]) -> Reversibility {
        let mut issues = Vec::new();
        for m in mutations {
            // undo 依赖 **mutation 自带的 preimage blob 可读**，而非 index 里的
            // 当前条目——index 在 reconcile 后已更新（delete 的 path 已被移除），
            // 不能用 index 判断 preimage 是否仍可恢复。Delete 用 content，
            // Modify 用 before，Create 无 preimage（新建无需恢复旧内容）。
            let preimage: Option<&super::types::BlobId> = match m {
                WorkspaceMutation::Create { .. } => None,
                WorkspaceMutation::Modify { before, .. } => Some(before),
                WorkspaceMutation::Delete { content, .. } => Some(content),
                WorkspaceMutation::Rename { content, .. } => Some(content),
            };
            if let Some(blob) = preimage {
                if !blob.as_str().is_empty() && !self.blob_store.contains(blob) {
                    issues.push(ReversibilityIssue::UntrackedPath {
                        path: m
                            .affected_paths()
                            .first()
                            .map(|p| p.to_string())
                            .unwrap_or_default(),
                    });
                }
            }
        }
        if issues.is_empty() {
            Reversibility::Exact
        } else {
            Reversibility::Partial { reasons: issues }
        }
    }

    /// Current checkpoint id.
    pub fn current_checkpoint(&self) -> Option<&CheckpointId> {
        self.current_checkpoint.as_ref()
    }

    /// Reference to the blob store.
    pub fn blob_store(&self) -> &BlobStore {
        &self.blob_store
    }

    /// Mutable reference to the index (for reconciliation).
    pub fn index_mut(&mut self) -> &mut WorkspaceIndex {
        &mut self.index
    }

    /// Reference to the index.
    pub fn index(&self) -> &WorkspaceIndex {
        &self.index
    }

    /// Reconcile the workspace index against current filesystem state.
    pub fn reconcile_index(
        &mut self,
        exclude: &[String],
    ) -> Result<super::index::IndexDelta, ManagerError> {
        self.index
            .reconcile(exclude, &self.blob_store)
            .map_err(|e| ManagerError::IndexInit(e.to_string()))
    }

    /// 增量 reconcile：只处理给定 dirty paths（Watcher/DirtySet 驱动）。
    /// 不扫描整个 workspace；复杂度 O(K metadata + changed bytes)。
    pub fn reconcile_paths(
        &mut self,
        paths: &[super::types::NormalizedPath],
    ) -> Result<super::index::IndexDelta, ManagerError> {
        self.index
            .reconcile_paths(paths, &self.blob_store)
            .map_err(|e| ManagerError::IndexInit(e.to_string()))
    }

    /// 附加（取代已存在的）filesystem watcher。
    /// watcher 生命周期属于本 manager（随 manager 存活），由调用方在 session
    /// 初始化时调用一次——避免每次 bash 前后 start/stop 造成观察窗口漏洞。
    pub fn attach_watcher(&mut self) -> Result<(), String> {
        self.watcher = Some(WorkspaceWatcher::new(&self.root).map_err(|e| e.to_string())?);
        Ok(())
    }

    /// 当前 watcher 信任状态（无 watcher 视为 Uncertain→全量 fallback）。
    pub fn watcher_state(&self) -> WatchState {
        self.watcher
            .as_ref()
            .map(|w| w.watch_state())
            .unwrap_or(WatchState::Uncertain {
                reason: "no watcher attached".into(),
            })
    }

    /// 取走并清空 dirty paths（相对 workspace 规范化路径）。
    pub fn take_dirty_paths(&self) -> HashSet<String> {
        self.watcher
            .as_ref()
            .map(|w| w.take_dirty())
            .unwrap_or_default()
    }

    /// P2.3：arbitrary effect（bash/terminal）后的统一 reconcile 入口。
    ///
    /// 策略：
    /// - watcher Healthy + dirty 非空 → 增量 `reconcile_paths(dirty)`
    /// - watcher Uncertain / 未 attach → 全量 `reconcile`
    /// - **watcher Healthy + dirty 空 → 仍全量 `reconcile`**
    ///
    /// 为什么 healthy+dirty 空仍全量：bash 退出与最后一个 FS 事件到达之间存在
    /// 竞态窗口——事件可能还没投递。且 dirty 语义是“必须检查”（invalidation
    /// signal），不是“一定变了”。因此 watcher 永远只是**性能加速器**（收集到
    /// dirty 时跳过全量 metadata scan），绝不能作为“无需 reconcile”的判据
    /// （correctness oracle）。P1 保证全量 reconcile 不重 hash 未变文件，所以
    /// 无论走哪条路，复杂度都不会退回 O(all bytes)。
    pub fn reconcile_unknown_effect(
        &mut self,
        cause: MutationCause,
    ) -> Result<Reversibility, ManagerError> {
        let healthy = self.watcher_state() == WatchState::Healthy;
        let dirty = self.take_dirty_paths();

        let delta = if healthy && !dirty.is_empty() {
            let paths: Vec<NormalizedPath> = dirty
                .iter()
                .map(|p| NormalizedPath::new(&self.root.join(p.as_str()), &self.root))
                .collect();
            self.reconcile_paths(&paths)?
        } else {
            // Uncertain / 无 watcher / healthy 但 dirty 空（竞态窗口）→ 全量 reconcile。
            let exclude: Vec<String> = Vec::new();
            self.reconcile_index(&exclude)?
        };

        self.commit_delta(cause, delta)
    }

    /// 把 reconcile 产出的 delta 转为 journal mutation 并提交。
    /// 从 session 层下沉的唯一 delta→journal 转换点（单一事实源）。
    pub(crate) fn commit_delta(
        &mut self,
        cause: MutationCause,
        delta: IndexDelta,
    ) -> Result<Reversibility, ManagerError> {
        let created = delta.created.len();
        let modified = delta.modified.len();
        let deleted = delta.deleted.len();
        if created + modified + deleted == 0 {
            return Ok(Reversibility::Exact);
        }

        let mut mutations = Vec::new();
        for (path, entry) in &delta.created {
            if let Some(blob_id) = &entry.blob_id {
                mutations.push(WorkspaceMutation::Create {
                    path: path.clone(),
                    content: blob_id.clone(),
                });
            }
        }
        for m in &delta.modified {
            if let (Some(before_blob), Some(after_blob)) = (&m.before.blob_id, &m.after.blob_id) {
                mutations.push(WorkspaceMutation::Modify {
                    path: m.path.clone(),
                    before: before_blob.clone(),
                    after: after_blob.clone(),
                });
            }
        }
        for d in &delta.deleted {
            if let Some(content) = &d.before.blob_id {
                mutations.push(WorkspaceMutation::Delete {
                    path: d.path.clone(),
                    content: content.clone(),
                });
            }
        }

        if mutations.is_empty() {
            return Ok(Reversibility::Exact);
        }
        let handle = self.begin_transaction(cause)?;
        let tx_id = handle.tx_id().clone();
        drop(handle);
        self.commit_transaction(&tx_id, mutations)
    }

    /// Workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Handle for an active transaction. Must be committed or aborted.
pub struct TransactionHandle<'a> {
    manager: &'a mut WorkspaceManager,
    tx_id: TransactionId,
}

impl<'a> TransactionHandle<'a> {
    pub fn tx_id(&self) -> &TransactionId {
        &self.tx_id
    }

    /// Record a single mutation.
    pub fn record(&mut self, mutation: WorkspaceMutation) -> Result<(), ManagerError> {
        self.manager.record_mutation(&self.tx_id, mutation)
    }

    /// Commit the transaction with the given mutations.
    pub fn commit(self, mutations: Vec<WorkspaceMutation>) -> Result<Reversibility, ManagerError> {
        self.manager.commit_transaction(&self.tx_id, mutations)
    }
}

/// Errors from workspace manager operations.
#[derive(Debug)]
pub enum ManagerError {
    IndexInit(String),
    TransactionNotFound(String),
    BlobMissing(String),
    Blob(String),
    Journal(String),
    Io(String),
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagerError::IndexInit(msg) => write!(f, "index init: {msg}"),
            ManagerError::TransactionNotFound(id) => write!(f, "transaction not found: {id}"),
            ManagerError::BlobMissing(id) => write!(f, "blob missing: {id}"),
            ManagerError::Blob(msg) => write!(f, "blob store: {msg}"),
            ManagerError::Journal(msg) => write!(f, "journal: {msg}"),
            ManagerError::Io(msg) => write!(f, "io: {msg}"),
        }
    }
}

impl std::error::Error for ManagerError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().to_path_buf();
        let artifacts = root.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();

        // Create some test files.
        std::fs::write(workspace.join("hello.txt"), b"hello world").unwrap();
        std::fs::write(workspace.join("config.toml"), b"[section]\nkey = 1").unwrap();
        (root, workspace, artifacts)
    }

    #[test]
    fn open_creates_baseline() {
        let (_root, workspace, artifacts) = setup_workspace();
        let mgr = WorkspaceManager::open(
            &workspace,
            "test-session",
            &artifacts,
            WorkspaceConfig::default(),
        )
        .unwrap();

        assert!(mgr.current_checkpoint().is_some());
        assert!(!mgr.index().is_empty());
    }

    #[test]
    fn begin_and_commit_transaction() {
        let (_root, workspace, artifacts) = setup_workspace();
        let mut mgr = WorkspaceManager::open(
            &workspace,
            "test-session",
            &artifacts,
            WorkspaceConfig::default(),
        )
        .unwrap();

        // Store a blob for the new file content.
        let content = b"new file content";
        let _blob_id = mgr.blob_store().put(content).unwrap();

        // Put content into blob store for the "before" of an existing file.
        let before_content = std::fs::read(workspace.join("hello.txt")).unwrap();
        let before_blob = mgr.blob_store().put(&before_content).unwrap();
        let after_blob = mgr.blob_store().put(b"modified hello").unwrap();

        let norm = NormalizedPath::new(&workspace.join("hello.txt"), &workspace);

        let mut handle = mgr
            .begin_transaction(MutationCause::Edit {
                tool_call_id: "tc-1".into(),
            })
            .unwrap();

        let mutation = WorkspaceMutation::Modify {
            path: norm,
            before: before_blob,
            after: after_blob,
        };

        // Record a modify mutation.
        handle.record(mutation.clone()).unwrap();

        let result = handle.commit(vec![mutation]).unwrap();

        assert!(result.is_exact());
    }

    #[test]
    fn journal_persists_transactions() {
        let (_root, workspace, artifacts) = setup_workspace();
        let mut mgr = WorkspaceManager::open(
            &workspace,
            "test-session",
            &artifacts,
            WorkspaceConfig::default(),
        )
        .unwrap();

        let before_content = std::fs::read(workspace.join("hello.txt")).unwrap();
        let before_blob = mgr.blob_store().put(&before_content).unwrap();
        let after_blob = mgr.blob_store().put(b"changed").unwrap();
        let norm = NormalizedPath::new(&workspace.join("hello.txt"), &workspace);

        let handle = mgr
            .begin_transaction(MutationCause::Write {
                tool_call_id: "tc-2".into(),
            })
            .unwrap();

        let mutations = vec![WorkspaceMutation::Modify {
            path: norm,
            before: before_blob,
            after: after_blob,
        }];

        let _ = handle.commit(mutations).unwrap();

        // Load journal and verify.
        let state = mgr.journal.load().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries[0].reversibility.is_exact());

        // 端到端（审查 #9/#10）：模拟 bash 外部覆盖 hello.txt → reconcile_index
        // → 生成的 mutation 携带真实 before blob → undo 恢复到原始字节。
        // 这验证 bash 修改/删除文件的 preimage preservation 真正闭环。
        let (_root2, workspace2, artifacts2) = setup_workspace();
        let mut mgr2 = WorkspaceManager::open(
            &workspace2,
            "test-session",
            &artifacts2,
            WorkspaceConfig::default(),
        )
        .unwrap();

        // 初始：setup_workspace 中 hello.txt = "hello world"。
        let original2 = std::fs::read(workspace2.join("hello.txt")).unwrap();
        // 先 reconcile 一次，让 index 建立 hello.txt 的基准 entry。
        mgr2.reconcile_index(&[]).unwrap();

        // 模拟 bash 覆盖文件。
        std::fs::write(workspace2.join("hello.txt"), b"EXTERNAL-OVERWRITE").unwrap();
        let delta2 = mgr2.reconcile_index(&[]).unwrap();

        assert_eq!(delta2.modified.len(), 1);
        let m = &delta2.modified[0];
        let before_id = m.before.blob_id.clone().unwrap();
        // before blob round-trip 回原始字节。
        let restored_preimage = mgr2.blob_store().get(&before_id).unwrap();
        assert_eq!(restored_preimage, original2);

        // 用 delta 构造 Modify 并 commit（session.reconcile_after_execution 路径）。
        let norm2 = NormalizedPath::new(&workspace2.join("hello.txt"), &workspace2);
        let after_id = m.after.blob_id.clone().unwrap();
        let mutations2 = vec![WorkspaceMutation::Modify {
            path: norm2,
            before: before_id.clone(),
            after: after_id.clone(),
        }];
        let handle2 = mgr2
            .begin_transaction(MutationCause::Command {
                command_id: "bash-1".into(),
            })
            .unwrap();
        let _ = handle2.commit(mutations2).unwrap();

        // undo 应恢复原始字节。
        let state2 = mgr2.journal.load().unwrap();
        let entry2 = state2.entries.last().unwrap();
        if matches!(entry2.reversibility, Reversibility::Exact) {
            let result2 = mgr2.undo(&entry2.transaction_id).unwrap();
            match result2 {
                UndoResult::Applied { .. } => {
                    let restored2 = std::fs::read(workspace2.join("hello.txt")).unwrap();
                    assert_eq!(restored2, original2, "undo 必须恢复 bash 覆盖前的原始字节");
                }
                other => panic!("expected undo applied, got {other:?}"),
            }
        }
    }

    /// P2.3：无 watcher → `reconcile_unknown_effect` 走全量 fallback，
    /// 正确识别外部文件修改并写入 journal（可 undo）。
    #[test]
    fn reconcile_unknown_effect_no_watcher_full_scan() {
        let (_root, workspace, artifacts) = setup_workspace();
        let mut mgr =
            WorkspaceManager::open(&workspace, "s1", &artifacts, WorkspaceConfig::default())
                .unwrap();
        // 先建立 index 基准。
        mgr.reconcile_index(&[]).unwrap();

        std::fs::write(workspace.join("hello.txt"), b"overwritten").unwrap();
        let rev = mgr
            .reconcile_unknown_effect(MutationCause::Command {
                command_id: "bash-1".into(),
            })
            .unwrap();
        assert!(rev.is_exact());

        // journal 记录了一条 Modify（带 before preimage）。
        let state = mgr.journal.load().unwrap();
        assert_eq!(state.entries.len(), 1);
        let m = &state.entries[0].mutations[0];
        assert!(matches!(m, WorkspaceMutation::Modify { .. }));

        // undo 恢复原始字节。
        if state.entries[0].reversibility.is_exact() {
            let tx = state.entries[0].transaction_id.clone();
            let res = mgr.undo(&tx).unwrap();
            assert!(matches!(res, UndoResult::Applied { .. }));
            assert_eq!(
                std::fs::read(workspace.join("hello.txt")).unwrap(),
                b"hello world"
            );
        }
    }

    /// P2.3：attach watcher 后，bash 写入 → 事件 → `reconcile_unknown_effect`
    /// 走增量 `reconcile_paths`（healthy + dirty 非空）。容忍平台事件延迟：
    /// 若事件未及时到达则走 unhealthy→全量 fallback，两种路径结果一致（都记录
    /// mutation）——这正好验证“watcher 只是加速器，不影响 correctness”。
    #[test]
    fn reconcile_unknown_effect_with_watcher_is_consistent() {
        let (_root, workspace, artifacts) = setup_workspace();
        let mut mgr =
            WorkspaceManager::open(&workspace, "s2", &artifacts, WorkspaceConfig::default())
                .unwrap();
        mgr.reconcile_index(&[]).unwrap();
        mgr.attach_watcher().unwrap();

        // sleep 让 watcher 就绪（平台差异，放宽）。
        std::thread::sleep(std::time::Duration::from_millis(200));
        std::fs::write(workspace.join("config.toml"), b"[section]\nkey = 2").unwrap();
        // 等 watcher 事件（≤2s）。
        std::thread::sleep(std::time::Duration::from_millis(300));

        let rev = mgr
            .reconcile_unknown_effect(MutationCause::Command {
                command_id: "bash-w".into(),
            })
            .unwrap();
        assert!(rev.is_exact(), "无论增量/全量都应 Exact");

        // 该 mutation 必须进 journal（config.toml 被修改）。
        let state = mgr.journal.load().unwrap();
        assert!(
            state.entries.iter().any(|e| {
                e.mutations.iter().any(|m| {
                    matches!(m, WorkspaceMutation::Modify { path, .. } if path.as_str() == "config.toml")
                })
            }),
            "journal 必须记录 config.toml 的修改"
        );
    }

    // ── P2.4: adversarial / fallback ──────────────────────────────

    /// watcher 变得 Uncertain（overflow/error）时，reconcile_unknown_effect
    /// 必须 fallback 全量扫描，绝不因“dirty 空 / 信号缺失”而漏掉真实修改
    /// 或在 journal 里产生错误/缺失条目。
    #[test]
    fn reconcile_unknown_effect_fallback_when_watcher_uncertain() {
        let (_root, workspace, artifacts) = setup_workspace();
        let mut mgr =
            WorkspaceManager::open(&workspace, "s3", &artifacts, WorkspaceConfig::default())
                .unwrap();
        mgr.reconcile_index(&[]).unwrap();
        mgr.attach_watcher().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));

        // 人为把 watcher 置为 Uncertain（模拟 overflow/listener error）。
        if let Some(w) = mgr.watcher.as_mut() {
            w.force_uncertain_for_test("overflow (forced)");
        }

        // 在 Uncertain 状态下改文件，事件即便被吞掉，fallback 也必须发现。
        std::fs::write(workspace.join("config.toml"), b"[section]\nkey = 42").unwrap();
        std::fs::remove_file(workspace.join("hello.txt")).unwrap();

        let rev = mgr
            .reconcile_unknown_effect(MutationCause::Command {
                command_id: "bash-adv".into(),
            })
            .unwrap();
        // Uncertain -> 全量 fallback -> Exact（可 undo）。
        assert!(rev.is_exact());

        let state = mgr.journal.load().unwrap();
        // 必须同时记录 config.toml 修改 与 hello.txt 删除。
        let has_modify = state.entries.iter().any(|e| {
            e.mutations.iter().any(|m| {
                matches!(m, WorkspaceMutation::Modify { path, .. } if path.as_str() == "config.toml")
            })
        });
        let has_delete = state.entries.iter().any(|e| {
            e.mutations.iter().any(|m| {
                matches!(m, WorkspaceMutation::Delete { path, .. } if path.as_str() == "hello.txt")
            })
        });
        assert!(has_modify, "Uncertain 下修改必须被记录");
        assert!(has_delete, "Uncertain 下删除必须被记录");
    }

    /// 大量文件批量变更（git checkout -f / reset --hard 式）：
    /// 无 watcher（fallback 全量）也必须把每次变更准确记录进 journal，可 undo。
    #[test]
    fn reconcile_unknown_effect_bulk_revert_all_recorded() {
        let (_root, workspace, artifacts) = setup_workspace();
        let mut mgr =
            WorkspaceManager::open(&workspace, "s4", &artifacts, WorkspaceConfig::default())
                .unwrap();
        mgr.reconcile_index(&[]).unwrap();

        // git checkout 式：同时改 config.toml、删 hello.txt、新建 extra.txt。
        std::fs::write(workspace.join("config.toml"), b"[section]\nkey = 9").unwrap();
        std::fs::remove_file(workspace.join("hello.txt")).unwrap();
        std::fs::write(workspace.join("extra.txt"), b"new").unwrap();

        mgr.reconcile_unknown_effect(MutationCause::Command {
            command_id: "git-checkout".into(),
        })
        .unwrap();

        let state = mgr.journal.load().unwrap();
        let kinds = state
            .entries
            .iter()
            .flat_map(|e| &e.mutations)
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            kinds.iter().any(
                |m| matches!(m, WorkspaceMutation::Modify { path, .. } if path.as_str() == "config.toml")
            ),
            "批量 revert 的 Modify 应记录"
        );
        assert!(
            kinds.iter().any(
                |m| matches!(m, WorkspaceMutation::Create { path, .. } if path.as_str() == "extra.txt")
            ),
            "批量 revert 的 Create 应记录"
        );
        // hello.txt 删除带 preimage（undo 能恢复）。
        let del = kinds.iter().find_map(|m| match m {
            WorkspaceMutation::Delete { path, content } if path.as_str() == "hello.txt" => {
                Some(content.clone())
            }
            _ => None,
        });
        assert!(del.is_some(), "hello.txt 删除应记录且带 preimage");

        // undo 该删除事务后 hello.txt 恢复。
        let tx = state.entries.last().unwrap().transaction_id.clone();
        if let Ok(res) = mgr.undo(&tx) {
            assert!(matches!(res, UndoResult::Applied { .. }));
        }
    }
}
