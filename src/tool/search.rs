//! 带稳定分页快照的有界目录与内容检索。
//!
//! - 遵循 `.gitignore`，不跟随目录 symlink，跳过 binary 和超过 2 MiB 的普通源码候选；
//! - 单次默认计算预算：20,000 files、256 MiB scanned bytes、10 秒 deadline；
//! - 结果报告 `scanned_files`、`scanned_bytes`、`elapsed_ms` 和 `stop_reason=complete|result_limit|scan_limit|deadline|cancelled`；
//! - 分页保存有界有序结果 snapshot（默认最多 1,000 项），cursor 指向 snapshot + offset，
//!   翻页**不重新扫描** workspace（§20.2 场景 22）。

use std::time::Instant;

use camino::Utf8PathBuf;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};
use crate::tool::{ToolContext, path_rejected_outcome, resolve_tool_path};

/// 单次默认计算预算（§8.4）。
pub const MAX_SCAN_FILES: u64 = 20_000;
pub const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
pub const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// 超过 2 MiB 的普通源码候选跳过（§8.4）。
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 模型可见项数预算。
pub const MAX_RESULTS: usize = 1_000;
pub const PAGE_SIZE: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// 相对路径或目录；默认 workspace root。
    #[serde(default = "default_path")]
    pub path: String,
    /// 目录深度（默认 2）。
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// 上一页返回的 cursor（翻页不重新扫描）。
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_path() -> String {
    ".".to_string()
}

fn default_depth() -> usize {
    2
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// regex 模式（rust regex 语法）。
    pub pattern: String,
    #[serde(default = "default_path")]
    pub path: String,
    /// 上一页返回的 cursor。
    #[serde(default)]
    pub cursor: Option<String>,
}

/// 一次扫描的有界有序结果 snapshot（§8.4：cursor 指向 snapshot + offset）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSnapshot {
    /// 生成快照的工具（list/search；翻页结果按原工具名标记）。
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

pub fn list(args: ListArgs, ctx: &ToolContext) -> ToolOutcome {
    if let Some(cursor) = &args.cursor {
        return page(cursor, ctx);
    }
    let root = match resolve_tool_path(ctx, &args.path) {
        Ok(path) => path,
        Err(error) => return path_rejected_outcome("list", error),
    };
    if !root.is_dir() {
        return ToolOutcome::failed(
            "list",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!("status: failed\ntool: list\nerror: not_found\n\n{}", root),
                effect: None,
                artifact: None,
            },
        );
    }
    let started = Instant::now();
    let mut items: Vec<String> = Vec::new();
    let mut scanned_files = 0u64;
    let mut scanned_bytes = 0u64;
    let mut stop_reason = StopReason::Complete;

    'scan: for entry in WalkBuilder::new(&root, Some(args.depth)) {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if entry.depth() == 0 {
            continue; // root 本身
        }
        if entry.depth() > args.depth {
            continue;
        }
        if meta.is_dir() {
            items.push(format!("{}/", relative(&root, entry.path())));
        } else {
            scanned_files += 1;
            scanned_bytes += meta.len();
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            items.push(relative(&root, entry.path()));
        }
        if scanned_files >= MAX_SCAN_FILES {
            stop_reason = StopReason::ScanLimit;
            break 'scan;
        }
        if scanned_bytes >= MAX_SCAN_BYTES {
            stop_reason = StopReason::ScanLimit;
            break 'scan;
        }
        if started.elapsed() > SCAN_DEADLINE {
            stop_reason = StopReason::Deadline;
            break 'scan;
        }
        if ctx.cancel.is_cancelled() {
            stop_reason = StopReason::Cancelled;
            break 'scan;
        }
        if items.len() >= MAX_RESULTS {
            stop_reason = StopReason::ResultLimit;
            break 'scan;
        }
    }

    finish_scan(
        ctx,
        items,
        scanned_files,
        scanned_bytes,
        started,
        stop_reason,
        "list",
    )
}

pub fn search(args: SearchArgs, ctx: &ToolContext) -> ToolOutcome {
    if let Some(cursor) = &args.cursor {
        return page(cursor, ctx);
    }
    let pattern = match regex::Regex::new(&args.pattern) {
        Ok(pattern) => pattern,
        Err(error) => {
            return ToolOutcome::failed(
                "search",
                ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: rejected\ntool: search\nerror: invalid_regex\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    let root = match resolve_tool_path(ctx, &args.path) {
        Ok(path) => path,
        Err(error) => return path_rejected_outcome("search", error),
    };
    if !root.is_dir() {
        return ToolOutcome::failed(
            "search",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!("status: failed\ntool: search\nerror: not_found\n\n{root}"),
                effect: None,
                artifact: None,
            },
        );
    }
    let started = Instant::now();
    let mut items: Vec<String> = Vec::new();
    let mut scanned_files = 0u64;
    let mut scanned_bytes = 0u64;
    let mut stop_reason = StopReason::Complete;

    // 匹配预算（§8.4：100 matches、单行 300 chars、最多 32 KiB）。
    const MAX_MATCHES: usize = 100;
    const MAX_LINE_CHARS: usize = 300;
    const MAX_OUTPUT_BYTES: usize = 32 * 1024;
    let mut output_bytes = 0usize;

    'scan: for entry in WalkBuilder::new(&root, None) {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        scanned_files += 1;
        scanned_bytes += meta.len();
        if meta.len() > MAX_FILE_BYTES || meta.len() == 0 {
            continue;
        }
        // binary 检测：只读头 8 KiB 查 NUL（P1-8：不整文件读入内存）。
        let mut file = match std::fs::File::open(entry.path()) {
            Ok(file) => file,
            Err(_) => continue,
        };
        {
            use std::io::Read;
            let mut head = [0u8; 8192];
            let n = file.read(&mut head).unwrap_or(0);
            if head[..n].contains(&0) {
                continue;
            }
        }
        // head 读取已消费文件内容（小文件可能已到 EOF）：回退到文件头再流式读。
        {
            use std::io::{Seek, SeekFrom};
            let _ = file.seek(SeekFrom::Start(0));
        }
        // 流式按行匹配（P1-8：大文件不整读，内存峰值恒定）。
        let relative_path = relative(&root, entry.path());
        use std::io::BufRead;
        let reader = std::io::BufReader::new(file);
        for (line_idx, line) in reader.lines().enumerate() {
            let Ok(line) = line else { break };
            if pattern.is_match(&line) {
                let line = truncate_line(&line, MAX_LINE_CHARS);
                let item = format!("{}:{}: {}", relative_path, line_idx + 1, line);
                output_bytes += item.len();
                items.push(item);
                if items.len() >= MAX_MATCHES || output_bytes >= MAX_OUTPUT_BYTES {
                    stop_reason = StopReason::ResultLimit;
                    break 'scan;
                }
            }
        }
        if scanned_files >= MAX_SCAN_FILES {
            stop_reason = StopReason::ScanLimit;
            break 'scan;
        }
        if scanned_bytes >= MAX_SCAN_BYTES {
            stop_reason = StopReason::ScanLimit;
            break 'scan;
        }
        if started.elapsed() > SCAN_DEADLINE {
            stop_reason = StopReason::Deadline;
            break 'scan;
        }
        if ctx.cancel.is_cancelled() {
            stop_reason = StopReason::Cancelled;
            break 'scan;
        }
    }

    finish_scan(
        ctx,
        items,
        scanned_files,
        scanned_bytes,
        started,
        stop_reason,
        "search",
    )
}

fn finish_scan(
    ctx: &ToolContext,
    items: Vec<String>,
    scanned_files: u64,
    scanned_bytes: u64,
    started: Instant,
    stop_reason: StopReason,
    tool: &'static str,
) -> ToolOutcome {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let snapshot = ScanSnapshot {
        tool: tool.to_string(),
        items,
        scanned_files,
        scanned_bytes,
        elapsed_ms,
        stop_reason: stop_reason.as_str().to_string(),
    };
    let cursor = crate::ids::EventId::new_v7().to_string();
    {
        let mut store = crate::util::lock_mutex(&ctx.scan_snapshots, "scan_snapshots");
        // 有界：最多保留 16 个 snapshot（P1-6：按 cursor 有序淘汰最旧，
        // 此前 HashMap keys().next() 顺序不可预测，cursor 可能意外失效）。
        evict_oldest_snapshot(&mut store);
        store.insert(cursor.clone(), snapshot);
    }
    page(&cursor, ctx)
}

/// P1-6：snapshot 有界淘汰——淘汰 cursor 字典序最小的一项。
/// cursor 是 UUIDv7（时间前缀有序），字典序最小 ≈ 最旧。
pub fn evict_oldest_snapshot(store: &mut std::collections::HashMap<String, ScanSnapshot>) {
    const MAX_SNAPSHOTS: usize = 16;
    if store.len() >= MAX_SNAPSHOTS
        && let Some(oldest) = store.keys().min().cloned()
    {
        store.remove(&oldest);
    }
}

/// 翻页：从 snapshot 取 offset 窗口（不重新扫描，§8.4）。
fn page(cursor: &str, ctx: &ToolContext) -> ToolOutcome {
    let (snapshot_id, offset) = match cursor.split_once('#') {
        Some((id, offset)) => (id.to_string(), offset.parse::<usize>().unwrap_or(0)),
        None => (cursor.to_string(), 0),
    };
    let store = crate::util::lock_mutex(&ctx.scan_snapshots, "scan_snapshots");
    let Some(snapshot) = store.get(&snapshot_id) else {
        return ToolOutcome::failed(
            "page",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: failed\nerror: cursor_expired\n\ncursor 已失效（snapshot 被淘汰）；请重新搜索。".to_string(),
                effect: None,
                artifact: None,
            },
        );
    };
    let total = snapshot.items.len();
    let start = offset.min(total);
    let end = (start + PAGE_SIZE).min(total);
    let shown = end - start;
    let next_cursor = if end < total {
        Some(format!("{snapshot_id}#{end}"))
    } else {
        None
    };
    let body = snapshot.items[start..end].join(
        "
",
    );
    let output = format!(
        "status: succeeded
scanned_files: {}
scanned_bytes: {}
elapsed_ms: {}
stop_reason: {}
items: {shown} shown of {total}{}

{body}",
        snapshot.scanned_files,
        snapshot.scanned_bytes,
        snapshot.elapsed_ms,
        snapshot.stop_reason,
        match &next_cursor {
            Some(cursor) => format!(
                "
cursor: {cursor}"
            ),
            None => String::new(),
        },
    );
    let mut outcome = ToolOutcome::succeeded(&snapshot.tool, output);
    outcome.session_metadata = ToolMetadata {
        tool: snapshot.tool.clone(),
        target: Some(snapshot_id),
        ..Default::default()
    };
    outcome
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
    let utf8 = Utf8PathBuf::from_path_buf(path.to_path_buf()).unwrap_or_default();
    utf8.strip_prefix(root)
        .map(|p| p.to_string())
        .unwrap_or_else(|_| utf8.to_string())
}

/// 有界目录遍历（§8.4：ignore 规则、不跟随 symlink、跳过 binary 大文件）。
struct WalkBuilder {
    inner: ignore::Walk,
}

impl WalkBuilder {
    /// max_depth：None = 不限（search/index_files）；Some(n) 让 ignore crate
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
    let mut files: Vec<String> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for entry in WalkBuilder::new(root, None) {
        let Ok(entry) = entry else { continue };
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
        } else {
            files.push(relative);
        }
        if dirs.len() + files.len() >= limit {
            break;
        }
    }
    dirs.extend(files);
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 ToolContext（list 行为测试用）。
    fn test_ctx(root: &Utf8PathBuf) -> ToolContext {
        ToolContext {
            workspace_root: root.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            artifacts_root: root.join(".artifacts").into(),
            session_id: "test".into(),
            call_id: crate::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            shell_path: None,
            snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::edit::SnapshotStore::new(16, 4),
            )),
            current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
            interactive: false,
            allow_outside_workspace: true,
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

    /// P1-6：snapshot 有界淘汰按 cursor（UUIDv7 字典序≈时间序）取最小，
    /// 不能依赖 HashMap 无序迭代（cursor 意外失效会打断翻页）。
    #[test]
    fn snapshot_eviction_removes_oldest_cursor() {
        let mut store = std::collections::HashMap::new();
        // 插入 16 个（达到上限）后继续插入必须淘汰最小的 key。
        for i in 0..16u32 {
            store.insert(
                format!("00000000-0000-7000-8000-{i:012}"),
                ScanSnapshot {
                    tool: "list".into(),
                    items: vec![],
                    scanned_files: 0,
                    scanned_bytes: 0,
                    elapsed_ms: 0,
                    stop_reason: "complete".into(),
                },
            );
        }
        evict_oldest_snapshot(&mut store);
        assert_eq!(store.len(), 15, "达到上限时淘汰一个");
        assert!(
            !store.contains_key("00000000-0000-7000-8000-000000000000"),
            "必须淘汰字典序最小（最旧）的 cursor"
        );
        assert!(store.contains_key("00000000-0000-7000-8000-000000000001"));
    }

    /// P0-13：list 的 max_depth 必须让 ignore 在指定深度停止遍历
    /// （此前 WalkBuilder 无 max_depth，depth=2 也会扫描整棵树）。
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

    /// P0-13 行为面：list depth=2 不返回深层路径。
    #[test]
    fn list_respects_depth_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/b/c/d.txt"), "x").unwrap();
        std::fs::write(root.join("a/top.txt"), "x").unwrap();
        let ctx = test_ctx(&root);
        let outcome = list(
            ListArgs {
                path: ".".into(),
                depth: 2,
                cursor: None,
            },
            &ctx,
        );
        let output = outcome.model_payload.output;
        assert!(output.contains("top.txt"), "depth 2 内文件应列出: {output}");
        assert!(
            !output.contains("d.txt"),
            "depth 2 之外的路径不得出现: {output}"
        );
    }
}
