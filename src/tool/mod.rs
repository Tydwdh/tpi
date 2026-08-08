//! 内置工具（文档 §8）。
//!
//! 内建工具使用 `BuiltinTool` enum + `match` 静态分发；schema、参数类型、
//! 访问声明和执行函数放在同一工具模块（§8.1）。v1 不创建动态 registry、
//! 插件 ABI 或 `dyn Tool` 层。
//!
//! M1 范围（§21 M1）：`read`、`edit`、`write`。
//! M2：`list`、`search`、`bash`。M4：`update_plan`。M6：`web_search`、`web_fetch`。
//! §11（稳定化任务书）：ask_user 已移除——需要用户决定时模型直接输出问题并结束 run。

pub mod command;
pub mod edit;
pub mod files;
pub mod outcome;
pub mod plan;
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

/// 内置工具集合（§8.1 静态分发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinTool {
    Read,
    List,
    Search,
    Edit,
    Write,
    Bash,
    UpdatePlan,
    WebSearch,
    WebFetch,
}

impl BuiltinTool {
    pub fn name(&self) -> &'static str {
        match self {
            BuiltinTool::Read => "read",
            BuiltinTool::List => "list",
            BuiltinTool::Search => "search",
            BuiltinTool::Edit => "edit",
            BuiltinTool::Write => "write",
            BuiltinTool::Bash => "bash",
            BuiltinTool::UpdatePlan => "update_plan",
            BuiltinTool::WebSearch => "web_search",
            BuiltinTool::WebFetch => "web_fetch",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            BuiltinTool::Read => {
                "Read the contents of a file (or an @artifact/... reference). Text output begins with [revision=HASH], which can be passed to edit as revision. Text is truncated to 200 lines or 32KiB (whichever is hit first). Use start_line/line_count for large files."
            }
            BuiltinTool::List => {
                "List files and directories under a path with bounded output (200 items, depth 2). Follows .gitignore, skips symlinks, binaries and files over 2MiB. Use cursor for the next page; report includes scanned_files/scanned_bytes/elapsed_ms/stop_reason."
            }
            BuiltinTool::Search => {
                "Search file contents with a regex (100 matches max, one line 300 chars). Follows .gitignore. Use cursor for the next page without rescanning."
            }
            BuiltinTool::Edit => {
                "Atomically edit one file using revision-bound exact text replacement. Only the explicit old_text is replaced; adjacent content is never implicitly deleted. All replacements are validated before the file is written; the whole batch applies or nothing does."
            }
            BuiltinTool::Write => {
                "Create a new file with the given content. Fails with already_exists if the target already exists; existing files must be modified through edit."
            }
            BuiltinTool::Bash => {
                "Run a command through Git Bash with `set -o pipefail` enabled (pipeline failures are visible). This is the only execution tool: use it for programs, builds, tests, git, pipelines, redirection, globs and compound commands. Write Bash syntax; the host is Windows, never mix PowerShell syntax."
            }
            BuiltinTool::UpdatePlan => {
                "Replace the whole short plan atomically (max 7 unique items). Only for complex multi-step tasks; simple tasks do not need a plan. It is a progress state, not an extra workflow."
            }
            BuiltinTool::WebSearch => {
                "Search the web (DuckDuckGo, free, no API key) to discover sources. Returns title, URL and snippet. Results are for discovery only; never opens a browser and never calls a summary model."
            }
            BuiltinTool::WebFetch => {
                "Fetch a URL and convert HTML to plain text (bounded body). Returns final URL, status, content type, title and body; full body is stored as artifact."
            }
        }
    }

    /// 工具 schema（§5.2 schemars：从参数类型生成，减少描述与实现漂移）。
    pub fn schema(&self) -> ToolDef {
        let parameters = match self {
            BuiltinTool::Read => schema_value::<files::ReadArgs>("read"),
            BuiltinTool::List => schema_value::<search::ListArgs>("list"),
            BuiltinTool::Search => schema_value::<search::SearchArgs>("search"),
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
    pub fn parse_args(&self, arguments: &str) -> Result<ValidatedArgs, String> {
        match self {
            BuiltinTool::Read => serde_json::from_str::<files::ReadArgs>(arguments)
                .map(ValidatedArgs::Read)
                .map_err(|e| format!("invalid read args: {e}")),
            BuiltinTool::List => serde_json::from_str::<search::ListArgs>(arguments)
                .map(ValidatedArgs::List)
                .map_err(|e| format!("invalid list args: {e}")),
            BuiltinTool::Search => serde_json::from_str::<search::SearchArgs>(arguments)
                .map(ValidatedArgs::Search)
                .map_err(|e| format!("invalid search args: {e}")),
            BuiltinTool::Edit => serde_json::from_str::<edit::EditArgs>(arguments)
                .map(ValidatedArgs::Edit)
                .map_err(|e| format!("invalid edit args: {e}")),
            BuiltinTool::Write => serde_json::from_str::<files::WriteArgs>(arguments)
                .map(ValidatedArgs::Write)
                .map_err(|e| format!("invalid write args: {e}")),
            BuiltinTool::Bash => serde_json::from_str::<command::BashArgs>(arguments)
                .map(ValidatedArgs::Bash)
                .map_err(|e| format!("invalid bash args: {e}")),
            BuiltinTool::UpdatePlan => serde_json::from_str::<plan::UpdatePlanArgs>(arguments)
                .map(ValidatedArgs::UpdatePlan)
                .map_err(|e| format!("invalid update_plan args: {e}")),
            BuiltinTool::WebSearch => serde_json::from_str::<web::WebSearchArgs>(arguments)
                .map(ValidatedArgs::WebSearch)
                .map_err(|e| format!("invalid web_search args: {e}")),
            BuiltinTool::WebFetch => serde_json::from_str::<web::WebFetchArgs>(arguments)
                .map(ValidatedArgs::WebFetch)
                .map_err(|e| format!("invalid web_fetch args: {e}")),
        }
    }
}

/// 校验通过的参数（§8.2 `PreparedToolCall.validated_args` 的 M1 形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedArgs {
    Read(files::ReadArgs),
    List(search::ListArgs),
    Search(search::SearchArgs),
    Edit(edit::EditArgs),
    Write(files::WriteArgs),
    Bash(command::BashArgs),
    UpdatePlan(plan::UpdatePlanArgs),
    WebSearch(web::WebSearchArgs),
    WebFetch(web::WebFetchArgs),
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
        BuiltinTool::Edit,
        BuiltinTool::Write,
        BuiltinTool::Bash,
        BuiltinTool::UpdatePlan,
        BuiltinTool::WebSearch,
        BuiltinTool::WebFetch,
    ]
}

/// 工具是否需要在副作用前 write-ahead（§12.1：Write / WorkspaceUnknown）。
pub fn requires_write_ahead(tool: BuiltinTool) -> bool {
    matches!(
        tool,
        BuiltinTool::Edit | BuiltinTool::Write | BuiltinTool::Bash
    )
}

/// update_plan 是否计入工具调用预算（§13：计入）。
pub const UPDATE_PLAN_COUNTS_TO_BUDGET: bool = true;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    /// BUG-009：同步工具（read）经 spawn_blocking 执行后仍返回正确结果。
    #[tokio::test]
    async fn execute_runs_sync_tools_through_blocking_pool() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.txt")).unwrap();
        std::fs::write(path.as_std_path(), "hello 世界\n").unwrap();
        let ctx = ToolContext {
            workspace_root: workspace,
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
}
