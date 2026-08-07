//! 有界目录/内容检索（文档 §8.4、§15.2 渐进检索）。
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
use crate::tool::{ToolContext, resolve_workspace_path};

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
    let root = resolve_workspace_path(&ctx.workspace_root, &args.path);
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

    'scan: for entry in WalkBuilder::new(&root, ctx) {
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
    let root = resolve_workspace_path(&ctx.workspace_root, &args.path);
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

    'scan: for entry in WalkBuilder::new(&root, ctx) {
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
        // binary 检测：读头 8 KiB 查 NUL。
        let head = match std::fs::read(entry.path()) {
            Ok(head) => head,
            Err(_) => continue,
        };
        if head.iter().take(8192).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&head);
        let relative_path = relative(&root, entry.path());
        for (line_idx, line) in text.lines().enumerate() {
            if pattern.is_match(line) {
                let line = truncate_line(line, MAX_LINE_CHARS);
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
    )
}

fn finish_scan(
    ctx: &ToolContext,
    items: Vec<String>,
    scanned_files: u64,
    scanned_bytes: u64,
    started: Instant,
    stop_reason: StopReason,
) -> ToolOutcome {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let snapshot = ScanSnapshot {
        items,
        scanned_files,
        scanned_bytes,
        elapsed_ms,
        stop_reason: stop_reason.as_str().to_string(),
    };
    let cursor = crate::ids::EventId::new_v7().to_string();
    {
        let mut store = ctx.scan_snapshots.lock().unwrap();
        // 有界：最多保留 16 个 snapshot。
        if store.len() >= 16 {
            let oldest = store.keys().next().cloned();
            if let Some(oldest) = oldest {
                store.remove(&oldest);
            }
        }
        store.insert(cursor.clone(), snapshot);
    }
    page(&cursor, ctx)
}

/// 翻页：从 snapshot 取 offset 窗口（不重新扫描，§8.4）。
fn page(cursor: &str, ctx: &ToolContext) -> ToolOutcome {
    let (snapshot_id, offset) = match cursor.split_once('#') {
        Some((id, offset)) => (id.to_string(), offset.parse::<usize>().unwrap_or(0)),
        None => (cursor.to_string(), 0),
    };
    let store = ctx.scan_snapshots.lock().unwrap();
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
    let mut outcome = ToolOutcome::succeeded("list", output);
    outcome.session_metadata = ToolMetadata {
        tool: "list".into(),
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
    fn new(root: &Utf8PathBuf, _ctx: &ToolContext) -> Self {
        let mut builder = ignore::WalkBuilder::new(root.as_std_path());
        builder
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(false)
            .parents(true)
            .standard_filters(true)
            .max_depth(None);
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
