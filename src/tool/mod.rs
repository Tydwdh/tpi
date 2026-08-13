//! 内置工具及其统一执行边界。
//!
//! 内建工具使用 `BuiltinTool` enum + `match` 静态分发；schema、参数类型、
//! 访问声明和执行函数放在同一工具模块。当前实现不创建动态 registry、
//! 插件 ABI 或 `dyn Tool` 层。
//!
//! `ask_user` 不是工具：需要用户决定时，模型直接输出问题并结束当前 run。

pub mod command;
pub mod edit;
pub mod files;
pub mod outcome;
pub mod plan;
pub mod registry;
pub mod search;
pub mod web;

use camino::Utf8PathBuf;
use outcome::{ModelPayload, ToolOutcome, ToolStatus};
use tokio_util::sync::CancellationToken;

use crate::provider::ToolDef;

/// 工具 schema 的 JSON 序列化（schemars 生成，理论上不会失败）。
/// 失败时记录日志并返回 null（模型将看到无参数 schema，但进程不崩溃）。
fn schema_value<T: schemars::JsonSchema>(tool: &'static str) -> serde_json::Value {
    match serde_json::to_value(schemars::schema_for!(T)) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(tool, error = %error, "tool schema 序列化失败；暴露空 schema");
            serde_json::Value::Null
        }
    }
}

/// 参数校验失败（§8.2 预检）：serde 错误 + 期望的参数 JSON shape。
///
/// 模型可见消息经 [`std::fmt::Display`] 渲染：先给 serde 具体错误，
/// 再给结构化 expected shape（字段名+类型），模型据此自纠错，不必重新猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsError {
    pub tool: &'static str,
    /// serde 解析/校验的底层错误详情（如 `missing field \`path\``）。
    pub detail: String,
    /// 期望的参数 shape（由 schemars schema 渲染，§5.2 同源）。
    pub expected_shape: String,
}

impl std::fmt::Display for ArgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid {} args: {}\n\nexpected shape:\n{}",
            self.tool, self.detail, self.expected_shape
        )
    }
}

/// 泛型解析入口：反序列化失败时，从同一 schemars schema 渲染期望 shape。
fn parse_args_typed<T>(
    tool: &'static str,
    arguments: &str,
    to_validated: impl FnOnce(T) -> ValidatedArgs,
) -> Result<ValidatedArgs, ArgsError>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema,
{
    serde_json::from_str::<T>(arguments)
        .map(to_validated)
        .map_err(|error| ArgsError {
            tool,
            detail: error.to_string(),
            expected_shape: render_expected_shape(&schema_value::<T>(tool)),
        })
}

/// 从 schemars root schema 渲染紧凑的期望 shape（`{ field: type, ... }`）。
fn render_expected_shape(schema: &serde_json::Value) -> String {
    let definitions = schema
        .get("definitions")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    render_schema_node(schema, &definitions)
}

/// 递归渲染 schema 节点；`$ref` 就地解析到 definitions，保持 shape 自包含。
fn render_schema_node(node: &serde_json::Value, definitions: &serde_json::Value) -> String {
    if let Some(reference) = node.get("$ref").and_then(|v| v.as_str())
        && let Some(name) = reference.strip_prefix("#/definitions/")
        && let Some(target) = definitions.get(name)
    {
        return render_schema_node(target, definitions);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(parts) = node.get(key).and_then(|v| v.as_array()) {
            return parts
                .iter()
                .map(|part| render_schema_node(part, definitions))
                .collect::<Vec<_>>()
                .join(" | ");
        }
    }
    if let Some(variants) = node.get("enum").and_then(|v| v.as_array()) {
        return variants
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    let type_name = |value: &serde_json::Value| {
        value
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("any")
            .to_string()
    };
    match type_name(node).as_str() {
        "object" => {
            let Some(properties) = node.get("properties").and_then(|p| p.as_object()) else {
                return "object".into();
            };
            let required: std::collections::BTreeSet<&str> = node
                .get("required")
                .and_then(|r| r.as_array())
                .map(|array| array.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let fields = properties.iter().map(|(name, property)| {
                let optional = if required.contains(name.as_str()) {
                    ""
                } else {
                    "?"
                };
                format!(
                    "{name}{optional}: {}",
                    render_schema_node(property, definitions)
                )
            });
            format!("{{ {} }}", fields.collect::<Vec<_>>().join(", "))
        }
        "array" => {
            let items = node
                .get("items")
                .map(|items| render_schema_node(items, definitions));
            match items {
                Some(items) => format!("[{items}]"),
                None => "[]".into(),
            }
        }
        other => other.to_string(),
    }
}

/// 内置工具集合（§8.1 静态分发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinTool {
    Read,
    List,
    Search,
    Glob,
    Edit,
    Write,
    Bash,
    UpdatePlan,
    WebSearch,
    WebFetch,
}

/// 工具对调度器和崩溃恢复真正重要的执行语义。
///
/// 这个分类刻意归属于 [`BuiltinTool`]：新增工具时，编译器会要求在
/// [`BuiltinTool::execution_class`] 中明确选择行为，不能由调度器的兜底分支
/// 静默假设为 `Pure`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolExecutionClass {
    Pure,
    FileReadExact,
    FileReadRecursive,
    FileWriteExact,
    WorkspaceUnknown,
}

/// session 中断后如何解释“缺少 ToolCompleted”的调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolRecoveryPolicy {
    /// 工具不会改变需要恢复的外部状态。
    NoEffect,
    /// edit/write 使用 commit metadata 判定已提交、未提交或未知。
    FileCommit,
    /// bash 等任意副作用工具无法可靠推断执行结果。
    Unknown,
}

impl ToolExecutionClass {
    pub(crate) fn requires_write_ahead(self) -> bool {
        matches!(self, Self::FileWriteExact | Self::WorkspaceUnknown)
    }

    pub(crate) fn recovery_policy(self) -> ToolRecoveryPolicy {
        match self {
            Self::Pure | Self::FileReadExact | Self::FileReadRecursive => {
                ToolRecoveryPolicy::NoEffect
            }
            Self::FileWriteExact => ToolRecoveryPolicy::FileCommit,
            Self::WorkspaceUnknown => ToolRecoveryPolicy::Unknown,
        }
    }
}

impl BuiltinTool {
    pub fn name(&self) -> &'static str {
        match self {
            BuiltinTool::Read => "read",
            BuiltinTool::List => "list",
            BuiltinTool::Search => "search",
            BuiltinTool::Glob => "glob",
            BuiltinTool::Edit => "edit",
            BuiltinTool::Write => "write",
            BuiltinTool::Bash => "bash",
            BuiltinTool::UpdatePlan => "update_plan",
            BuiltinTool::WebSearch => "web_search",
            BuiltinTool::WebFetch => "web_fetch",
        }
    }

    /// 持久化/provider 名称到内建工具的唯一反解析入口。
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "list" => Some(Self::List),
            "search" => Some(Self::Search),
            "glob" => Some(Self::Glob),
            "edit" => Some(Self::Edit),
            "write" => Some(Self::Write),
            "bash" => Some(Self::Bash),
            "update_plan" => Some(Self::UpdatePlan),
            "web_search" => Some(Self::WebSearch),
            "web_fetch" => Some(Self::WebFetch),
            _ => None,
        }
    }

    /// 调度、write-ahead 与恢复共同使用的行为事实源。
    pub(crate) fn execution_class(self) -> ToolExecutionClass {
        match self {
            Self::Read => ToolExecutionClass::FileReadExact,
            Self::List | Self::Search | Self::Glob => ToolExecutionClass::FileReadRecursive,
            Self::Edit | Self::Write => ToolExecutionClass::FileWriteExact,
            Self::Bash => ToolExecutionClass::WorkspaceUnknown,
            Self::UpdatePlan | Self::WebSearch | Self::WebFetch => ToolExecutionClass::Pure,
        }
    }

    pub(crate) fn requires_write_ahead(self) -> bool {
        self.execution_class().requires_write_ahead()
    }

    pub(crate) fn recovery_policy(self) -> ToolRecoveryPolicy {
        self.execution_class().recovery_policy()
    }

    pub fn description(&self) -> &'static str {
        match self {
            BuiltinTool::Read => {
                "Read the contents of a file (or an @artifact/... reference) as text. \
Output: `[revision=HASH]` header (pass to edit as revision) + `lines: X-Y of N` range + \
numbered lines `N: text` (precise single-line references). Truncated at 200 lines or 32KiB; \
use start_line/line_count to page, and follow the returned 续读 hint. \
Use to inspect files before editing; no cost beyond local I/O. \
Example: read src/main.rs"
            }
            BuiltinTool::List => {
                "List files and directories under a path (bounded: 200 items, depth 2). \
Follows .gitignore, skips symlinks, binaries and files over 2MiB. \
Output: items with relative paths + report (scanned_files/scanned_bytes/elapsed_ms/stop_reason); \
use returned cursor for the next page without rescanning. \
Use for a directory overview before searching/editing. \
Example: list path=src depth=2"
            }
            BuiltinTool::Search => {
                "Search file contents with a rust regex (ripgrep kernel; 100 matches max, \
one line 300 chars). Follows .gitignore. \
Output: matched lines with file paths; cursor pages without rescanning. \
Path may be a directory (recursive) or a single file. \
Use max_results to bound hits, include to filter by glob (e.g. `**/*.rs`), \
exclude to skip path components (e.g. \"tests\", \"vendor\"). \
Use when you know the text pattern but not which file. \
Example: search pattern=\"fn estimate_request\" include=[\"**/*.rs\"]"
            }
            BuiltinTool::Glob => {
                "Find files by filename glob pattern (e.g. `**/*.rs`, `src/**/*.ts`, `Cargo.toml`). \
Follows .gitignore, skips symlinks; results sorted by modification time (newest first). \
Use when you know the filename/path shape but not the location; prefer over search \
when the match is on file names, not contents. \
Example: glob pattern=\"**/*test*.rs\""
            }
            BuiltinTool::Edit => {
                "Atomically edit one file using revision-bound exact text replacement. \
Only the explicit old_text is replaced; adjacent content is never implicitly deleted; \
the whole batch applies or nothing does. \
You must pass the file's current revision (from read output) — stale revisions are rejected. \
Output: unified diff + applied count + previous/current revision. \
Prefer edit over write for localized changes (smaller, safer diffs). \
Example: edit path=src/main.rs revision=b3:<hex> replacements=[{old_text, new_text}]"
            }
            BuiltinTool::Write => {
                "Write an entire file (creates if missing; if it exists, `revision` must match \
the current one — use edit for localized changes). \
Output: unified diff + applied count. \
Use for new files or full rewrites. \
Example: write path=README.md content=... revision=..."
            }
            BuiltinTool::Bash => {
                "Run a command through Git Bash (`set -o pipefail` enabled; stderr is not \
failure — check status/exit_code). This is the only execution tool: use it for programs, \
builds, tests, git, pipelines, redirection and compound commands. \
Output: status/program/exit_code + bounded stdout/stderr tail; the full output is saved as \
an @artifact/... reference (read it for the complete result). \
Logical shell state persists across calls: `cd` changes the session cwd; `export`/`unset` \
change the session environment (both auto-inherited by later calls). The optional `cwd` field, \
if given, overrides the working directory for this call only (it does not change the session \
cwd unless the command itself runs `cd`). \
Cost: real execution time, capped by timeout_ms (default 120s, max 24h). \
Example: bash command=\"cargo test\" timeout_ms=180000"
            }
            BuiltinTool::UpdatePlan => {
                "Update the plan with a complete explicit snapshot (max 7 total items). Every item \
MUST be an object {\"text\": ..., \"status\": \"pending|in_progress|completed|cancelled|blocked\"}; \
never infer status from omitted items. Each call replaces the whole plan, so include every \
item you want to retain; an empty list clears it. At most one item may be in_progress. \
Simple tasks do not need a plan. Once a plan exists, update it promptly after EVERY completed \
step or direction change — do not batch updates until the end. Mark items completed one by \
one as each is actually done; never mark all remaining items completed at once — a plan with \
no pending items is finished and stops being injected. If you need a user decision or \
an external condition, mark the relevant item blocked before asking. \
Example: update_plan items=[{\"text\": \"inspect build\", \"status\": \"completed\"}, \
{\"text\": \"fix error\", \"status\": \"in_progress\"}, {\"text\": \"run tests\", \"status\": \"pending\"}]"
            }
            BuiltinTool::WebSearch => {
                "Search the web (DuckDuckGo HTML endpoint, free, no API key) to discover sources. \
Output: <external_content source=\"web_search\"> wrapping numbered hits (title, url, snippet). \
Results are for discovery only — never opens a browser, never calls a summary model; \
verify claims by fetching the source. \
Use to find sources; don't cite snippets as final evidence. \
Cost: network request, ~1-5s. \
Example: web_search query=\"rust tokio select\" count=5"
            }
            BuiltinTool::WebFetch => {
                "Fetch a URL and convert HTML to plain text (bounded to 48KiB body). \
Output: <external_content source=\"URL\"> wrapping final url/status/content_type/title + \
body; the full body is saved as an @artifact/... reference. \
Rejects loopback/private/link-local targets (SSRF guard). \
IMPORTANT: the `url` MUST come from a web_search result (or the user's explicit link) — \
never guess or invent URLs; hallucinated URLs 404 or point at the wrong page. \
Use to read the actual source found by web_search. \
Cost: network round-trip, ~1-15s depending on site. \
Example: web_fetch url=\"https://docs.rs/tokio\""
            }
        }
    }

    /// 工具 schema（§5.2 schemars：从参数类型生成，减少描述与实现漂移）。
    pub fn schema(&self) -> ToolDef {
        let parameters = match self {
            BuiltinTool::Read => schema_value::<files::ReadArgs>("read"),
            BuiltinTool::List => schema_value::<search::ListArgs>("list"),
            BuiltinTool::Search => schema_value::<search::SearchArgs>("search"),
            BuiltinTool::Glob => schema_value::<search::GlobArgs>("glob"),
            BuiltinTool::Edit => schema_value::<edit::EditArgs>("edit"),
            BuiltinTool::Write => schema_value::<files::WriteArgs>("write"),
            BuiltinTool::Bash => schema_value::<command::BashArgs>("bash"),
            BuiltinTool::UpdatePlan => schema_value::<plan::UpdatePlanArgs>("update_plan"),
            BuiltinTool::WebSearch => schema_value::<web::WebSearchArgs>("web_search"),
            BuiltinTool::WebFetch => schema_value::<web::WebFetchArgs>("web_fetch"),
        };
        ToolDef {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters,
        }
    }

    /// 校验并解析参数（§8.2：schema 校验是预检的一部分）。
    pub fn parse_args(&self, arguments: &str) -> Result<ValidatedArgs, ArgsError> {
        match self {
            BuiltinTool::Read => parse_args_typed("read", arguments, ValidatedArgs::Read),
            BuiltinTool::List => parse_args_typed("list", arguments, ValidatedArgs::List),
            BuiltinTool::Search => parse_args_typed("search", arguments, ValidatedArgs::Search),
            BuiltinTool::Glob => parse_args_typed("glob", arguments, ValidatedArgs::Glob),
            BuiltinTool::Edit => parse_args_typed("edit", arguments, ValidatedArgs::Edit),
            BuiltinTool::Write => parse_args_typed("write", arguments, ValidatedArgs::Write),
            BuiltinTool::Bash => parse_args_typed("bash", arguments, ValidatedArgs::Bash),
            BuiltinTool::UpdatePlan => {
                parse_args_typed("update_plan", arguments, ValidatedArgs::UpdatePlan)
            }
            BuiltinTool::WebSearch => {
                parse_args_typed("web_search", arguments, ValidatedArgs::WebSearch)
            }
            BuiltinTool::WebFetch => {
                parse_args_typed("web_fetch", arguments, ValidatedArgs::WebFetch)
            }
        }
    }
}

/// 校验通过的参数（§8.2 `PreparedToolCall.validated_args` 的 M1 形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedArgs {
    Read(files::ReadArgs),
    List(search::ListArgs),
    Search(search::SearchArgs),
    Glob(search::GlobArgs),
    Edit(edit::EditArgs),
    Write(files::WriteArgs),
    Bash(command::BashArgs),
    UpdatePlan(plan::UpdatePlanArgs),
    WebSearch(web::WebSearchArgs),
    WebFetch(web::WebFetchArgs),
}

impl ValidatedArgs {
    /// 参数由哪个工具产生。调度边界用它验证 `(BuiltinTool, ValidatedArgs)`
    /// 没有被错误拼接；不变量破坏时必须保守隔离而不是提升为 Pure。
    pub(crate) fn tool(&self) -> BuiltinTool {
        match self {
            Self::Read(_) => BuiltinTool::Read,
            Self::List(_) => BuiltinTool::List,
            Self::Search(_) => BuiltinTool::Search,
            Self::Glob(_) => BuiltinTool::Glob,
            Self::Edit(_) => BuiltinTool::Edit,
            Self::Write(_) => BuiltinTool::Write,
            Self::Bash(_) => BuiltinTool::Bash,
            Self::UpdatePlan(_) => BuiltinTool::UpdatePlan,
            Self::WebSearch(_) => BuiltinTool::WebSearch,
            Self::WebFetch(_) => BuiltinTool::WebFetch,
        }
    }

    /// 文件资源型工具的原始 path 参数；具体 Exact/Recursive 和 Read/Write
    /// 由 [`BuiltinTool::execution_class`] 决定。
    pub(crate) fn path(&self) -> Option<&str> {
        match self {
            Self::Read(args) => Some(&args.path),
            Self::List(args) => Some(&args.path),
            Self::Search(args) => Some(&args.path),
            Self::Glob(args) => Some(&args.path),
            Self::Edit(args) => Some(&args.path),
            Self::Write(args) => Some(&args.path),
            Self::Bash(_) | Self::UpdatePlan(_) | Self::WebSearch(_) | Self::WebFetch(_) => None,
        }
    }
}

/// 工具流式输出事件（bash 实时输出 → UI；tool 层不依赖 agent 层）。
///
/// BUG-012：此前 unbounded channel 会在 UI 消费慢时无限堆积；现在是有界
/// channel + try_send，满时丢弃（实时输出是 lossy telemetry，§12：不允许为
/// UI streaming 建无限队列；工具自身输出仍有界：24 KiB tail + artifact）。
pub struct ToolStreamEvent {
    /// 工具调用的内部 id（与 ToolStarted 事件一致，UI 按此匹配卡片）。
    pub call_id: crate::ids::ToolCallId,
    /// 流标识（STREAM_STDOUT / STREAM_STDERR）。
    pub stream: u8,
    pub text: String,
}

/// 工具流式输出通道容量（有界；满时丢弃新帧，见 [`ToolStreamEvent`]）。
pub const TOOL_STREAM_CAPACITY: usize = 256;
/// 工具执行上下文。
#[derive(Clone)]
pub struct ToolContext {
    pub workspace_root: Utf8PathBuf,
    /// §9.1 自由模式：允许访问 workspace 外的绝对路径（默认 true；false 恢复严格沙箱）。
    pub allow_outside_workspace: bool,
    pub cancel: CancellationToken,
    /// artifact 根目录（§14.1：`~/.tpi/artifacts`）。
    pub artifacts_root: std::path::PathBuf,
    /// 当前 session id（artifact 引用与 cursor 作用域）。
    pub session_id: String,
    /// 当前工具调用的内部 id（流式输出事件按此匹配 UI 卡片）。
    pub call_id: crate::ids::ToolCallId,
    /// 流式输出通道（bash 实时输出；None = 无 UI 订阅）。
    pub output_tx: Option<tokio::sync::mpsc::Sender<ToolStreamEvent>>,
    /// list/search 分页 snapshot（session 作用域；cursor 翻页不重新扫描，§8.4）。
    pub scan_snapshots:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, search::ScanSnapshot>>>,
    /// Git Bash 路径解析（§11.2；None = 自动探测）。
    pub shell_path: Option<Utf8PathBuf>,
    /// session-local bounded SnapshotStore（§10.1）。
    pub snapshot_store: std::sync::Arc<std::sync::Mutex<crate::tool::edit::SnapshotStore>>,
    /// 当前原子短计划（§13；agent loop 持有，update_plan 原子替换）。
    pub current_plan: std::sync::Arc<std::sync::Mutex<Option<crate::tool::plan::Plan>>>,
    /// Logical Shell Session（任务书 §S1：属于 Workspace；bash 工具读写）。
    /// 与 `workspace` 内的 shell 是同一状态源（构造时共享 Arc）。
    pub shell: std::sync::Arc<std::sync::Mutex<crate::shell::ShellSessionState>>,
    /// ActiveWorkspace（§26-§30：Tool Protocol 的分发目标）。
    /// `workspace_root`/`allow_outside_workspace`/`shell` 是当前 workspace 的投影。
    pub workspace: std::sync::Arc<std::sync::Mutex<crate::workspace::ActiveWorkspace>>,
    /// 交互模式（`-p` 为 false；§11 移除 ask_user 后仅保留供未来交互原语使用）。
    pub interactive: bool,
}

/// 路径解析失败（§9.1：禁止 workspace 外访问）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathResolveError {
    Empty,
    Invalid,
    OutsideWorkspace,
}

impl std::fmt::Display for PathResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty path"),
            Self::Invalid => write!(f, "invalid path"),
            Self::OutsideWorkspace => write!(f, "path outside workspace"),
        }
    }
}

/// 词法规范化路径（解析 `.` / `..`，不访问文件系统）。
fn normalize_lexical(path: &std::path::Path) -> Result<std::path::PathBuf, PathResolveError> {
    use std::path::Component;
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                normalized.push(prefix.as_os_str());
            }
            Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(PathResolveError::OutsideWorkspace);
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

/// 把模型给的路径解析为 workspace 内路径（§9.1：相对路径优先；绝对路径必须在 root 内）。
pub fn resolve_workspace_path(
    workspace_root: &Utf8PathBuf,
    path: &str,
) -> Result<Utf8PathBuf, PathResolveError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(PathResolveError::Empty);
    }
    if trimmed.contains('\0') {
        return Err(PathResolveError::Invalid);
    }

    let candidate = Utf8PathBuf::from(trimmed);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    };

    let normalized = normalize_lexical(joined.as_std_path())?;
    let workspace_norm = normalize_lexical(workspace_root.as_std_path())?;
    if !normalized.starts_with(&workspace_norm) {
        return Err(PathResolveError::OutsideWorkspace);
    }

    // 已存在路径：canonicalize 防止 symlink 逃逸；
    // 不存在路径（写新文件）：canonicalize 最近存在的祖先，防止经 junction/symlink 写穿
    //（§9.1：例如 `write workspace/link/new.txt` 借 workspace 内 junction 逃逸到外部）。
    let ws_canonical = std::fs::canonicalize(workspace_root.as_std_path())
        .unwrap_or_else(|_| workspace_norm.clone());
    if let Some(canonical) = canonical_ancestor(&normalized)
        && !canonical.starts_with(&ws_canonical)
    {
        return Err(PathResolveError::OutsideWorkspace);
    }

    Utf8PathBuf::from_path_buf(normalized).map_err(|_| PathResolveError::Invalid)
}

/// 工具路径解析（§9.1 自由模式）：按 `ctx.allow_outside_workspace` 决定是否
/// 允许 workspace 外的绝对路径。
///
/// - 自由模式（默认 true）：只做词法规范化（拒绝空/NUL），绝对路径可指向任意位置——
///   与 bash 的自由保持一致（bash 本来就能访问任意路径）；
/// - 严格模式（false）：走 [`resolve_workspace_path`]（含 junction/symlink 写穿检查）。
pub fn resolve_tool_path(ctx: &ToolContext, path: &str) -> Result<Utf8PathBuf, PathResolveError> {
    resolve_write_path(&ctx.workspace_root, path, ctx.allow_outside_workspace)
}

/// 写工具提交计划/恢复元数据的路径解析（§9.1 自由模式）：
/// `allow_outside_workspace=true` → 词法规范化（绝对路径可用，与 bash 一致）；
/// `false` → 严格 workspace 沙箱（含 junction/symlink 写穿检查）。
/// agent 层 plan/recovery 与 tool 层执行必须使用同一解析，否则自由模式下
/// 写 workspace 外文件会在提交计划阶段被误拒（missing_commit_plan）。
pub fn resolve_write_path(
    workspace_root: &Utf8PathBuf,
    path: &str,
    allow_outside_workspace: bool,
) -> Result<Utf8PathBuf, PathResolveError> {
    if !allow_outside_workspace {
        return resolve_workspace_path(workspace_root, path);
    }
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(PathResolveError::Empty);
    }
    if trimmed.contains('\0') {
        return Err(PathResolveError::Invalid);
    }
    let candidate = Utf8PathBuf::from(trimmed);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    };
    let normalized = normalize_lexical(joined.as_std_path())?;
    Utf8PathBuf::from_path_buf(normalized).map_err(|_| PathResolveError::Invalid)
}

/// 调度锁的路径标识（与工具实际解析一致，保证等价写法映射到同一锁）。
///
/// 严格模式：`resolve_workspace_path`（外部路径本就不会真正执行）；
/// 自由模式：对路径做词法规范化，外部绝对路径的等价写法（`.\`、`..`、大小写之外的
/// 词法差异）收敛为同一标识——否则同一外部文件可能被两个等价路径并行写入（竞态）。
pub fn resolve_lock_path(
    workspace_root: &Utf8PathBuf,
    path: &str,
    allow_outside_workspace: bool,
) -> Utf8PathBuf {
    if !allow_outside_workspace {
        return resolve_workspace_path(workspace_root, path)
            .unwrap_or_else(|_| Utf8PathBuf::from(path.trim()));
    }
    let trimmed = path.trim();
    let candidate = Utf8PathBuf::from(trimmed);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    };
    normalize_lexical(joined.as_std_path())
        .ok()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
        .unwrap_or(joined)
}
/// 对路径本身做 canonicalize；失败时逐级向上解析最近存在的祖先。
///
/// 目标尚不存在时（create 场景）`canonicalize` 返回 Err；此时必须解析其父链上
/// 最近存在的目录（可能是 junction/symlink），否则写穿检查被静默跳过。
fn canonical_ancestor(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(canonical) => return Some(canonical),
            Err(_) => {
                if !current.pop() {
                    return None;
                }
            }
        }
    }
}

/// artifact 引用组件校验（禁止路径分隔符与 `..`）。
pub fn validate_artifact_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 路径被拒绝时的标准 tool outcome。
pub fn path_rejected_outcome(tool: &str, error: PathResolveError) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        outcome::ModelPayload {
            status: outcome::ToolStatus::Rejected,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!(
                "status: rejected\ntool: {tool}\nerror: {error}\n\n路径必须位于 workspace 内。"
            ),
            effect: None,
            artifact: None,
        },
    )
}

/// 执行一个已校验的工具调用（§8.2：预期失败返回 ToolOutcome，不返回 Err）。
///
/// `plan` 是写工具（edit/write）的提交计划：agent loop 在持久化 `ToolStarted`
pub async fn execute(
    tool: BuiltinTool,
    args: ValidatedArgs,
    ctx: &ToolContext,
    plan: Option<&crate::tool::edit::CommitPlan>,
) -> ToolOutcome {
    let start = std::time::Instant::now();
    let outcome = match (tool, args) {
        // 异步工具（网络/进程）留在 async 上下文。
        (BuiltinTool::Bash, ValidatedArgs::Bash(args)) => command::bash(args, ctx).await,
        (BuiltinTool::WebSearch, ValidatedArgs::WebSearch(args)) => {
            web::web_search(args, ctx).await
        }
        (BuiltinTool::WebFetch, ValidatedArgs::WebFetch(args)) => web::web_fetch(args, ctx).await,
        // §11/BUG-009：同步工具（文件 IO/正则扫描）挪到 blocking 池，避免大目录扫描、
        // 大文件读取、网络盘遍历阻塞 Tokio worker——并行 4 个工具时可能占满全部
        // worker，导致 TUI 事件循环（ui_rx/ticker/键盘）明显卡顿。
        (tool, args) => {
            let ctx = ctx.clone();
            let plan = plan.cloned();
            tokio::task::spawn_blocking(move || match (tool, args) {
                (BuiltinTool::Read, ValidatedArgs::Read(args)) => files::read(args, &ctx),
                (BuiltinTool::List, ValidatedArgs::List(args)) => search::list(args, &ctx),
                (BuiltinTool::Search, ValidatedArgs::Search(args)) => search::search(args, &ctx),
                (BuiltinTool::Glob, ValidatedArgs::Glob(args)) => search::glob(args, &ctx),
                (BuiltinTool::Edit, ValidatedArgs::Edit(args)) => files::edit(args, &ctx, plan.as_ref()),
                (BuiltinTool::Write, ValidatedArgs::Write(args)) => files::write(args, &ctx, plan.as_ref()),
                // §13：update_plan 是原生同步控制操作。
                (BuiltinTool::UpdatePlan, ValidatedArgs::UpdatePlan(args)) => plan::update_plan(args, &ctx),
                (tool, args) => {
                    // 内部不变量：ValidatedArgs 由同工具解析产生；异常组合按失败上报。
                    tracing::error!(
                        tool = ?tool,
                        args = ?args,
                        "execute: tool 与已解析参数不匹配（内部不变量破坏）",
                    );
                    ToolOutcome::failed(
                        "internal",
                        ModelPayload {
                            status: ToolStatus::Failed,
                            program: None,
                            exit_code: None,
                            duration_ms: 0,
                            output: "status: failed\\ntool: internal\\nerror: args_mismatch\\n\\n内部错误：工具与参数不匹配。".into(),
                            effect: None,
                            artifact: None,
                        },
                    )
                }
            })
            .await
            .unwrap_or_else(|error| {
                // spawn_blocking panic/取消：按基础设施失败上报，不崩溃进程。
                tracing::error!(tool = ?tool, error = %error, "execute: blocking tool failed");
                ToolOutcome::failed(
                    "internal",
                    ModelPayload {
                        status: ToolStatus::Failed,
                        program: None,
                        exit_code: None,
                        duration_ms: 0,
                        output: "status: failed\\ntool: internal\\nerror: tool_panicked\\n\\n内部错误：工具执行线程异常退出。".into(),
                        effect: None,
                        artifact: None,
                    },
                )
            })
        }
    };
    outcome.with_timing(start.elapsed().as_millis() as u64)
}

/// 当前实现的工具列表（§8.1：只暴露已实现工具）。
pub fn implemented_tools() -> Vec<BuiltinTool> {
    vec![
        BuiltinTool::Read,
        BuiltinTool::List,
        BuiltinTool::Search,
        BuiltinTool::Glob,
        BuiltinTool::Edit,
        BuiltinTool::Write,
        BuiltinTool::Bash,
        BuiltinTool::UpdatePlan,
        BuiltinTool::WebSearch,
        BuiltinTool::WebFetch,
    ]
}

/// update_plan 是否计入工具调用预算（§13：计入）。
pub const UPDATE_PLAN_COUNTS_TO_BUDGET: bool = true;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn implemented_tool_names_round_trip_to_the_same_behavior_owner() {
        let mut names = std::collections::BTreeSet::new();
        for tool in implemented_tools() {
            assert!(names.insert(tool.name()), "工具名必须唯一: {}", tool.name());
            assert_eq!(BuiltinTool::from_name(tool.name()), Some(tool));
            // 调用穷尽分类，确保测试覆盖当前暴露的每个工具。
            let _ = tool.execution_class();
        }
        assert_eq!(BuiltinTool::from_name("unknown_tool"), None);
    }

    #[test]
    fn write_ahead_and_recovery_policy_come_from_execution_class() {
        for tool in implemented_tools() {
            let class = tool.execution_class();
            assert_eq!(
                tool.requires_write_ahead(),
                matches!(
                    class,
                    ToolExecutionClass::FileWriteExact | ToolExecutionClass::WorkspaceUnknown
                )
            );
            assert_eq!(tool.recovery_policy(), class.recovery_policy());
        }
    }

    /// §4 ACI：工具 description 必须含典型调用示例（书中：示例提升工具准确率 72%→90%），
    /// 高频工具（bash/search/web_fetch/edit）必须含执行代价或边界说明。
    #[test]
    fn tool_descriptions_carry_examples_and_cost() {
        for tool in implemented_tools() {
            let desc = tool.description();
            assert!(
                desc.contains("Example:"),
                "{} description 必须含典型示例: {desc}",
                tool.name()
            );
        }
        // 高频工具必须有执行代价或边界（bash 代价 / web_fetch 代价+SSRF / search 何时用）。
        for (name, marker) in [
            ("bash", "timeout_ms"),
            ("web_fetch", "SSRF"),
            ("web_search", "discovery only"),
            ("search", "Use when"),
            ("edit", "stale revisions are rejected"),
        ] {
            let tool = BuiltinTool::from_name(name).unwrap();
            assert!(
                tool.description().contains(marker),
                "{} description 应含 {marker}",
                name
            );
        }
    }

    /// §4 ACI：参数 doc 注释必须注入 schema description（schemars 默认行为）——
    /// 否则模型看不到参数示例/语义。
    #[test]
    fn parameter_docs_reach_schema_descriptions() {
        let schema = BuiltinTool::Bash.schema();
        let params = schema.parameters.to_string();
        assert!(
            params.contains("cargo test"),
            "bash.command 示例必须进 schema: {params}"
        );
        assert!(
            params.contains("120000"),
            "bash.timeout_ms 默认值必须进 schema: {params}"
        );
        let search_schema = BuiltinTool::Search.schema();
        assert!(
            search_schema
                .parameters
                .to_string()
                .contains("fn estimate_request"),
            "search.pattern 示例必须进 schema"
        );
        let fetch_schema = BuiltinTool::WebFetch.schema();
        assert!(
            fetch_schema.parameters.to_string().contains("loopback"),
            "web_fetch.url 边界必须进 schema"
        );
    }

    /// BUG-009：同步工具（read）经 spawn_blocking 执行后仍返回正确结果。
    #[tokio::test]
    async fn execute_runs_sync_tools_through_blocking_pool() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.txt")).unwrap();
        std::fs::write(path.as_std_path(), "hello 世界\n").unwrap();
        let local = crate::workspace::LocalWorkspace::new(workspace.clone(), true);
        let ctx = ToolContext {
            workspace_root: workspace.clone(),
            shell: local.shell.clone(),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(local),
            )),
            cancel: CancellationToken::new(),
            artifacts_root: dir.path().join("artifacts"),
            session_id: "test-session".into(),
            call_id: crate::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            interactive: false,
            allow_outside_workspace: true,
        };
        let outcome = execute(
            BuiltinTool::Read,
            ValidatedArgs::Read(files::ReadArgs {
                path: path.to_string(),
                start_line: 1,
                line_count: 200,
            }),
            &ctx,
            None,
        )
        .await;
        assert_eq!(outcome.status, outcome::ToolStatus::Succeeded);
        assert!(outcome.model_text().contains("hello 世界"));
    }

    /// C2：parse_args 对非法参数返回结构化 expected shape（字段名+类型），
    /// 而不是只有 serde 的裸错误字符串——模型据此可自纠错。
    #[test]
    fn parse_args_invalid_reports_expected_shape() {
        let err = BuiltinTool::Edit.parse_args("{}").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("expected shape"),
            "错误必须说明期望 shape: {msg}"
        );
        assert!(msg.contains("path"), "expected shape 必须含 path: {msg}");
        assert!(
            msg.contains("revision"),
            "expected shape 必须含 revision: {msg}"
        );
        assert!(
            msg.contains("replacements"),
            "expected shape 必须含 replacements: {msg}"
        );

        let err = BuiltinTool::Write.parse_args("{}").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("expected shape"),
            "write: 错误必须说明期望 shape: {msg}"
        );
        assert!(
            msg.contains("content"),
            "write: expected shape 必须含 content: {msg}"
        );
    }
}
