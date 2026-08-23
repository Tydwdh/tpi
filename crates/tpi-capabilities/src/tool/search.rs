//! 有界目录扫描与索引基础设施。
//!
//! 曾提供 `search` / `glob` 内容检索工具；它们已被删除——内容/文件名检索
//! 交给 `bash` + `rg --files` / `find` / `ls`（rg 已足够成熟）。
//!
//! 本模块仍保留被其它功能复用的纯扫描部分：
//! - `scan_dir`：`read` 目录分支（有界、遵循 `.gitignore`、不跟随 symlink）；
//! - `index_files`：`@` 引用补全（workspace 文件索引）；
//! - `ScanSnapshot` / `scan_snapshots`：遗留的 cursor 分页 snapshot 类型
//!   （ToolContext 字段类型，保留以维持最小改动面）。
//!
//! 单次默认计算预算：20,000 files、256 MiB scanned bytes、10 秒 deadline；
//! 结果报告 `scanned_files`、`scanned_bytes`、`elapsed_ms` 和 `stop_reason`。

use std::time::Instant;

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::tool::ToolContext;

/// 单次默认计算预算（§8.4）。
pub const MAX_SCAN_FILES: u64 = 20_000;
pub const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
pub const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// 超过 2 MiB 的普通源码候选跳过（§8.4）。
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 模型可见项数预算。
pub const MAX_RESULTS: usize = 1_000;
const MAX_RESULT_PATH_CHARS: usize = 2_048;

/// 一次扫描的有界有序结果 snapshot（遗留：cursor 分页类型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSnapshot {
    /// 生成快照的工具名。
    #[serde(default)]
    pub tool: String,
    pub items: Vec<String>,
    pub scanned_files: u64,
    pub scanned_bytes: u64,
    pub elapsed_ms: u64,
    pub stop_reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Complete,
    ResultLimit,
    ScanLimit,
    Deadline,
    Cancelled,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::Complete => "complete",
            StopReason::ResultLimit => "result_limit",
            StopReason::ScanLimit => "scan_limit",
            StopReason::Deadline => "deadline",
            StopReason::Cancelled => "cancelled",
        }
    }
}

/// 目录扫描结果（`read` 目录分支复用）。
#[derive(Debug, Clone)]
pub struct DirScan {
    pub items: Vec<String>,
    pub scanned_files: u64,
    pub scanned_bytes: u64,
    pub elapsed_ms: u64,
    pub stop_reason: StopReason,
}

/// 有界目录扫描（§8.4：gitignore、不跟随 symlink、跳过 binary/超大文件、
/// 深度限制、结果上限）。`read` 目录分支调用（原 list 工具的职责并入 read）。
///
/// 调用方负责 `resolve_tool_path` 与目录判定；这里只做扫描与统计。
pub(crate) fn scan_dir(root: &Utf8PathBuf, depth: usize, ctx: &ToolContext) -> DirScan {
    let started = Instant::now();
    let mut items: Vec<String> = Vec::new();
    let mut scanned_files = 0u64;
    let mut scanned_bytes = 0u64;
    let mut stop_reason = StopReason::Complete;

    'scan: for entry in WalkBuilder::new(root, Some(depth)) {
        if ctx.cancel.is_cancelled() {
            stop_reason = StopReason::Cancelled;
            break;
        }
        if started.elapsed() > SCAN_DEADLINE {
            stop_reason = StopReason::Deadline;
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|kind| kind.is_symlink()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if entry.depth() == 0 {
            continue; // root 本身
        }
        if entry.depth() > depth {
            continue;
        }
        if meta.is_dir() {
            items.push(format!(
                "{}/",
                truncate_line(&relative(root, entry.path()), MAX_RESULT_PATH_CHARS)
            ));
        } else {
            scanned_files = scanned_files.saturating_add(1);
            scanned_bytes = scanned_bytes.saturating_add(meta.len());
            if scanned_files >= MAX_SCAN_FILES || scanned_bytes >= MAX_SCAN_BYTES {
                stop_reason = StopReason::ScanLimit;
                break 'scan;
            }
            if meta.len() > MAX_FILE_BYTES || is_binary_file(entry.path()) {
                continue;
            }
            items.push(truncate_line(
                &relative(root, entry.path()),
                MAX_RESULT_PATH_CHARS,
            ));
        }
        if items.len() >= MAX_RESULTS {
            stop_reason = StopReason::ResultLimit;
            break 'scan;
        }
    }

    DirScan {
        items,
        scanned_files,
        scanned_bytes,
        elapsed_ms: started.elapsed().as_millis() as u64,
        stop_reason,
    }
}

fn truncate_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

fn relative(root: &Utf8PathBuf, path: &std::path::Path) -> String {
    path.strip_prefix(root.as_std_path())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_binary_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return true;
    };
    let mut head = [0u8; 8192];
    match file.read(&mut head) {
        Ok(read) => head[..read].contains(&0),
        Err(_) => true,
    }
}

/// 有界目录遍历（§8.4：ignore 规则、不跟随 symlink、跳过 binary 大文件）。
struct WalkBuilder {
    inner: ignore::Walk,
}

impl WalkBuilder {
    /// max_depth：None = 不限（index_files）；Some(n) 让 ignore crate
    /// 在深度 n 处停止（P0-13：list depth=2 不再遍历整棵树）。
    fn new(root: &Utf8PathBuf, max_depth: Option<usize>) -> Self {
        let mut builder = ignore::WalkBuilder::new(root.as_std_path());
        builder
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(false)
            .parents(true)
            .standard_filters(true)
            .max_depth(max_depth);
        Self {
            inner: builder.build(),
        }
    }
}

impl Iterator for WalkBuilder {
    type Item = Result<ignore::DirEntry, ignore::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// workspace 文件索引（`@` 引用补全用；跟随 .gitignore、不跟随 symlink，有界）。
///
/// 相对路径、按目录优先排序（`dir/` 条目在前，便于 @ 逐级下钻）。
pub fn index_files(root: &Utf8PathBuf, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let mut files: Vec<String> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    let started = Instant::now();
    let mut visited = 0u64;
    for entry in WalkBuilder::new(root, None) {
        visited = visited.saturating_add(1);
        if visited > MAX_SCAN_FILES || started.elapsed() > SCAN_DEADLINE {
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|kind| kind.is_symlink()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root.as_std_path()) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if meta.is_dir() {
            dirs.push(format!("{relative}/"));
        } else if meta.len() <= MAX_FILE_BYTES && !is_binary_file(entry.path()) {
            files.push(relative);
        }
        if dirs.len() + files.len() >= limit {
            break;
        }
    }
    dirs.sort_unstable();
    files.sort_unstable();
    dirs.extend(files);
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 ToolContext。
    fn test_ctx(root: &Utf8PathBuf) -> ToolContext {
        let local = crate::workspace::LocalWorkspace::new(root.clone(), true);
        ToolContext {
            workspace_root: root.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            artifacts_root: root.join(".artifacts").into(),
            session_id: "test".into(),
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            shell_path: None,
            snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::edit::SnapshotStore::new(16, 4),
            )),
            current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_goal: None,
            shell: local.shell.clone(),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(local),
            )),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            terminals: Default::default(),
            resources: None,
            resource_identity: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
            workspace_session: None,
        }
    }

    #[test]
    fn index_files_lists_relative_paths_dirs_first_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "x\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let files = index_files(&root, 100);
        // 目录优先（@ 引用逐级下钻）；路径为相对形式。
        assert!(files.contains(&"src/".to_string()), "{files:?}");
        assert!(files.contains(&"src/main.rs".to_string()), "{files:?}");
        assert!(
            !files.contains(&"ignored.txt".to_string()),
            ".gitignore 生效: {files:?}"
        );

        // 有界。
        for i in 0..300 {
            std::fs::write(root.join(format!("f{i:03}.txt")), "x\n").unwrap();
        }
        let files = index_files(&root, 50);
        assert_eq!(files.len(), 50, "索引必须受 limit 约束");
    }

    /// P0-13：list 的 max_depth 必须让 ignore 在指定深度停止遍历。
    #[test]
    fn walk_builder_respects_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/b/c/d.txt"), "x").unwrap();
        std::fs::write(root.join("a/top.txt"), "x").unwrap();

        // max_depth=2：深层条目（depth>2）不会出现在遍历结果里。
        let depths: Vec<usize> = WalkBuilder::new(&root, Some(2))
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.depth())
            .collect();
        assert!(
            depths.iter().all(|d| *d <= 2),
            "max_depth 必须限制遍历深度: {depths:?}"
        );

        // 对比：不限深度时深层条目可达。
        let depths: Vec<usize> = WalkBuilder::new(&root, None)
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.depth())
            .collect();
        assert!(
            depths.iter().any(|d| *d > 2),
            "无限制时可达深层: {depths:?}"
        );
    }

    /// P0-13 行为面：scan_dir（read 目录分支）depth=2 不返回深层路径。
    #[test]
    fn scan_dir_respects_depth_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/b/c/d.txt"), "x").unwrap();
        std::fs::write(root.join("a/top.txt"), "x").unwrap();
        let ctx = test_ctx(&root);
        let scan = scan_dir(&root, 2, &ctx);
        let items = scan.items.join("\n");
        assert!(items.contains("top.txt"), "depth 2 内文件应列出: {items}");
        assert!(
            !items.contains("d.txt"),
            "depth 2 之外的路径不得出现: {items}"
        );
    }

    #[test]
    fn index_zero_limit_and_binary_filter_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.join("text.txt"), b"hello").unwrap();
        std::fs::write(root.join("binary.bin"), b"a\0b").unwrap();
        assert!(index_files(&root, 0).is_empty());
        let indexed = index_files(&root, 10);
        assert!(indexed.contains(&"text.txt".into()));
        assert!(!indexed.contains(&"binary.bin".into()));
    }

    /// 取消时扫描必须停止（scan_dir 保留路径）。
    #[test]
    fn cancelled_scans_stop() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.join("binary.bin"), b"a\0b").unwrap();
        let ctx = test_ctx(&root);
        ctx.cancel.cancel();

        let scan = scan_dir(&root, 2, &ctx);
        assert!(
            scan.stop_reason == StopReason::Cancelled,
            "取消后必须停: {:?}",
            scan.stop_reason
        );
    }
}
