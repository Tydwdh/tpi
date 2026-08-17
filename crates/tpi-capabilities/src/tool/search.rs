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

use crate::tool::{ToolContext, path_rejected_outcome, resolve_tool_path};
use tpi_core::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

/// 路径不存在或非目录的统一诊断（§P0：区分「不存在」与「存在但非目录」，
/// 避免对文件路径误报 not_found 误导调用方）。
fn not_directory_outcome(tool: &'static str, root: &Utf8PathBuf) -> ToolOutcome {
    let exists = root.exists();
    let error = if exists {
        "not_a_directory"
    } else {
        "not_found"
    };
    let hint = if exists {
        "path 是文件而非目录（不是未找到）；该工具需要目录路径。"
    } else {
        "path 不存在。"
    };
    ToolOutcome::failed(
        tool,
        ModelPayload {
            status: ToolStatus::Failed,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!("status: failed\ntool: {tool}\nerror: {error}\n\n{root}\n{hint}"),
            effect: None,
            artifact: None,
        },
    )
}

/// 单次默认计算预算（§8.4）。
pub const MAX_SCAN_FILES: u64 = 20_000;
pub const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
pub const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// 超过 2 MiB 的普通源码候选跳过（§8.4）。
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 模型可见项数预算。
pub const MAX_RESULTS: usize = 1_000;
pub const PAGE_SIZE: usize = 200;
const MAX_RESULT_PATH_CHARS: usize = 2_048;

fn default_path() -> String {
    ".".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct GlobArgs {
    /// 文件名 glob 模式（如 `**/*.rs`、`src/**/*.ts`、`Cargo.toml`）。
    pub pattern: String,
    #[serde(default = "default_path")]
    pub path: String,
    /// 上一页返回的 cursor（翻页不重新扫描）。
    #[serde(default)]
    pub cursor: Option<String>,
    /// 目录深度（默认不限）。
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// regex 模式（rust regex 语法）。示例："fn estimate_request"、"TODO\\(.*\\)"。
    pub pattern: String,
    #[serde(default = "default_path")]
    pub path: String,
    /// 上一页返回的 cursor。
    #[serde(default)]
    pub cursor: Option<String>,
    /// 排除项：无 glob 元字符的条目按路径组件匹配（如 "tests"、"vendor"）；
    /// 含 glob 元字符（`* ? [ {`）的条目按完整 glob 匹配相对路径
    /// （如 `**/vendor/**`）。用于过滤 test/fixture/vendor 等噪音。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 结果条数上限（默认 100，最大 1000）。达到上限即停，可用 cursor 翻页继续。
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// 仅搜索匹配这些 glob 的文件（如 `**/*.rs`、`*.toml`）；空 = 不过滤。
    /// 单文件 path 时忽略。
    #[serde(default)]
    pub include: Vec<String>,
    /// 匹配行前后各显示多少行上下文（默认 0；等价 `rg -C N`）。
    /// >0 时输出上下文行（`path-N- content`），不计入 max_results。
    #[serde(default)]
    pub context: usize,
    /// 把 pattern 当作字面量字符串而非正则（默认 false；等价 `rg -F`）。
    #[serde(default)]
    pub literal: bool,
}

/// search 结果条数上限硬顶（§P1：max_results 默认 100，最多 1000）。
pub const MAX_SEARCH_RESULTS: usize = 1_000;

fn default_max_results() -> usize {
    100
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

/// 目录扫描结果（`read` 目录分支复用；§list 删除后由 read 承担目录浏览）。
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

/// glob：按文件名模式查找文件（§P1；opencode 同款语义）。
///
/// - 遵循 `.gitignore`、不跟随 symlink；结果按修改时间降序（最近修改在前），
///   同 mtime 按路径字典序；
/// - 分页复用 ScanSnapshot（cursor 翻页不重新扫描）。
pub fn glob(args: GlobArgs, ctx: &ToolContext) -> ToolOutcome {
    if let Some(cursor) = &args.cursor {
        return page(cursor, ctx);
    }
    let root = match resolve_tool_path(ctx, &args.path) {
        Ok(path) => path,
        Err(error) => return path_rejected_outcome("glob", error),
    };
    if !root.is_dir() {
        return not_directory_outcome("glob", &root);
    }
    let matcher = match globset::Glob::new(&args.pattern).map(|g| g.compile_matcher()) {
        Ok(matcher) => matcher,
        Err(error) => {
            return ToolOutcome::failed(
                "glob",
                ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("status: rejected\ntool: glob\nerror: invalid_glob\n\n{error}"),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    let started = Instant::now();
    let mut items: Vec<(String, std::time::SystemTime)> = Vec::new();
    let mut scanned_files = 0u64;
    let mut scanned_bytes = 0u64;
    let mut stop_reason = StopReason::Complete;

    'scan: for entry in WalkBuilder::new(&root, args.depth) {
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
        if !meta.is_file() {
            continue;
        }
        // ISSUE-041：scanned_files 统计**遍历到的全部**文件（与 list 的
        // 语义一致），而不是只统计命中——否则巨大仓库匹配 0 个时 scan_limit
        // 永不触发，scanned_files 与真实遍历量无关。字节同理。
        scanned_files = scanned_files.saturating_add(1);
        scanned_bytes = scanned_bytes.saturating_add(meta.len());
        if scanned_files >= MAX_SCAN_FILES || scanned_bytes >= MAX_SCAN_BYTES {
            stop_reason = StopReason::ScanLimit;
            break 'scan;
        }
        let rel = relative(&root, entry.path());
        if !matcher.is_match(rel.as_str()) {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        items.push((rel, mtime));
        if items.len() >= MAX_RESULTS {
            stop_reason = StopReason::ResultLimit;
            break 'scan;
        }
    }
    // §P1：按修改时间降序（最近修改在前；opencode glob 行为），同刻按路径序。
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let items: Vec<String> = items.into_iter().map(|(path, _)| path).collect();
    finish_scan(
        ctx,
        items,
        scanned_files,
        scanned_bytes,
        started,
        stop_reason,
        "glob",
        false,
        None,
    )
}

/// 构造正则 matcher：`literal=true` 时按字面量（`rg -F`），否则 rust regex。
fn build_matcher(args: &SearchArgs) -> Result<grep::regex::RegexMatcher, String> {
    let mut builder = grep::regex::RegexMatcherBuilder::new();
    if args.literal {
        builder.fixed_strings(true);
    }
    builder.build(&args.pattern).map_err(|e| e.to_string())
}

/// 构造 Searcher：`context>0` 时配置上下文行（前后各 N 行）。
fn build_searcher(context: usize) -> grep::searcher::Searcher {
    if context > 0 {
        grep::searcher::SearcherBuilder::new()
            .before_context(context)
            .after_context(context)
            .build()
    } else {
        grep::searcher::Searcher::new()
    }
}

/// 把 exclude 条目分成「组件子串匹配」与「glob 匹配」两组：
/// 含 glob 元字符（`* ? [ {`）→ 完整 glob 匹配相对路径（大小写不敏感）；
/// 否则 → 路径组件子串匹配（向后兼容，如 `tests`、`vendor`）。
/// 返回 (组件列表, glob set；glob 编译失败返回 Err)。
fn build_exclude_filters(
    exclude: &[String],
) -> (Vec<String>, Result<Option<globset::GlobSet>, String>) {
    let mut components = Vec::new();
    let mut builder = globset::GlobSetBuilder::new();
    let mut has_glob = false;
    for s in exclude.iter().filter(|s| !s.trim().is_empty()) {
        let trimmed = s.trim();
        let has_meta = trimmed.contains(['*', '?', '[', '{']);
        if has_meta {
            match globset::GlobBuilder::new(trimmed)
                .case_insensitive(true)
                .build()
            {
                Ok(glob) => {
                    builder.add(glob);
                    has_glob = true;
                }
                Err(e) => return (components, Err(e.to_string())),
            }
        } else {
            components.push(trimmed.to_ascii_lowercase());
        }
    }
    let set = if has_glob {
        match builder.build() {
            Ok(set) => Some(set),
            Err(e) => return (components, Err(e.to_string())),
        }
    } else {
        None
    };
    (components, Ok(set))
}

pub fn search(args: SearchArgs, ctx: &ToolContext) -> ToolOutcome {
    if let Some(cursor) = &args.cursor {
        return page(cursor, ctx);
    }
    if args.pattern.len() > 32 * 1024 {
        return ToolOutcome::failed(
            "search",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: rejected\ntool: search\nerror: regex_too_large\n\n正则表达式最多 32 KiB。".into(),
                effect: None,
                artifact: None,
            },
        );
    }
    let matcher = match build_matcher(&args) {
        Ok(matcher) => matcher,
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
    // §P1：search 支持单文件路径——文件路径直接搜该文件（ripgrep 语义），
    // 不再误报 not_found。
    if root.is_file() {
        return search_single_file(&matcher, &root, &args, ctx);
    }
    if !root.is_dir() {
        return not_directory_outcome("search", &root);
    }
    // §P1：include glob 过滤（文件名模式；编译失败 → rejected）。
    let include = match build_glob_set(&args.include) {
        Ok(set) => set,
        Err(error) => {
            return ToolOutcome::failed(
                "search",
                ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: rejected\ntool: search\nerror: invalid_glob\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    let started = Instant::now();
    let mut scanned_files = 0u64;
    let mut scanned_bytes = 0u64;
    let mut stop_reason = StopReason::Complete;
    // §exclude：无 glob 元字符 → 路径组件匹配（向后兼容）；含元字符 → glob。
    let (component_excludes, glob_excludes) = build_exclude_filters(&args.exclude);
    let glob_excludes = match glob_excludes {
        Ok(set) => set,
        Err(error) => {
            return ToolOutcome::failed(
                "search",
                ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: rejected\ntool: search\nerror: invalid_exclude_glob\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };

    // 匹配预算（§8.4：max_results 条、单行 300 chars、最多 32 KiB）。
    let max_results = args.max_results.clamp(1, MAX_SEARCH_RESULTS);
    const MAX_LINE_CHARS: usize = 300;
    const MAX_OUTPUT_BYTES: usize = 32 * 1024;
    let mut output_bytes = 0usize;
    // 跨文件累计匹配数（max_results 是全局上限，不是每文件上限）。
    let mut match_count = 0usize;

    // ripgrep 内核：Searcher（memchr 加速逐行搜索 + 行号追踪）逐文件复用。
    // context>0 时配置上下文行（Sink 的 context() 消费）。
    let mut searcher = build_searcher(args.context);
    // 每文件匹配行（按文件分组，输出 [path#revision] header）。
    let mut file_groups: Vec<(String, String, Vec<String>)> = Vec::new();
    'scan: for entry in WalkBuilder::new(&root, None) {
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
        let rel = relative(&root, entry.path());
        // §exclude：组件子串匹配（如 tests）或完整 glob（如 `**/vendor/**`）。
        if let Some(set) = &glob_excludes
            && set.is_match(rel.as_str())
        {
            continue;
        }
        if !component_excludes.is_empty() {
            let rel_lower = rel.as_str().to_ascii_lowercase();
            let excluded = component_excludes.iter().any(|pattern| {
                rel_lower
                    .split(['/', '\\'])
                    .any(|component| component.contains(pattern.as_str()))
            });
            if excluded {
                continue;
            }
        }
        // §P1：include glob 过滤（如 `**/*.rs`）。
        if !include.is_empty() && !include.is_match(rel.as_str()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        scanned_files = scanned_files.saturating_add(1);
        scanned_bytes = scanned_bytes.saturating_add(meta.len());
        if scanned_files >= MAX_SCAN_FILES || scanned_bytes >= MAX_SCAN_BYTES {
            stop_reason = StopReason::ScanLimit;
            break 'scan;
        }
        if meta.len() > MAX_FILE_BYTES || meta.len() == 0 {
            continue;
        }
        // 整读 bytes（同一份：hash + record + 搜索；§anchor 不变量）。
        let bytes = match std::fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        // 计算 revision 并注册 snapshot（edit_range stale 恢复用）。
        let revision = crate::tool::edit::revision_of(&bytes);
        // §模型协议：record 先行以分配 r{id}，再用 display_revision 输出。
        let display_rev = if let Ok(snapshot) = crate::tool::edit::build_snapshot(
            Utf8PathBuf::from_path_buf(entry.path().to_path_buf()).unwrap_or_default(),
            bytes.clone(),
        ) {
            let mut store = tpi_core::util::lock_mutex(&ctx.snapshot_store, "snapshot_store");
            store.record(snapshot);
            store.display_revision(&revision)
        } else {
            revision
        };
        // 该文件局部匹配行（跨文件分组，不直接塞全局 items）。
        let mut file_items: Vec<String> = Vec::new();
        let limits = SearchLimits {
            max_results,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_line_chars: MAX_LINE_CHARS,
        };
        if search_file_into(
            &mut searcher,
            &matcher,
            &bytes,
            &rel,
            &limits,
            &mut file_items,
            &mut output_bytes,
            &mut match_count,
        ) {
            stop_reason = StopReason::ResultLimit;
        }
        if !file_items.is_empty() {
            file_groups.push((rel.clone(), display_rev, file_items));
        }
        if stop_reason == StopReason::ResultLimit {
            break 'scan;
        }
    }

    // 拼分组文本：每个文件一段，段首 [path#revision]（§anchor）。
    // 同时把扁平 items 存进 snapshot（供 cursor 翻页，翻页不带 revision）。
    let mut grouped_body = String::new();
    for (rel, display_rev, file_items) in &file_groups {
        if !grouped_body.is_empty() {
            grouped_body.push('\n');
        }
        grouped_body.push_str(&format!("[{}#{display_rev}]", display_rel(rel)));
        for item in file_items {
            grouped_body.push('\n');
            grouped_body.push_str(item);
        }
    }
    let items: Vec<String> = file_groups
        .into_iter()
        .flat_map(|(_, _, items)| items)
        .collect();

    finish_scan(
        ctx,
        items,
        scanned_files,
        scanned_bytes,
        started,
        stop_reason,
        "search",
        // context>0：保留 ripgrep 输出顺序（匹配行与上下文行交错），
        // 不按噪音目录重排（否则上下文被拆散）。
        args.context == 0,
        // 首屏用分组文本（[path#revision] + 行）；翻页仍走扁平 items。
        Some(grouped_body),
    )
}

/// §P1：单文件搜索（search path 指向文件时；ripgrep 语义，不再误报 not_found）。
fn search_single_file(
    matcher: &grep::regex::RegexMatcher,
    path: &Utf8PathBuf,
    args: &SearchArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    let started = Instant::now();
    let mut items: Vec<String> = Vec::new();
    let mut output_bytes = 0usize;
    let mut stop_reason = StopReason::Complete;
    let Ok(meta) = path.metadata() else {
        return not_directory_outcome("search", path);
    };
    let scanned_files = 1u64;
    let scanned_bytes = meta.len();
    if meta.len() > 0 && meta.len() <= MAX_FILE_BYTES {
        let rel = relative(&ctx.workspace_root, path.as_std_path());
        // 整读 bytes（hash + record + 搜索同一份；§anchor 不变量）。
        let bytes = match std::fs::read(path.as_std_path()) {
            Ok(bytes) => bytes,
            Err(_) => {
                return finish_scan(ctx, items, 1, 0, started, stop_reason, "search", true, None);
            }
        };
        let revision = crate::tool::edit::revision_of(&bytes);
        let display_rev =
            if let Ok(snapshot) = crate::tool::edit::build_snapshot(path.clone(), bytes.clone()) {
                let mut store = tpi_core::util::lock_mutex(&ctx.snapshot_store, "snapshot_store");
                store.record(snapshot);
                store.display_revision(&revision)
            } else {
                revision
            };
        let mut searcher = build_searcher(args.context);
        let mut match_count = 0usize;
        let limits = SearchLimits {
            max_results: args.max_results.clamp(1, MAX_SEARCH_RESULTS),
            max_output_bytes: 32 * 1024,
            max_line_chars: 300,
        };
        if search_file_into(
            &mut searcher,
            matcher,
            &bytes,
            &rel,
            &limits,
            &mut items,
            &mut output_bytes,
            &mut match_count,
        ) {
            stop_reason = StopReason::ResultLimit;
        }
        if !items.is_empty() {
            // 单文件也输出 [path#revision] header（与其他路径一致）。
            let mut grouped = format!("[{}#{display_rev}]", display_rel(&rel));
            for item in &items {
                grouped.push('\n');
                grouped.push_str(item);
            }
            finish_scan(
                ctx,
                items.clone(),
                scanned_files,
                scanned_bytes,
                started,
                stop_reason,
                "search",
                args.context == 0,
                Some(grouped),
            )
        } else {
            finish_scan(
                ctx,
                items,
                scanned_files,
                scanned_bytes,
                started,
                stop_reason,
                "search",
                args.context == 0,
                None,
            )
        }
    } else {
        finish_scan(
            ctx,
            items,
            scanned_files,
            scanned_bytes,
            started,
            stop_reason,
            "search",
            args.context == 0,
            None,
        )
    }
}

/// 搜索单文件时的预算（§P1：max_results 条、单行 300 chars、最多 32 KiB）。
struct SearchLimits {
    max_results: usize,
    max_output_bytes: usize,
    max_line_chars: usize,
}

/// 用 ripgrep 内核搜索单个文件，匹配累积进 `items`。返回 true = 达上限。
/// 整读 bytes（二进制预检 + 搜索同一份 bytes；供 revision/record 复用）。
#[allow(clippy::too_many_arguments)] // 与 finish_scan 同：统计/预算参数多但语义单一。
fn search_file_into(
    searcher: &mut grep::searcher::Searcher,
    matcher: &grep::regex::RegexMatcher,
    bytes: &[u8],
    rel: &str,
    limits: &SearchLimits,
    items: &mut Vec<String>,
    output_bytes: &mut usize,
    match_count: &mut usize,
) -> bool {
    // binary 预检（头 8 KiB 查 NUL；与目录模式一致）。
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return false;
    }
    let rel = truncate_line(rel, MAX_RESULT_PATH_CHARS);
    let mut sink = SearchSink {
        items,
        output_bytes,
        rel: rel.to_string(),
        max_results: limits.max_results,
        max_output_bytes: limits.max_output_bytes,
        max_line_chars: limits.max_line_chars,
        match_count,
        limit_hit: false,
    };
    // 兜底：binary 检测（遇 NUL 停止该文件搜索）；默认 \n 行分隔，
    // CRLF 行的 \r 由 sink 剥离。
    searcher.set_binary_detection(grep::searcher::BinaryDetection::quit(b'\0'));
    let _ = searcher.search_reader(matcher, bytes, &mut sink);
    sink.limit_hit
}

/// 搜索 sink：累积 `rel:line: content` 项（匹配行）与 `rel-N- content`
/// 项（context 上下文行），受条数/字节预算约束。
/// 行号来自 ripgrep Searcher（1-based）。
struct SearchSink<'a> {
    items: &'a mut Vec<String>,
    output_bytes: &'a mut usize,
    rel: String,
    max_results: usize,
    max_output_bytes: usize,
    max_line_chars: usize,
    /// 跨文件累计匹配数（context 行不计入；max_results 是全局上限）。
    match_count: &'a mut usize,
    limit_hit: bool,
}

impl grep::searcher::Sink for SearchSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        mat: &grep::searcher::SinkMatch,
    ) -> Result<bool, Self::Error> {
        let Some(line_number) = mat.line_number() else {
            return Ok(true);
        };
        // 非 UTF-8 行：跳过（与 BufRead::lines 遇错中止文件语义近似）。
        let Ok(line) = std::str::from_utf8(mat.bytes()) else {
            return Ok(true);
        };
        // CRLF：Searcher 按 \n 切行，行尾 \r 属内容，剥离后再输出。
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line = truncate_line(line, self.max_line_chars);
        let item = format!("{}:{line_number}: {line}", self.rel);
        let item_len = item.len();
        if self.output_bytes.saturating_add(item_len) > self.max_output_bytes {
            self.limit_hit = true;
            return Ok(false);
        }
        *self.output_bytes = self.output_bytes.saturating_add(item_len);
        self.items.push(item);
        *self.match_count += 1;
        if *self.match_count >= self.max_results {
            self.limit_hit = true;
            return Ok(false);
        }
        Ok(true)
    }

    /// context 行（Before/After）：`rel-N- content` 格式（pi 同款），
    /// 不计入 max_results，只受字节预算约束。Match 由默认实现转发到 matched。
    fn context(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        context: &grep::searcher::SinkContext,
    ) -> Result<bool, Self::Error> {
        if matches!(context.kind(), grep::searcher::SinkContextKind::Other) {
            return Ok(true); // 未知 context 类型跳过
        }
        let Some(line_number) = context.line_number() else {
            return Ok(true);
        };
        let Ok(line) = std::str::from_utf8(context.bytes()) else {
            return Ok(true);
        };
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line = truncate_line(line, self.max_line_chars);
        let item = format!("{}-{line_number}- {line}", self.rel);
        let item_len = item.len();
        if self.output_bytes.saturating_add(item_len) > self.max_output_bytes {
            self.limit_hit = true;
            return Ok(false);
        }
        *self.output_bytes = self.output_bytes.saturating_add(item_len);
        self.items.push(item);
        Ok(true)
    }
}

/// 编译 include glob 模式集（§P1：`**/*.rs` 等；空列表返回空 set = 不过滤）。
fn build_glob_set(patterns: &[String]) -> Result<globset::GlobSet, String> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns.iter().filter(|s| !s.trim().is_empty()) {
        builder.add(globset::Glob::new(pattern).map_err(|e| e.to_string())?);
    }
    builder.build().map_err(|e| e.to_string())
}

/// 展示用路径（与 read 的 display_path 语义一致：相对路径原样，绝对转相对）。
fn display_rel(rel: &str) -> String {
    rel.to_string()
}

#[allow(clippy::too_many_arguments)] // §P1：统计字段 + resort 开关，参数多但语义单一。
fn finish_scan(
    ctx: &ToolContext,
    mut items: Vec<String>,
    scanned_files: u64,
    scanned_bytes: u64,
    started: Instant,
    stop_reason: StopReason,
    tool: &'static str,
    resort: bool,
    // 首屏自定义 body（如 search 的分组 [path#revision]）；None = 默认 page()。
    first_page_body: Option<String>,
) -> ToolOutcome {
    // §工具改进：噪音目录后置排序——src/根目录源码优先露出，tests/fixtures/
    // vendor 等噪音结果沉底（各自组内保持字典序），避免 100 条上限被
    // 测试文件噪音淹没。glob 已按 mtime 降序排好，resort=false 保留其顺序。
    if resort {
        items.sort_unstable_by(|a, b| noise_rank(a).cmp(&noise_rank(b)).then_with(|| a.cmp(b)));
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let snapshot = ScanSnapshot {
        tool: tool.to_string(),
        items,
        scanned_files,
        scanned_bytes,
        elapsed_ms,
        stop_reason: stop_reason.as_str().to_string(),
    };
    let cursor = tpi_core::ids::EventId::new_v7().to_string();
    {
        let mut store = tpi_core::util::lock_mutex(&ctx.scan_snapshots, "scan_snapshots");
        // 有界：最多保留 16 个 snapshot（P1-6：按 cursor 有序淘汰最旧，
        // 此前 HashMap keys().next() 顺序不可预测，cursor 可能意外失效）。
        evict_oldest_snapshot(&mut store);
        store.insert(cursor.clone(), snapshot);
    }
    let mut outcome = if let Some(body) = first_page_body {
        // 首屏自定义 body（search 分组 [path#revision]）：拼接报告头 + body。
        let total = 0; // body 已是完整分组文本，不显示 count（page() 会算）。
        let _ = total;
        let output = format!(
            "status: succeeded\nscanned_files: {scanned_files}\nscanned_bytes: {scanned_bytes}\nelapsed_ms: {elapsed_ms}\nstop_reason: {}\n\n{body}",
            stop_reason.as_str(),
        );
        ToolOutcome::succeeded(tool, output).with_metadata(ToolMetadata {
            tool: tool.to_string(),
            target: Some(cursor.clone()),
            ..Default::default()
        })
    } else {
        page(&cursor, ctx)
    };
    // §工具改进：命中结果上限（ResultLimit）时给出排除/收窄指引——
    // 用户不必反复换 path 缩小范围。
    if stop_reason == StopReason::ResultLimit {
        outcome.model_payload.output.push_str(
            "\n\n结果达上限。可加 exclude 排除目录（如 exclude=[\"tests\"]）或收窄 path 后重新搜索。",
        );
    }
    outcome
}

/// 噪音目录排序等级：0 = 源码/根目录（优先），1 = 噪音（沉底）。
/// 按路径片段匹配（`tests/`、`test/`、`testdata/`、`fixtures/`、`vendor/`、
/// `node_modules/`、`migrations/`）。
fn noise_rank(item: &str) -> u8 {
    const NOISE: &[&str] = &[
        "tests/",
        "test/",
        "testdata/",
        "fixtures/",
        "vendor/",
        "node_modules/",
        "migrations/",
        ".github/",
    ];
    let lower = item.to_ascii_lowercase();
    if NOISE.iter().any(|n| lower.contains(n)) {
        1
    } else {
        0
    }
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
    if cursor.is_empty() || cursor.len() > 256 || cursor.chars().any(char::is_control) {
        return invalid_cursor_outcome();
    }
    let (snapshot_id, offset) = match cursor.split_once('#') {
        Some((id, offset)) if !id.is_empty() => match offset.parse::<usize>() {
            Ok(offset) => (id.to_string(), offset),
            Err(_) => return invalid_cursor_outcome(),
        },
        Some(_) => return invalid_cursor_outcome(),
        None => (cursor.to_string(), 0),
    };
    let store = tpi_core::util::lock_mutex(&ctx.scan_snapshots, "scan_snapshots");
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

fn invalid_cursor_outcome() -> ToolOutcome {
    ToolOutcome::failed(
        "page",
        ModelPayload {
            status: ToolStatus::Rejected,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: "status: rejected\nerror: invalid_cursor".into(),
            effect: None,
            artifact: None,
        },
    )
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

    /// 构造最小 ToolContext（list 行为测试用）。
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
            shell: local.shell.clone(),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(local),
            )),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            terminals: Default::default(),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
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
                    tool: "search".into(),
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
    fn malformed_cursor_is_rejected_instead_of_restarting_first_page() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let ctx = test_ctx(&root);
        for cursor in ["", "missing#not-a-number", "#1"] {
            let outcome = page(cursor, &ctx);
            assert_eq!(outcome.status, ToolStatus::Rejected);
            assert!(outcome.model_payload.output.contains("invalid_cursor"));
        }
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

    #[test]
    fn cancelled_scans_stop_before_skipped_files() {
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

        let searched = search(
            SearchArgs {
                pattern: "a".into(),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        assert!(
            searched
                .model_payload
                .output
                .contains("stop_reason: cancelled")
        );
    }

    #[test]
    fn oversized_regex_is_rejected_before_compilation() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let ctx = test_ctx(&root);
        let outcome = search(
            SearchArgs {
                pattern: "a".repeat(32 * 1024 + 1),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Rejected);
        assert!(outcome.model_payload.output.contains("regex_too_large"));
    }

    /// §P0：区分「不存在」与「存在但非目录」——对文件路径不再误报 not_found。
    #[test]
    fn missing_and_non_directory_paths_are_distinguished() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.join("a.txt"), "needle\n").unwrap();
        let ctx = test_ctx(&root);

        // read 目录分支对不存在路径走文件读取的 NotFound（语义由 files.rs
        // 覆盖）；此处仅保留 search 对不存在路径的 not_found 断言。

        // search 对不存在路径：not_found。
        let searched = search(
            SearchArgs {
                pattern: "x".into(),
                path: "nope".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        assert!(searched.model_payload.output.contains("not_found"));
    }

    /// §P1：search 接受单文件路径（ripgrep 语义）——直接搜该文件，不再报错。
    #[test]
    fn search_accepts_single_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.join("a.txt"), "line one\nneedle here\n").unwrap();
        let ctx = test_ctx(&root);
        let outcome = search(
            SearchArgs {
                pattern: "needle".into(),
                path: "a.txt".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        let text = outcome.model_payload.output;
        assert!(text.contains("a.txt:2: needle here"), "{text}");
        assert!(!text.contains("line one"));
    }

    /// §P1：include glob 只搜匹配文件（`**/*.rs`）。
    #[test]
    fn search_include_glob_filters_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "needle\n").unwrap();
        std::fs::write(root.join("src/a.txt"), "needle\n").unwrap();
        let ctx = test_ctx(&root);
        let outcome = search(
            SearchArgs {
                pattern: "needle".into(),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: vec!["**/*.rs".into()],
                context: 0,
                literal: false,
            },
            &ctx,
        );
        let text = outcome.model_payload.output;
        assert!(text.contains("src/a.rs:1: needle"), "{text}");
        assert!(!text.contains("a.txt"), "include 过滤必须排除 .txt: {text}");
    }

    /// §P1：max_results 限制命中条数并报 result_limit。
    #[test]
    fn search_max_results_bounds_hits() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        for i in 0..3 {
            std::fs::write(root.join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let ctx = test_ctx(&root);
        let outcome = search(
            SearchArgs {
                pattern: "needle".into(),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 2,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        let text = outcome.model_payload.output;
        assert!(text.contains("stop_reason: result_limit"), "{text}");
        assert_eq!(
            text.matches(":1: needle").count(),
            2,
            "max_results=2 只能有 2 条命中: {text}"
        );
    }

    /// §P1：CRLF 文件匹配行剥离 \r（Windows 常见），输出干净。
    #[test]
    fn search_strips_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.join("win.txt"), b"needle\r\nsecond\r\n").unwrap();
        let ctx = test_ctx(&root);
        let outcome = search(
            SearchArgs {
                pattern: "needle".into(),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        let text = outcome.model_payload.output;
        assert!(
            text.contains("win.txt:1: needle"),
            "CRLF 必须剥离: {text:?}"
        );
        assert!(!text.contains("\\r"), "不得残留 \\r: {text:?}");
    }

    /// §P1：glob 按文件名模式匹配，结果按修改时间降序（最近修改在前）。
    #[test]
    fn glob_matches_pattern_and_sorts_by_mtime() {
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // 文件名顺序与 mtime 顺序相反：zzz 字典序最大但 mtime 最新。
        // 只有 glob 按 mtime 降序（且 finish_scan 保留顺序）时 zzz 才排最前；
        // 若被字典序覆盖，aaa 会排到 zzz 前——测试即失败。
        std::fs::write(root.join("src/aaa.rs"), "x\n").unwrap();
        std::fs::write(root.join("src/zzz.rs"), "y\n").unwrap();
        std::fs::write(root.join("src/note.txt"), "z\n").unwrap();
        // 显式设置 mtime：aaa 更早，zzz 更晚。
        let now = SystemTime::now();
        let set_mtime = |path: &str, age: u64| {
            std::fs::File::options()
                .write(true)
                .open(root.join(path))
                .unwrap()
                .set_modified(now - Duration::from_secs(age))
                .unwrap();
        };
        set_mtime("src/aaa.rs", 100);
        set_mtime("src/zzz.rs", 10);
        let ctx = test_ctx(&root);
        let outcome = glob(
            GlobArgs {
                pattern: "**/*.rs".into(),
                path: ".".into(),
                cursor: None,
                depth: None,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        let text = outcome.model_payload.output;
        let zzz_pos = text.find("src/zzz.rs").expect("含 zzz.rs: {text}");
        let aaa_pos = text.find("src/aaa.rs").expect("含 aaa.rs: {text}");
        assert!(
            zzz_pos < aaa_pos,
            "mtime 最新必须排最前（不被字典序覆盖）: {text}"
        );
        assert!(!text.contains("note.txt"), "非 .rs 不匹配: {text}");
    }

    /// §P1：glob 无效模式 → rejected。
    #[test]
    fn glob_invalid_pattern_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let ctx = test_ctx(&root);
        let outcome = glob(
            GlobArgs {
                pattern: "[".into(),
                path: ".".into(),
                cursor: None,
                depth: None,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Rejected);
        assert!(outcome.model_payload.output.contains("invalid_glob"));
    }

    /// literal=true：pattern 按字面量匹配（`rg -F` 语义），正则元字符不再生效。
    #[test]
    fn literal_treats_pattern_as_fixed_string() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        // `a.b` 字面量只匹配含点号的；正则语义会匹配 axb。
        std::fs::write(root.join("f.txt"), "axb\na.b\n").unwrap();
        let ctx = test_ctx(&root);

        let literal = search(
            SearchArgs {
                pattern: "a.b".into(),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: true,
            },
            &ctx,
        );
        let text = literal.model_payload.output;
        assert!(text.contains("f.txt:2: a.b"), "字面量匹配点号行: {text}");
        assert!(!text.contains("axb"), "literal 不把 . 当通配符: {text}");

        // 对照：正则语义下 `a.b` 匹配 axb。
        let regex = search(
            SearchArgs {
                pattern: "a.b".into(),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        let text = regex.model_payload.output;
        assert!(text.contains("axb"), "正则把 . 当通配符: {text}");
    }

    /// context=N：匹配行前后各 N 行以 `path-N- text` 输出，且保留 ripgrep 顺序。
    #[test]
    fn context_emits_before_after_lines_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.join("f.txt"), "one\ntwo\nMATCH\nfour\nfive\n").unwrap();
        let ctx = test_ctx(&root);
        let outcome = search(
            SearchArgs {
                pattern: "MATCH".into(),
                path: ".".into(),
                cursor: None,
                exclude: Vec::new(),
                max_results: 100,
                include: Vec::new(),
                context: 1,
                literal: false,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        let text = outcome.model_payload.output;
        assert!(text.contains("f.txt:3: MATCH"), "匹配行: {text}");
        assert!(text.contains("f.txt-2- two"), "前 1 行: {text}");
        assert!(text.contains("f.txt-4- four"), "后 1 行: {text}");
        // 顺序：before → match → after（context>0 不按噪音重排）。
        let before = text.find("f.txt-2- two").unwrap();
        let matched = text.find("f.txt:3: MATCH").unwrap();
        let after = text.find("f.txt-4- four").unwrap();
        assert!(before < matched && matched < after, "顺序: {text}");
    }

    /// exclude 含 glob 元字符：按完整 glob 排除（`**/vendor/**`），
    /// 无元字符条目仍组件匹配（向后兼容）。
    #[test]
    fn exclude_supports_glob_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join("src/vendor")).unwrap();
        std::fs::write(root.join("src/app.rs"), "needle\n").unwrap();
        std::fs::write(root.join("src/vendor/lib.rs"), "needle\n").unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/t.rs"), "needle\n").unwrap();
        let ctx = test_ctx(&root);
        let outcome = search(
            SearchArgs {
                pattern: "needle".into(),
                path: ".".into(),
                cursor: None,
                exclude: vec!["**/vendor/**".into(), "tests".into()],
                max_results: 100,
                include: Vec::new(),
                context: 0,
                literal: false,
            },
            &ctx,
        );
        let text = outcome.model_payload.output;
        assert!(text.contains("src/app.rs"), "src 内文件保留: {text}");
        assert!(
            !text.contains("src/vendor/lib.rs"),
            "glob 排除 vendor 深层: {text}"
        );
        assert!(!text.contains("tests/t.rs"), "组件匹配排除 tests: {text}");
    }
}
