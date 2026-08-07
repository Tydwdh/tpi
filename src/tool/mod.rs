//! 内置工具（文档 §8）。
//!
//! 内建工具使用 `BuiltinTool` enum + `match` 静态分发；schema、参数类型、
//! 访问声明和执行函数放在同一工具模块（§8.1）。v1 不创建动态 registry、
//! 插件 ABI 或 `dyn Tool` 层。
//!
//! M1 范围（§21 M1）：`read`、`edit`、`write`、`run`。
//! M2：`list`、`search`、`bash`。M4：`update_plan`、`ask_user`。M6：`web_search`、`web_fetch`。

pub mod command;
pub mod edit;
pub mod files;
pub mod outcome;
pub mod plan;
pub mod search;
pub mod web;

use camino::Utf8PathBuf;
use outcome::ToolOutcome;
use tokio_util::sync::CancellationToken;

use crate::provider::ToolDef;

/// 内置工具集合（§8.1 静态分发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinTool {
    Read,
    List,
    Search,
    Edit,
    Write,
    Run,
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
            BuiltinTool::Run => "run",
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
            BuiltinTool::Run => {
                "Run a program directly with explicit arguments (no shell). Returns real status, exit_code and duration visible to the model. Prefer for builds, tests, git and ordinary programs; use bash only for pipelines/redirection."
            }
            BuiltinTool::Bash => {
                "Run a Bash command through Git Bash with `set -o pipefail` enabled (pipeline failures are visible). Only for pipelines, redirection, globs or compound commands; ordinary programs use run."
            }
            BuiltinTool::UpdatePlan => {
                "Replace the whole short plan atomically (max 7 unique items). Only for complex multi-step tasks; simple tasks do not need a plan. It is a progress state, not an extra workflow."
            }
            BuiltinTool::WebSearch => {
                "Search the web (Brave) to discover sources. Returns title, URL, snippet and age. Results are for discovery only; never opens a browser and never calls a summary model."
            }
            BuiltinTool::WebFetch => {
                "Fetch a URL and convert HTML to plain text (bounded body). Returns final URL, status, content type, title and body; full body is stored as artifact."
            }
        }
    }

    /// 工具 schema（§5.2 schemars：从参数类型生成，减少描述与实现漂移）。
    pub fn schema(&self) -> ToolDef {
        let parameters = match self {
            BuiltinTool::Read => {
                serde_json::to_value(schemars::schema_for!(files::ReadArgs)).unwrap()
            }
            BuiltinTool::List => {
                serde_json::to_value(schemars::schema_for!(search::ListArgs)).unwrap()
            }
            BuiltinTool::Search => {
                serde_json::to_value(schemars::schema_for!(search::SearchArgs)).unwrap()
            }
            BuiltinTool::Edit => {
                serde_json::to_value(schemars::schema_for!(edit::EditArgs)).unwrap()
            }
            BuiltinTool::Write => {
                serde_json::to_value(schemars::schema_for!(files::WriteArgs)).unwrap()
            }
            BuiltinTool::Run => {
                serde_json::to_value(schemars::schema_for!(command::RunArgs)).unwrap()
            }
            BuiltinTool::Bash => {
                serde_json::to_value(schemars::schema_for!(command::BashArgs)).unwrap()
            }
            BuiltinTool::UpdatePlan => {
                serde_json::to_value(schemars::schema_for!(plan::UpdatePlanArgs)).unwrap()
            }
            BuiltinTool::WebSearch => {
                serde_json::to_value(schemars::schema_for!(web::WebSearchArgs)).unwrap()
            }
            BuiltinTool::WebFetch => {
                serde_json::to_value(schemars::schema_for!(web::WebFetchArgs)).unwrap()
            }
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
            BuiltinTool::Run => serde_json::from_str::<command::RunArgs>(arguments)
                .map(ValidatedArgs::Run)
                .map_err(|e| format!("invalid run args: {e}")),
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
    Run(command::RunArgs),
    Bash(command::BashArgs),
    UpdatePlan(plan::UpdatePlanArgs),
    WebSearch(web::WebSearchArgs),
    WebFetch(web::WebFetchArgs),
}

/// 工具执行上下文。
pub struct ToolContext {
    pub workspace_root: Utf8PathBuf,
    pub cancel: CancellationToken,
    /// artifact 根目录（§14.1：`~/.tpi/artifacts`）。
    pub artifacts_root: std::path::PathBuf,
    /// 当前 session id（artifact 引用与 cursor 作用域）。
    pub session_id: String,
    /// list/search 分页 snapshot（session 作用域；cursor 翻页不重新扫描，§8.4）。
    pub scan_snapshots:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, search::ScanSnapshot>>>,
    /// Git Bash 路径解析（§11.2；None = 自动探测）。
    pub shell_path: Option<Utf8PathBuf>,
    /// session-local bounded SnapshotStore（§10.1）。
    pub snapshot_store: std::sync::Arc<std::sync::Mutex<crate::tool::edit::SnapshotStore>>,
    /// 当前原子短计划（§13；agent loop 持有，update_plan 原子替换）。
    pub current_plan: std::sync::Arc<std::sync::Mutex<Option<crate::tool::plan::Plan>>>,
    /// Brave API key 的环境变量名（§17；未配置时 web_search 明确 unavailable）。
    pub web_brave_key_env: String,
}

/// 把模型给的路径解析为 workspace 内路径（§9.1：优先相对路径；绝对路径映射到 root）。
pub fn resolve_workspace_path(workspace_root: &Utf8PathBuf, path: &str) -> Utf8PathBuf {
    let candidate = Utf8PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    }
}

/// 执行一个已校验的工具调用（§8.2：预期失败返回 ToolOutcome，不返回 Err）。
///
/// `plan` 是写工具（edit/write）的提交计划：agent loop 在持久化 `ToolStarted`
/// 前生成（§10.7 第 1 条：temp/backup 标识先于副作用）。
pub async fn execute(
    tool: BuiltinTool,
    args: ValidatedArgs,
    ctx: &ToolContext,
    plan: Option<&crate::tool::edit::CommitPlan>,
) -> ToolOutcome {
    let start = std::time::Instant::now();
    let outcome = match (tool, args) {
        (BuiltinTool::Read, ValidatedArgs::Read(args)) => files::read(args, ctx),
        (BuiltinTool::List, ValidatedArgs::List(args)) => search::list(args, ctx),
        (BuiltinTool::Search, ValidatedArgs::Search(args)) => search::search(args, ctx),
        (BuiltinTool::Edit, ValidatedArgs::Edit(args)) => files::edit(args, ctx, plan),
        (BuiltinTool::Write, ValidatedArgs::Write(args)) => files::write(args, ctx, plan),
        (BuiltinTool::Run, ValidatedArgs::Run(args)) => command::run(args, ctx).await,
        (BuiltinTool::Bash, ValidatedArgs::Bash(args)) => command::bash(args, ctx).await,
        // §13：update_plan 是原生同步控制操作，不进入普通调度队列。
        (BuiltinTool::UpdatePlan, ValidatedArgs::UpdatePlan(args)) => plan::update_plan(args, ctx),
        (BuiltinTool::WebSearch, ValidatedArgs::WebSearch(args)) => {
            web::web_search(args, ctx).await
        }
        (BuiltinTool::WebFetch, ValidatedArgs::WebFetch(args)) => web::web_fetch(args, ctx).await,
        _ => unreachable!("validated args must match the tool"),
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
        BuiltinTool::Run,
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
        BuiltinTool::Edit | BuiltinTool::Write | BuiltinTool::Run | BuiltinTool::Bash
    )
}

/// update_plan 是否计入工具调用预算（§13：计入）。
pub const UPDATE_PLAN_COUNTS_TO_BUDGET: bool = true;
