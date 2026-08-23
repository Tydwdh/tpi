//! 内置工具及其统一执行边界。
//!
//! 内建工具使用 `BuiltinTool` enum + `match` 静态分发；schema、参数类型、
//! 访问声明和执行函数放在同一工具模块。当前实现不创建动态 registry、
//! 插件 ABI 或 `dyn Tool` 层。
//!
//! `ask_user` 不是工具：需要用户决定时，模型直接输出问题并结束当前 run。

pub mod builtins;
pub mod command;
pub mod edit;
pub mod files;
pub mod inspect;
pub mod invariants;
pub use tpi_core::outcome::{Effect, ModelPayload, ToolOutcome, ToolStatus};
pub mod pipeline;
pub mod plan_exec;
pub mod policy;
pub mod process;
pub mod registry;
pub mod request_input;
pub mod scheduler;
pub mod search;
pub mod selector;
pub mod terminal;
pub mod goal;
pub mod undo;
pub mod web;

use camino::Utf8PathBuf;
use tokio_util::sync::CancellationToken;

use tpi_core::message::ToolDef;

/// 工具 schema 的 JSON 序列化（schemars 生成，理论上不会失败）。
/// 失败时记录日志并返回 null（模型将看到无参数 schema，但进程不崩溃）。
pub(crate) fn schema_value<T: schemars::JsonSchema>(tool: &'static str) -> serde_json::Value {
    match serde_json::to_value(schemars::schema_for!(T)) {
        Ok(value) => normalize_tool_parameters(value),
        Err(error) => {
            tracing::error!(tool, error = %error, "tool schema 序列化失败；暴露空 schema");
            serde_json::Value::Null
        }
    }
}

/// 统一工具 parameters 的 schema invariant（§协议）：`type: object` 的 schema
/// **必须始终带 `properties`**（min：空 `{}`）。
///
/// schemars 对无字段结构体（如 `InspectArgs {}`）只生成 `{"type":"object"}`，
/// **没有 `properties`**。LM Studio 0.4.x 等严格 validator（LMStudio issue
/// #2203）在 `function.parameters.properties` 缺失时报 HTTP 400，而 DeepSeek/
/// OpenAI 等宽松 provider 接受该缺省——schema 不合法被掩饰，直到遇到严格
/// 端才暴露。这里把该 invariant 集中收敛：所有工具（含无参数工具）发出的
/// parameters 都是合法 JSON Schema。
fn normalize_tool_parameters(schema: serde_json::Value) -> serde_json::Value {
    let mut schema = match schema {
        serde_json::Value::Object(map) => map,
        other => return other,
    };
    if schema.get("type").and_then(|t| t.as_str()) == Some("object") {
        schema
            .entry("properties")
            .or_insert_with(|| serde_json::json!({}));
    }
    serde_json::Value::Object(schema)
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
    Edit,
    Write,
    Bash,
    Process,
    /// §8：六个 terminal_* 操作合并为统一的 tagged-action terminal 工具。
    Terminal,
    /// §13（AGENTS.md）：模型请求用户输入（run 挂起，非结束）。
    RequestInput,
    /// §15（AGENTS.md）：Runtime Introspection——查询能力快照。
    RuntimeInspect,
    UpdatePlan,
    WebSearch,
    WebFetch,
    ActivateSkill,
    /// §B1：基于 Mutation Journal 撤销/重做本 session 的文件变更（模型侧
    /// `tpi undo`；CAS 语义，与 CLI 共用 journal 数据源）。
    Undo,
    /// Goal：跨轮 objective（模型侧 get/complete/drop）。
    Goal,
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
    FileWriteExact,
    WorkspaceUnknown,
}

impl ToolExecutionClass {
    /// 是否需要 write-ahead（跨 crate：agent 的 tool_runtime 用）。
    pub fn requires_write_ahead(self) -> bool {
        matches!(self, Self::FileWriteExact | Self::WorkspaceUnknown)
    }
}

impl BuiltinTool {
    pub fn name(&self) -> &'static str {
        match self {
            BuiltinTool::Read => "read",
            BuiltinTool::Edit => "edit",
            BuiltinTool::Write => "write",
            BuiltinTool::Bash => "bash",
            BuiltinTool::Process => "process",
            BuiltinTool::Terminal => "terminal",
            BuiltinTool::RequestInput => "request_input",
            BuiltinTool::RuntimeInspect => "runtime_inspect",
            BuiltinTool::UpdatePlan => "update_plan",
            BuiltinTool::WebSearch => "web_search",
            BuiltinTool::WebFetch => "web_fetch",
            BuiltinTool::ActivateSkill => "activate_skill",
            BuiltinTool::Undo => "undo",
            BuiltinTool::Goal => "goal",
        }
    }

    /// 持久化/provider 名称到内建工具的唯一反解析入口。
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "edit" => Some(Self::Edit),
            "write" => Some(Self::Write),
            "bash" => Some(Self::Bash),
            "process" => Some(Self::Process),
            "terminal" => Some(Self::Terminal),
            "request_input" => Some(Self::RequestInput),
            "runtime_inspect" => Some(Self::RuntimeInspect),
            "update_plan" => Some(Self::UpdatePlan),
            "web_search" => Some(Self::WebSearch),
            "web_fetch" => Some(Self::WebFetch),
            "activate_skill" => Some(Self::ActivateSkill),
            "undo" => Some(Self::Undo),
            "goal" => Some(Self::Goal),
            _ => None,
        }
    }

    /// 调度、write-ahead 与恢复共同使用的行为事实源。
    pub(crate) fn execution_class(self) -> ToolExecutionClass {
        match self {
            Self::Read => ToolExecutionClass::FileReadExact,
            Self::Edit | Self::Write => ToolExecutionClass::FileWriteExact,
            Self::Bash => ToolExecutionClass::WorkspaceUnknown,
            // §5：process 工具读 registry / 控制 managed process，无文件副作用。
            Self::Process => ToolExecutionClass::Pure,
            // A terminal can run arbitrary shell commands over multiple calls, so
            // it must be isolated from other workspace mutations and use the
            // conservative recovery policy.
            Self::Terminal => ToolExecutionClass::WorkspaceUnknown,
            // §13：request_input 请求用户输入（无文件副作用）。
            Self::RequestInput => ToolExecutionClass::Pure,
            // §15：runtime_inspect 只读投影（无文件副作用）。
            Self::RuntimeInspect => ToolExecutionClass::Pure,
            Self::UpdatePlan | Self::WebSearch | Self::WebFetch | Self::ActivateSkill => {
                ToolExecutionClass::Pure
            }
            // §B1：undo/redo 是 journal CAS 回滚——写目标文件，但路径由 journal
            // 决定（不在 args 中）。声明 WorkspaceUnknown：串行执行、write-ahead、
            // 与其它写隔离；不参与 path 资源锁（无 args.path）。
            Self::Undo => ToolExecutionClass::WorkspaceUnknown,
            Self::Goal => ToolExecutionClass::Pure,
        }
    }

    /// 是否需要 write-ahead（跨 crate：agent 的 tool_runtime 用）。
    pub fn requires_write_ahead(self) -> bool {
        self.execution_class().requires_write_ahead()
    }

    pub fn description(&self) -> &'static str {
        match self {
            BuiltinTool::Read => {
                "Read the contents of a file (or an @artifact/... reference) as text, \
or list a directory's entries when path is a directory. \
File mode: `[revision=HASH]` header (pass to edit as revision) + `lines: X-Y of N` range + \
numbered lines `N: text` (precise single-line references). Truncated at 200 lines or 32KiB; \
use start_line/line_count to page, and follow the returned 续读 hint. \
Directory mode: entries with relative paths + scan report (scanned_files/scanned_bytes/elapsed_ms/stop_reason); \
default depth=1 (direct children), pass depth for recursion; start_line/line_count page entries. \
Follows .gitignore, skips symlinks, binaries and files over 2MiB. \
Use to inspect files before editing, or for a directory overview before searching. \
Example: read src/main.rs; read src depth=2"
            }
            BuiltinTool::Edit => {
                "Atomically edit one file: replace existing text with new text (V3, no revision needed). \
Each entry maps old_text → new_text; old_text must uniquely match the file (with \
canonical normalization: Exact → NormalizeEOL → IgnoreTrailingWhitespace → \
EquivalentIndentation; >1 match is rejected). old_text serves as location, \
precondition, context and structural boundary — no line numbers needed. \
Insert = old_text only (new_text empty... use a non-empty new_text for insert; \
delete = empty new_text). Batch: all entries resolve atomically; any failure or overlap rejects the whole batch — the file stays unchanged. \
Output: unified diff + applied count. \
Prefer edit over write for localized changes. \
Example: edit path=src/main.rs replacements=[{\"old_text\": \"let x = 1;\\n\", \"new_text\": \"let x = 2;\\n\"}]"
            }
            BuiltinTool::Write => {
                "Write an entire file: creates if missing, overwrites atomically if exists (no revision needed). \
Output: unified diff + applied count. \
Use for new files or full rewrites/overwrites. \
Example: write path=README.md content=..."
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
`background=true` starts a TPI-owned managed process: returns immediately with a logical \
process id (p17) and the command keeps running while you continue other work; manage it later \
with `process` (status/output/wait/cancel). Do not use shell '&'/nohup when TPI should manage \
the process. Do not repeatedly poll an unchanged background process — continue independent \
work and use `process wait`/`status` only when the result is needed. \
Cost: real execution time (foreground, capped by timeout_ms; default 120s, max 24h) or \
immediate return (background). \
Example: bash command=\"cargo test\" timeout_ms=180000; bash command=\"python app.py\" background=true"
            }
            BuiltinTool::Process => {
                "Manage TPI-owned background processes started with `bash(background=true)`. \
Actions: `list` (all processes), `status` (state/command/runtime/workspace + tail), `output` \
(recent output; full output in the @artifact reference), `wait` (block up to timeout_ms; \
completes with the terminal state, still-running returns `running` — not an error), \
`cancel` (terminate the whole process tree). \
Use wait/status only when the result is needed; do not poll an unchanged process. \
Cost: list/status/output/cancel ~local I/O; wait blocks up to timeout_ms. \
Example: process action=status id=\"p17\"; process action=wait id=\"p17\" timeout_ms=10000"
            }
            BuiltinTool::Terminal => {
                "Manage a persistent interactive PTY terminal (tagged action schema). \
Actions: open (create new PTY), write (send raw input bytes), read (read incremental output), \
resize (change terminal dimensions), signal (send interrupt), close (terminate and release). \
Unlike a background job, a terminal preserves stdin and terminal state. \
Examples: terminal action=open rows=24 cols=80; terminal action=write id=\"t1\" data=\"cargo test\" submit=true; \
terminal action=read id=\"t1\" after=42; terminal action=close id=\"t1\""
            }
            BuiltinTool::RequestInput => {
                "Request input from the user. Use when you need a decision, clarification, \
permission, or additional information that only the user can provide. \
The run SUSPENDS at this point (it does not end): questions are shown to the user, \
their answers are recorded, and the run continues with full history. \
Only available in interactive runs - in non-interactive mode (print/web) it is \
rejected; proceed with existing information instead. \
Supports multiple questions in one call: pass a `questions` array; each item may \
carry a `header` grouping title and suggested `options` the user can pick (by number) \
or override with custom text. Batch related questions into one call instead of \
suspending repeatedly. Do not end the conversation with a question — call this tool instead. \
Example: request_input questions=[{\"question\": \"should I run the full test suite?\", \
\"options\": [{\"label\": \"yes\"}, {\"label\": \"no\"}]}, {\"question\": \"deploy target?\", \"header\": \"release\"}]"
            }
            BuiltinTool::RuntimeInspect => {
                "Inspect the current runtime capabilities: available tools (with provider/\
origin), discovered skills, workspace kind/identity, and managed processes. \
Use when you need to know what you can do in this environment instead of \
guessing from the system prompt. Read-only; takes no arguments. \
Example: runtime_inspect"
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
            BuiltinTool::ActivateSkill => {
                "Activate a skill by name (progressive disclosure, Level 2): reads the \
full SKILL.md body and returns it (with references/scripts listings). \
Available skills are listed in the system prompt's Available skills section. \
Skills teach the Agent how to combine existing tools (bash/read/MCP) for a \
workflow — they are NOT tools themselves. \
Use when the task matches a skill's description. \
Example: activate_skill name=\"rust-review\""
            }
            BuiltinTool::Undo => {
                "Undo or redo file changes committed by this session's edit/write tools, \
based on the Mutation Journal (before/after snapshots recorded on every successful \
edit/write). CAS semantics: a file is restored only if its current content matches \
the journal snapshot; if any file conflicts (externally modified), NOTHING is written \
and all files are reported — safe against partial rollback. \
This works WITHOUT git: use it to revert your own mistaken edits, roll back an \
experimental change, or restore a file deleted/rewritten via write. \
It does NOT cover changes made through bash (external writes are invisible to the journal). \
Actions: undo (default) restores previous content; redo reapplies. \
Scope: last (default) reverts the most recent mutation; all reverts the whole session \
to the earliest state. \
Example: undo; undo action=redo scope=all"
            }
            BuiltinTool::Goal => {
                "Manage the durable completion goal for multi-round autonomous work. \
Ops: get (current goal), complete (mark achieved, must evidence), drop (clear). \
Do not use for trivial single-turn work. Use get before complete. \
Example: goal op=get; goal op=complete"
            }
        }
    }

    /// 工具 schema（§5.2 schemars：从参数类型生成，减少描述与实现漂移）。
    pub fn schema(&self) -> ToolDef {
        let parameters = match self {
            BuiltinTool::Read => schema_value::<files::ReadArgs>("read"),
            BuiltinTool::Edit => schema_value::<edit::EditArgs>("edit"),
            BuiltinTool::Write => schema_value::<files::WriteArgs>("write"),
            BuiltinTool::Bash => schema_value::<command::BashArgs>("bash"),
            BuiltinTool::Process => schema_value::<process::ProcessArgs>("process"),
            BuiltinTool::Terminal => schema_value::<terminal::TerminalArgs>("terminal"),
            BuiltinTool::RequestInput => {
                schema_value::<request_input::RequestInputArgs>("request_input")
            }
            BuiltinTool::RuntimeInspect => schema_value::<inspect::InspectArgs>("runtime_inspect"),
            BuiltinTool::UpdatePlan => {
                schema_value::<tpi_core::plan::UpdatePlanArgs>("update_plan")
            }
            BuiltinTool::WebSearch => schema_value::<web::WebSearchArgs>("web_search"),
            BuiltinTool::WebFetch => schema_value::<web::WebFetchArgs>("web_fetch"),
            BuiltinTool::ActivateSkill => {
                schema_value::<crate::skills::activate::ActivateSkillArgs>("activate_skill")
            }
            BuiltinTool::Undo => schema_value::<undo::UndoArgs>("undo"),
            BuiltinTool::Goal => schema_value::<crate::tool::goal::GoalArgs>("goal"),
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
            BuiltinTool::Edit => parse_args_typed("edit", arguments, ValidatedArgs::Edit),
            BuiltinTool::Write => parse_args_typed("write", arguments, ValidatedArgs::Write),
            BuiltinTool::Bash => parse_args_typed("bash", arguments, ValidatedArgs::Bash),
            BuiltinTool::Process => parse_args_typed("process", arguments, ValidatedArgs::Process),
            BuiltinTool::Terminal => {
                parse_args_typed("terminal", arguments, ValidatedArgs::Terminal)
            }
            BuiltinTool::RequestInput => {
                parse_args_typed("request_input", arguments, ValidatedArgs::RequestInput)
            }
            BuiltinTool::RuntimeInspect => {
                parse_args_typed("runtime_inspect", arguments, ValidatedArgs::RuntimeInspect)
            }
            BuiltinTool::UpdatePlan => {
                parse_args_typed("update_plan", arguments, ValidatedArgs::UpdatePlan)
            }
            BuiltinTool::WebSearch => {
                parse_args_typed("web_search", arguments, ValidatedArgs::WebSearch)
            }
            BuiltinTool::WebFetch => {
                parse_args_typed("web_fetch", arguments, ValidatedArgs::WebFetch)
            }
            BuiltinTool::ActivateSkill => {
                parse_args_typed("activate_skill", arguments, ValidatedArgs::ActivateSkill)
            }
            BuiltinTool::Undo => parse_args_typed("undo", arguments, ValidatedArgs::Undo),
            BuiltinTool::Goal => parse_args_typed("goal", arguments, ValidatedArgs::Goal),
        }
    }
}

/// 校验通过的参数（§8.2 `PreparedToolCall.validated_args` 的 M1 形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedArgs {
    Read(files::ReadArgs),
    Edit(edit::EditArgs),
    Write(files::WriteArgs),
    Bash(command::BashArgs),
    Process(process::ProcessArgs),
    Terminal(terminal::TerminalArgs),
    RequestInput(request_input::RequestInputArgs),
    RuntimeInspect(inspect::InspectArgs),
    UpdatePlan(tpi_core::plan::UpdatePlanArgs),
    WebSearch(web::WebSearchArgs),
    WebFetch(web::WebFetchArgs),
    ActivateSkill(crate::skills::activate::ActivateSkillArgs),
    Undo(undo::UndoArgs),
    Goal(crate::tool::goal::GoalArgs),
}

impl ValidatedArgs {
    /// 参数由哪个工具产生。调度边界用它验证 `(BuiltinTool, ValidatedArgs)`
    /// 没有被错误拼接；不变量破坏时必须保守隔离而不是提升为 Pure。
    pub(crate) fn tool(&self) -> BuiltinTool {
        match self {
            Self::Read(_) => BuiltinTool::Read,
            Self::Edit(_) => BuiltinTool::Edit,
            Self::Write(_) => BuiltinTool::Write,
            Self::Bash(_) => BuiltinTool::Bash,
            Self::Process(_) => BuiltinTool::Process,
            Self::Terminal(_) => BuiltinTool::Terminal,
            Self::RequestInput(_) => BuiltinTool::RequestInput,
            Self::RuntimeInspect(_) => BuiltinTool::RuntimeInspect,
            Self::UpdatePlan(_) => BuiltinTool::UpdatePlan,
            Self::WebSearch(_) => BuiltinTool::WebSearch,
            Self::WebFetch(_) => BuiltinTool::WebFetch,
            Self::ActivateSkill(_) => BuiltinTool::ActivateSkill,
            Self::Undo(_) => BuiltinTool::Undo,
            Self::Goal(_) => BuiltinTool::Goal,
        }
    }

    /// 文件资源型工具的原始 path 参数；具体 Exact/Recursive 和 Read/Write
    /// 由 [`BuiltinTool::execution_class`] 决定。
    pub(crate) fn path(&self) -> Option<&str> {
        match self {
            Self::Read(args) => Some(&args.path),
            Self::Edit(args) => Some(&args.path),
            Self::Write(args) => Some(&args.path),
            Self::Bash(_)
            | Self::Process(_)
            | Self::Terminal(_)
            | Self::RequestInput(_)
            | Self::RuntimeInspect(_)
            | Self::UpdatePlan(_)
            | Self::WebSearch(_)
            | Self::WebFetch(_)
            | Self::ActivateSkill(_)
            | Self::Undo(_)
            | Self::Goal(_) => None,
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
    pub call_id: tpi_core::ids::ToolCallId,
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
    pub call_id: tpi_core::ids::ToolCallId,
    /// 流式输出通道（bash 实时输出；None = 无 UI 订阅）。
    pub output_tx: Option<tokio::sync::mpsc::Sender<ToolStreamEvent>>,
    /// 遗留的 scan snapshot 类型（search/glob 已删除；字段保留以最小改动面）。
    pub scan_snapshots:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, search::ScanSnapshot>>>,
    /// Git Bash 路径解析（§11.2；None = 自动探测）。
    pub shell_path: Option<Utf8PathBuf>,
    /// session-local bounded SnapshotStore（§10.1）。
    pub snapshot_store: std::sync::Arc<std::sync::Mutex<crate::tool::edit::SnapshotStore>>,
    /// 当前原子短计划（§13；agent loop 持有，update_plan 原子替换）。
    pub current_plan: std::sync::Arc<std::sync::Mutex<Option<tpi_core::plan::Plan>>>,
    /// 当前 goal（与 plan 同为 durable harness state；user /goal + 模型 goal tool 共用）。
    #[allow(clippy::type_complexity)]
    pub current_goal: Option<std::sync::Arc<std::sync::Mutex<Option<tpi_core::goal::Goal>>>>,
    /// Logical Shell Session（任务书 §S1：属于 Workspace；bash 工具读写）。
    /// 与 `workspace` 内的 shell 是同一状态源（构造时共享 Arc）。
    pub shell: std::sync::Arc<std::sync::Mutex<crate::shell::ShellSessionState>>,
    /// ActiveWorkspace（§26-§30：Tool Protocol 的分发目标）。
    /// `workspace_root`/`allow_outside_workspace`/`shell` 是当前 workspace 的投影。
    pub workspace: std::sync::Arc<std::sync::Mutex<crate::workspace::ActiveWorkspace>>,
    /// ManagedProcess registry（任务书 §8：session 级共享；background bash 注册，
    /// `process` 工具读取/取消）。
    pub processes: std::sync::Arc<std::sync::Mutex<crate::process::managed::ProcessRegistry>>,
    /// Persistent PTY terminal registry. Separate lifecycle from ManagedProcess.
    pub terminals: std::sync::Arc<std::sync::Mutex<crate::terminal::TerminalRegistry>>,
    /// ToolRegistry（builtin + MCP；runtime_inspect 枚举能力用；与 ToolRuntime 共享）。
    pub registry: std::sync::Arc<std::sync::Mutex<crate::tool::registry::ToolRegistry>>,
    /// 交互模式（`-p` 为 false；§11 移除 ask_user 后仅保留供未来交互原语使用）。
    pub interactive: bool,
    /// Workspace transaction session（§新架构：统一 workspace mutation tracking）。
    pub workspace_session: Option<std::sync::Arc<crate::workspace::session::WorkspaceSession>>,
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

/// Windows + Git Bash/MSYS2 环境：把 Unix 风格绝对路径（`/tmp/...`、`/c/...`）
/// 翻译为 Windows 路径（`cygpath -w`），否则 Windows 的 `Path::is_absolute()`
/// 会把 `/tmp` 判为绝对路径 → 解析成当前盘根 `C:\tmp`（read `/tmp/x` 报
/// `not found: C:\tmp\x`，而 Git Bash 的 `/tmp` 实际映射到用户临时目录）。
///
/// 非 Windows、不是 Unix 绝对路径、或 cygpath 不可用/失败时原样返回。
#[cfg(windows)]
pub fn translate_msys_path(path: &str) -> String {
    let trimmed = path.trim();
    // Windows 绝对路径（盘符 `C:\`、UNC `\\`）不转换；
    // 相对路径 / 盘符相对（`C:foo`）也不转换。
    let looks_windows_abs = (trimmed.len() >= 3
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
        && trimmed.as_bytes()[1] == b':')
        || trimmed.starts_with("\\\\");
    if !trimmed.starts_with('/') || looks_windows_abs {
        return trimmed.to_string();
    }
    // `cygpath -w`：Git Bash / MSYS2 环境可用；失败则原样返回（调用方再报错）。
    if let Ok(out) = std::process::Command::new("cygpath")
        .args(["-w", trimmed])
        .output()
        && out.status.success()
        && let Ok(s) = String::from_utf8(out.stdout)
    {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(not(windows))]
pub fn translate_msys_path(path: &str) -> String {
    path.trim().to_string()
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
    // 统一入口：先翻译 Unix 风格路径（Windows + Git Bash）。
    let path = translate_msys_path(path);
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

/// 统一文件 identity（§B4）：Windows 大小写折叠，所有组件共用。
///
/// `src/Foo.rs` 与 `src/foo.rs` 是同一物理文件——调度锁、snapshot store、
/// mutation journal 必须用同一 identity 判定，否则同一文件被当作两个资源
/// 并行写（ISSUE-004 静默丢内容）或 undo 匹配失败。
/// 非 Windows 大小写敏感文件系统原样返回。
#[cfg(windows)]
pub fn path_identity(path: &str) -> String {
    path.to_lowercase()
}

#[cfg(not(windows))]
pub fn path_identity(path: &str) -> String {
    path.to_string()
}

/// 调度锁的路径标识（与工具实际解析一致，保证等价写法映射到同一锁）。
///
/// 严格模式：`resolve_workspace_path`（外部路径本就不会真正执行）——
/// 同样应用大小写折叠（§B4：Windows 严格模式下 `Foo.rs`/`foo.rs` 也要
/// 收敛为同一把锁，否则 ISSUE-004 竞态在严格模式仍存在）。
/// 自由模式：对路径做词法规范化，外部绝对路径的等价写法（`.\`、`..`、大小写之外的
/// 词法差异）收敛为同一标识——否则同一外部文件可能被两个等价路径并行写入（竞态）。
pub fn resolve_lock_path(
    workspace_root: &Utf8PathBuf,
    path: &str,
    allow_outside_workspace: bool,
) -> Utf8PathBuf {
    if !allow_outside_workspace {
        let resolved = resolve_workspace_path(workspace_root, path)
            .unwrap_or_else(|_| Utf8PathBuf::from(path.trim()));
        return Utf8PathBuf::from(path_identity(resolved.as_str()));
    }
    // 与 resolve_write_path 同一入口翻译（Unix 风格 → Windows），保证
    // 等价写法（`/tmp/x` vs `C:\Users\...\Temp\x`）映射到同一把锁。
    let trimmed = translate_msys_path(path);
    let candidate = Utf8PathBuf::from(trimmed);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    };
    let resolved = normalize_lexical(joined.as_std_path())
        .ok()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
        .unwrap_or(joined);
    casefold_lock_path(resolved)
}

/// Windows 文件系统大小写不敏感：`C:\ws\main.rs` 与 `C:\ws\MAIN.rs` 是**同一
/// 物理文件**。锁身份必须大小写无关，否则同一文件的两笔写被调度为不同资源锁、
/// 并行执行，后完成者覆盖先完成者且双方都报 succeeded（ISSUE-004 静默丢内容）。
/// 统一小写是安全的身份收敛（不会把不同文件误并为同一把锁）。非 Windows 的
/// 大小写敏感文件系统不做处理。
#[cfg(windows)]
fn casefold_lock_path(path: Utf8PathBuf) -> Utf8PathBuf {
    Utf8PathBuf::from(path.as_str().to_lowercase())
}

#[cfg(not(windows))]
fn casefold_lock_path(path: Utf8PathBuf) -> Utf8PathBuf {
    path
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

/// P7 下沉：`validate_artifact_component` 移入 core（`tpi_core::util`），此处
/// re-export 保持 `tool::validate_artifact_component` 兼容。
pub use tpi_core::util::validate_artifact_component;

/// 路径被拒绝时的标准 tool outcome。
pub fn path_rejected_outcome(tool: &str, error: PathResolveError) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        tpi_core::outcome::ModelPayload {
            status: tpi_core::outcome::ToolStatus::Rejected,
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
        (BuiltinTool::Process, ValidatedArgs::Process(args)) => process::process(args, ctx).await,
        (BuiltinTool::Terminal, ValidatedArgs::Terminal(args)) => {
            terminal::terminal(args, ctx).await
        }
        (BuiltinTool::RequestInput, ValidatedArgs::RequestInput(args)) => {
            request_input::request_input(args, ctx).await
        }
        (BuiltinTool::RuntimeInspect, ValidatedArgs::RuntimeInspect(args)) => {
            inspect::runtime_inspect(args, ctx).await
        }
        // §Skills：activate_skill 是同步控制操作（读 SKILL.md）。
        (BuiltinTool::ActivateSkill, ValidatedArgs::ActivateSkill(args)) => {
            crate::skills::activate::activate_skill(args, ctx)
        }
        // §B1：undo/redo 是同步文件回滚（journal CAS；blocking 池执行）。
        (BuiltinTool::Undo, ValidatedArgs::Undo(args)) => {
            let ctx = ctx.clone();
            tokio::task::spawn_blocking(move || undo::undo(args, &ctx))
                .await
                .unwrap_or_else(|e| {
                    ToolOutcome::failed(
                        "undo",
                        ModelPayload {
                            status: ToolStatus::Failed,
                            program: Some("undo".into()),
                            exit_code: None,
                            duration_ms: 0,
                            output: format!("undo 任务失败: {e}"),
                            effect: None,
                            artifact: None,
                        },
                    )
                })
        }
        // §11/BUG-009：同步工具（文件 IO/正则扫描）挪到 blocking 池，避免大目录扫描、
        // 大文件读取、网络盘遍历阻塞 Tokio worker——并行 4 个工具时可能占满全部
        // worker，导致 TUI 事件循环（ui_rx/ticker/键盘）明显卡顿。
        (tool, args) => {
            let ctx = ctx.clone();
            let plan = plan.cloned();
            tokio::task::spawn_blocking(move || match (tool, args) {
                (BuiltinTool::Read, ValidatedArgs::Read(args)) => files::read(args, &ctx),
                (BuiltinTool::Edit, ValidatedArgs::Edit(args)) => files::edit(args, &ctx, plan.as_ref()),
                (BuiltinTool::Write, ValidatedArgs::Write(args)) => files::write(args, &ctx, plan.as_ref()),
                // §13：update_plan 是原生同步控制操作。
                (BuiltinTool::UpdatePlan, ValidatedArgs::UpdatePlan(args)) => plan_exec::update_plan(args, &ctx),
                (BuiltinTool::Goal, ValidatedArgs::Goal(args)) => goal::goal(args, &ctx),
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
        BuiltinTool::Edit,
        BuiltinTool::Write,
        BuiltinTool::Bash,
        BuiltinTool::Process,
        BuiltinTool::Terminal,
        BuiltinTool::RequestInput,
        BuiltinTool::RuntimeInspect,
        BuiltinTool::UpdatePlan,
        BuiltinTool::WebSearch,
        BuiltinTool::WebFetch,
        BuiltinTool::ActivateSkill,
        BuiltinTool::Goal,
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

    /// 协议 invariant：所有 builtin 工具的 parameters（`type: object`）必须带
    /// `properties`（含无参数工具，如 runtime_inspect）。无此字段会触发严格
    /// validator（LM Studio 0.4.x）的 HTTP 400。
    #[test]
    fn every_builtin_tool_parameters_carries_properties() {
        for tool in implemented_tools() {
            let def = tool.schema();
            let params = &def.parameters;
            assert!(
                params.is_object(),
                "{}/{} 参数 schema 必须是 object: {params}",
                tool.name(),
                def.name
            );
            if params.get("type") == Some(&serde_json::json!("object")) {
                assert!(
                    params.get("properties").is_some(),
                    "{}/{} 的 object schema 必须带 properties: {params}",
                    tool.name(),
                    def.name
                );
            }
        }
    }

    /// normalize_tool_parameters 对无字段 object schema 补 `properties: {}`。
    #[test]
    fn normalize_empty_object_schema_keeps_properties_object() {
        let bare = serde_json::json!({ "type": "object" });
        let normalized = normalize_tool_parameters(bare);
        assert_eq!(normalized["type"], "object");
        assert_eq!(normalized["properties"], serde_json::json!({}));

        // 已有 properties 不覆盖。
        let with = serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } }
        });
        let normalized = normalize_tool_parameters(with);
        assert_eq!(normalized["properties"]["id"]["type"], "string");

        // 非 object 根节点原样透传。
        let array = serde_json::json!({ "type": "array" });
        assert_eq!(normalize_tool_parameters(array.clone()), array);
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
            // recovery 策略已下沉 core（tpi_core::outcome::tool_recovery_policy），
            // 一致性由 outcome::recovery_policy_tests 保证。
        }
    }

    /// P7-02 拆 crate：core 的 tool_recovery_policy 名字表必须与 execution_class
    /// 映射一致（单一事实源防漂移；core 不依赖 capabilities，此断言在 tpi 侧）。
    #[test]
    fn recovery_policy_consistent_with_execution_class() {
        for tool in implemented_tools() {
            let expected = match tool.execution_class() {
                ToolExecutionClass::Pure | ToolExecutionClass::FileReadExact => {
                    tpi_core::outcome::ToolRecoveryPolicy::NoEffect
                }
                ToolExecutionClass::FileWriteExact => {
                    tpi_core::outcome::ToolRecoveryPolicy::FileCommit
                }
                ToolExecutionClass::WorkspaceUnknown => {
                    tpi_core::outcome::ToolRecoveryPolicy::Unknown
                }
            };
            assert_eq!(
                tpi_core::outcome::tool_recovery_policy(tool.name()),
                expected,
                "core 策略与 BuiltinTool 执行分类一致: {}",
                tool.name()
            );
        }
    }

    /// §4 ACI：工具 description 必须含典型调用示例（书中：示例提升工具准确率 72%→90%）。
    #[test]
    fn tool_descriptions_carry_examples_and_cost() {
        for tool in implemented_tools() {
            let desc = tool.description();
            // “Example”前缀兼容单数/复数形式（terminal 列举多个示例用 Examples:）。
            assert!(
                desc.contains("Example"),
                "{} description 必须含典型示例: {desc}",
                tool.name()
            );
        }
        // terminal 用复数形式列举多个示例（同样满足“必须含示例”约束）。
        let terminal = BuiltinTool::Terminal.description();
        assert!(
            terminal.contains("Examples:") && terminal.contains("terminal action=open"),
            "terminal description 必须含典型示例"
        );
        // 高频工具必须有执行代价或边界（bash 代价 / web_fetch 代价+SSRF）。
        for (name, marker) in [
            ("bash", "timeout_ms"),
            ("web_fetch", "SSRF"),
            ("web_search", "discovery only"),
            ("edit", "Atomically edit"),
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
        let fetch_schema = BuiltinTool::WebFetch.schema();
        assert!(
            fetch_schema.parameters.to_string().contains("loopback"),
            "web_fetch.url 边界必须进 schema"
        );
    }

    /// §13 升级（对标 AskUserQuestion）：request_input schema 必须暴露
    /// questions 数组与 header/options 字段（模型侧结构化参数），
    /// 同时保留旧 question 字段兼容。
    #[test]
    fn request_input_schema_exposes_questions_header_options() {
        let schema = BuiltinTool::RequestInput.schema();
        let params = schema.parameters.to_string();
        for marker in ["questions", "header", "options", "question"] {
            assert!(
                params.contains(marker),
                "request_input schema 应含 {marker}: {params}"
            );
        }
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
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            current_goal: None,
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            terminals: Default::default(),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
            workspace_session: None,
        };
        let outcome = execute(
            BuiltinTool::Read,
            ValidatedArgs::Read(files::ReadArgs {
                path: path.to_string(),
                start_line: 1,
                line_count: 200,
                depth: None,
            }),
            &ctx,
            None,
        )
        .await;
        assert_eq!(outcome.status, tpi_core::outcome::ToolStatus::Succeeded);
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
        // V3 后 revision 已从协议移除（内部 BLAKE3 CAS，模型不可见）。
        assert!(
            !msg.contains("revision"),
            "V3 协议不得再要求 revision: {msg}"
        );
        assert!(
            msg.contains("replacements"),
            "expected shape 必须含 replacements（V3）: {msg}"
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

    /// Windows + Git Bash：Unix 风格绝对路径（`/tmp/...`）翻译为 Windows 路径，
    /// 不再解析成当前盘根 `C:\tmp`（read 修复）。
    #[test]
    fn unix_abs_path_is_translated_on_windows() {
        let translated = translate_msys_path("/tmp/probe.txt");
        if cfg!(windows) {
            // 本环境（Git Bash/MSYS2）cygpath 可用 → 应翻译为 Windows 路径。
            if cygpath_available() {
                assert!(
                    !translated.starts_with("\\tmp") && !translated.starts_with("/tmp"),
                    "Unix 绝对路径必须翻译，而非原样当 Windows 绝对路径: {translated}"
                );
                assert!(
                    translated.contains("\\") && translated.len() > 5,
                    "翻译结果应为 Windows 路径: {translated}"
                );
            }
        } else {
            assert_eq!(translated, "/tmp/probe.txt");
        }
        // Windows 绝对路径与相对路径不翻译。
        assert_eq!(translate_msys_path("C:\\x\\y.rs"), "C:\\x\\y.rs");
        assert_eq!(translate_msys_path("src/main.rs"), "src/main.rs");
    }

    /// read 的 `/tmp/x` 在 Windows + Git Bash 下解析到真实临时目录，
    /// 而不是 C 盘根。
    #[test]
    fn resolve_write_path_translates_unix_tmp() {
        let workspace = Utf8PathBuf::from("C:\\ws");
        if !cfg!(windows) || !cygpath_available() {
            return; // 仅 Windows + cygpath 环境验证
        }
        let resolved = resolve_write_path(&workspace, "/tmp/probe.txt", true).unwrap();
        let s = resolved.as_str().to_lowercase();
        assert!(
            s.contains("temp") || s.contains("\\tmp") || s.starts_with("c:\\"),
            "应解析到真实临时目录，而非 C:\\tmp: {resolved}"
        );
        assert!(!s.starts_with("c:\\tmp"), "不得解析成 C:\\tmp: {resolved}");
    }

    #[cfg(windows)]
    fn cygpath_available() -> bool {
        std::process::Command::new("cygpath")
            .arg("-w")
            .arg("/")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    fn cygpath_available() -> bool {
        false
    }

    /// ISSUE-004：Windows 大小写不敏感文件系统下，锁身份必须大小写无关——
    /// `main.rs` 与 `MAIN.rs` 是同一物理文件，必须映射到同一把锁（否则两笔
    /// 并行写会静默丢内容）。非 Windows 保持大小写敏感语义。
    #[test]
    fn lock_path_is_casefolded_on_windows() {
        let root = camino::Utf8PathBuf::from("C:\\ws");
        let lower = resolve_lock_path(&root, "src/main.rs", true);
        let upper = resolve_lock_path(&root, "src/MAIN.rs", true);
        let mixed = resolve_lock_path(&root, "SRC/Main.Rs", true);
        #[cfg(windows)]
        {
            assert_eq!(lower, upper, "大小写变体必须映射到同一把锁");
            assert_eq!(lower, mixed);
            assert_eq!(
                lower.as_str(),
                lower.as_str().to_lowercase(),
                "Windows 锁路径必须统一小写"
            );
        }
        #[cfg(not(windows))]
        {
            assert_ne!(lower, upper, "大小写敏感文件系统上不同大小写是不同文件");
        }
    }

    /// §B4：严格模式锁也大小写折叠（此前只折叠自由模式，严格模式 ISSUE-004
    /// 竞态仍存在）。`src/Foo.rs` 与 `src/foo.rs` 必须同一锁。
    #[test]
    fn lock_path_casefolded_in_strict_mode() {
        let root = camino::Utf8PathBuf::from("C:\\ws");
        let lower = resolve_lock_path(&root, "src/foo.rs", false);
        let upper = resolve_lock_path(&root, "src/Foo.rs", false);
        #[cfg(windows)]
        {
            assert_eq!(lower, upper, "严格模式大小写变体必须同一锁（§B4）");
        }
        #[cfg(not(windows))]
        {
            assert_ne!(lower, upper);
        }
    }

    /// §B4：path_identity 统一收敛（scheduler/snapshot/journal 共用）。
    #[test]
    fn path_identity_unifies_case_variants() {
        #[cfg(windows)]
        {
            assert_eq!(
                path_identity("C:\\ws\\Main.rs"),
                path_identity("c:\\ws\\main.RS")
            );
        }
        #[cfg(not(windows))]
        {
            assert_ne!(path_identity("a/Main.rs"), path_identity("a/main.rs"));
        }
    }
}
