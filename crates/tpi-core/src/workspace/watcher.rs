//! Filesystem watcher for workspace change detection.
//!
//! Uses the `notify` crate to detect filesystem changes and mark paths dirty
//! for reconciliation. Watcher events are invalidation signals, NOT truth source.
//!
//! ```text
//! watch event → mark dirty → transaction reconcile → filesystem + index comparison
//! ```

use notify::{RecursiveMode, Watcher as NotifyWatcher};
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

/// Watcher 对文件系统的信任状态。
///
/// - `Healthy`：watcher 运行正常，事件未遗漏，`DirtySet` 可信。
/// - `Uncertain`：发生可能遗漏事件的情况（overflow / listener error /
///   `need_rescan`），此时必须做全量 reconcile 兜底，不能信任增量结果。
///
/// 原则（§workspace）：watcher 是**性能加速器**，不是 correctness oracle。
/// `Uncertain` 不是错误，而是“此前的增量信号不再可信 → 调用方 fallback”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchState {
    Healthy,
    Uncertain { reason: String },
}

/// 跨线程共享的 watcher 状态。
#[derive(Debug, Clone)]
pub struct SharedWatchState(Arc<Mutex<WatchState>>);

impl SharedWatchState {
    pub fn healthy() -> Self {
        Self(Arc::new(Mutex::new(WatchState::Healthy)))
    }

    /// 标记为 Uncertain（由 handler 线程/overflow 触发）。
    pub fn set_uncertain(&self, reason: impl Into<String>) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = WatchState::Uncertain {
                reason: reason.into(),
            };
        }
    }

    /// 读取当前状态。
    pub fn get(&self) -> WatchState {
        self.0
            .lock()
            .map(|g| g.clone())
            .unwrap_or(WatchState::Uncertain {
                reason: "lock poisoned".into(),
            })
    }

    pub fn is_healthy(&self) -> bool {
        self.get() == WatchState::Healthy
    }
}

/// 真正的 filesystem watcher：用 `notify` 监听 workspace 根目录（recursive），
/// 把变化事件写入共享 `DirtySet`，并在发生遗漏风险时把状态置为 `Uncertain`。
///
/// 生命周期属于 workspace session（由 `WorkspaceManager` 持有并随 session 存活），
/// 不能跟单个 bash 命令——否则每次 start/stop 会造成观察窗口漏洞。
pub struct WorkspaceWatcher {
    dirty: SharedDirtySet,
    state: SharedWatchState,
    /// 持有 RecommendedWatcher 使其保持活跃（drop 即停止监听）。
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
    root: PathBuf,
}

impl WorkspaceWatcher {
    /// 创建 watcher 并开始递归监听 `root`。排除/exclude 与 debounce 由
    /// handler 内建 well-known 集（`.git`/`target` 等）与调用方 reconcile 策略处理；
    /// 此处不做内部 debounce（事件本身足够快，批量合并交调用方）。
    pub fn new(root: &Path) -> Result<Self, String> {
        let dirty = new_shared_dirty_set();
        let state = SharedWatchState::healthy();
        let dirty_for_handler = dirty.clone();
        let state_for_handler = state.clone();
        let root_for_handler = root.to_path_buf();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let event = match res {
                Ok(e) => e,
                Err(err) => {
                    // Listener 错误 → 无法确定是否漏事件 → Uncertain。
                    state_for_handler.set_uncertain(format!("watcher error: {err}"));
                    return;
                }
            };
            // overflow / 需重扫 → 之前可能漏事件 → Uncertain。
            if event.need_rescan() {
                state_for_handler.set_uncertain("need_rescan (overflow)");
            }
            for path in &event.paths {
                if is_excluded(path, &[]) {
                    continue;
                }
                let Some(rel) = normalize_watch_path(path, &root_for_handler) else {
                    continue; // 非 workspace 内路径：忽略。
                };
                if let Ok(mut ds) = dirty_for_handler.lock() {
                    ds.mark_dirty(&rel);
                }
            }
        })
        .map_err(|e| format!("recommended_watcher: {e}"))?;

        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| format!("watch {}: {e}", root.display()))?;

        Ok(Self {
            dirty,
            state,
            watcher,
            root: root.to_path_buf(),
        })
    }

    /// 是否处于 Uncertain（需全量 fallback）。
    pub fn is_uncertain(&self) -> bool {
        !self.state.is_healthy()
    }

    /// 当前信任状态。
    pub fn watch_state(&self) -> WatchState {
        self.state.get()
    }

    /// 取并清空所有 dirty path（相对 workspace 的规范化路径）。
    pub fn take_dirty(&self) -> HashSet<String> {
        self.dirty
            .lock()
            .map(|mut d| d.take_dirty())
            .unwrap_or_default()
    }

    /// 若无 pending dirty，事件又健康，则没有变化需 reconcile。
    pub fn dirty_empty(&self) -> bool {
        self.dirty.lock().map(|d| d.is_empty()).unwrap_or(false)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
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

    #[test]
    fn watch_state_healthy_by_default() {
        let ws = SharedWatchState::healthy();
        assert_eq!(ws.get(), WatchState::Healthy);
        assert!(ws.is_healthy());
    }

    #[test]
    fn shared_watch_state_uncertain_marker() {
        let ws = SharedWatchState::healthy();
        ws.set_uncertain("overflow");
        assert!(!ws.is_healthy());
        assert!(
            matches!(ws.get(), WatchState::Uncertain { reason } if reason.contains("overflow"))
        );
    }

    /// 集成：WorkspaceWatcher 将真实 FS 事件写入 DirtySet（平台延迟容忍的重试）。
    #[test]
    fn watcher_marks_dirty_on_file_write() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        let watcher = WorkspaceWatcher::new(dir.path()).expect("watcher 应可创建");

        // 初始 Healthy。
        assert!(watcher.is_uncertain() == false);

        // 改文件 → 触发事件。
        std::fs::write(dir.path().join("hello.txt"), b"hi there").unwrap();

        // 平台事件延迟不定，重试等待（≤2s）。
        let mut seen = false;
        for _ in 0..40 {
            let dirty = watcher.take_dirty();
            if dirty.iter().any(|p| p == "hello.txt") {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(seen, "watcher 应在 2s 内把 hello.txt 标记 dirty");
    }

    #[test]
    fn watcher_healthy_without_events() {
        let dir = tempdir().unwrap();
        let watcher = WorkspaceWatcher::new(dir.path()).unwrap();
        assert_eq!(watcher.watch_state(), WatchState::Healthy);
        assert!(watcher.dirty_empty());
    }

    #[test]
    fn watcher_ignores_excluded_paths() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let watcher = WorkspaceWatcher::new(dir.path()).unwrap();

        // 写 .git 内文件 → 不应标记 dirty（is_excluded 拦截）。
        std::fs::write(dir.path().join(".git").join("HEAD"), b"ref").unwrap();
        let mut excluded_leaked = false;
        for _ in 0..20 {
            let dirty = watcher.take_dirty();
            if dirty.iter().any(|p| p.starts_with(".git")) {
                excluded_leaked = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!excluded_leaked, "excluded 路径不得进入 dirty");
    }
}
