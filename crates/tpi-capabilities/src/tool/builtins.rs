//! §11：每个 builtin 工具直接实现 `Tool` trait。
//!
//! 删除 `BuiltinTool` 枚举 + `ValidatedArgs` 枚举 + `BuiltinToolAdapter`
//! 双轨系统后，builtin 工具与 MCP 工具走完全相同的 ToolRegistry → Tool::execute 路径。
//! 工具来源（origin）只作为 metadata，不选择执行分支。

use std::sync::Arc;

use async_trait::async_trait;

use crate::tool::ToolContext;
use crate::tool::registry::{Tool, ToolAccessClass, ToolOrigin};
use tpi_core::outcome::ToolOutcome;

/// 参数解析失败时的标准化拒绝输出。
fn rejected(tool: &str, error: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        tpi_core::outcome::ModelPayload {
            status: tpi_core::outcome::ToolStatus::Rejected,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!("status: rejected\ntool: {tool}\nerror: invalid_arguments\n\n{error}"),
            effect: None,
            artifact: None,
        },
    )
}

/// 从 target path 生成 commit plan（edit/write 需要）。
fn plan_for(path: &camino::Utf8PathBuf) -> crate::tool::edit::CommitPlan {
    crate::tool::edit::prepare_commit(path)
}

// ===================================================================
// 所有 builtin Tool 实现——每个 struct 对应一个模型可见工具。
// ===================================================================

pub struct ReadTool;
#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read the contents of a file (or an @artifact/... reference) as text, \
or list a directory's entries when path is a directory. \
File mode: `lines: X-Y of N` range + numbered lines `N: text`. \
Truncated at 200 lines or 32KiB; use start_line/line_count to page. \
Directory mode: entries with relative paths + scan report; default depth=1. \
Example: read src/main.rs; read src depth=2"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::files::ReadArgs>("read")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn access_class(&self) -> ToolAccessClass {
        ToolAccessClass::ReadOnly
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::files::ReadArgs>(args) {
            Ok(a) => crate::tool::files::read(a, ctx),
            Err(e) => rejected("read", e),
        }
    }
}

pub struct EditTool;
#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Atomically edit one file: replace existing text with new text (V3, no revision needed). \
Each entry maps old_text → new_text; old_text must uniquely match the file. \
Prefer edit over write for localized changes. \
Example: edit path=src/main.rs replacements=[{\"old_text\": \"let x = 1;\\n\", \"new_text\": \"let x = 2;\\n\"}]"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::edit::EditArgs>("edit")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::edit::EditArgs>(args) {
            Ok(a) => {
                let path = match crate::tool::resolve_tool_path(ctx, &a.path) {
                    Ok(p) => p,
                    Err(e) => return crate::tool::path_rejected_outcome("edit", e),
                };
                let plan = plan_for(&path);
                crate::tool::files::edit(a, ctx, Some(&plan))
            }
            Err(e) => rejected("edit", e),
        }
    }
}

pub struct WriteTool;
#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write an entire file: creates if missing, overwrites atomically if exists (no revision needed). \
Output: unified diff + applied count. Use for new files or full rewrites/overwrites. \
Example: write path=README.md content=..."
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::files::WriteArgs>("write")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::files::WriteArgs>(args) {
            Ok(a) => {
                let path = match crate::tool::resolve_tool_path(ctx, &a.path) {
                    Ok(p) => p,
                    Err(e) => return crate::tool::path_rejected_outcome("write", e),
                };
                let plan = plan_for(&path);
                crate::tool::files::write(a, ctx, Some(&plan))
            }
            Err(e) => rejected("write", e),
        }
    }
}

pub struct GoalTool;
#[async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &str {
        "goal"
    }
    fn description(&self) -> &str {
        "Manage the durable completion goal for multi-round autonomous work. \
Ops: get (current goal), complete (mark achieved, must evidence), drop (clear). \
Do not use for trivial single-turn work. Use get before complete. \
Example: goal op=get; goal op=complete"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::goal::GoalArgs>("goal")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::goal::GoalArgs>(args) {
            Ok(a) => crate::tool::goal::goal(a, ctx),
            Err(e) => rejected("goal", e),
        }
    }
}

pub struct BashTool;
#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a command through Git Bash (`set -o pipefail` enabled; stderr is not \
failure — check status/exit_code). This is the only execution tool: use it for programs, \
builds, tests, git, pipelines, redirection and compound commands. \
Cost: real execution time (foreground, capped by timeout_ms; default 120s, max 24h) or \
immediate return (background). \
Example: bash command=\"cargo test\" timeout_ms=180000; bash command=\"python app.py\" background=true"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::command::BashArgs>("bash")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::command::BashArgs>(args) {
            Ok(a) => crate::tool::command::bash(a, ctx).await,
            Err(e) => rejected("bash", e),
        }
    }
}

pub struct ProcessTool;
#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str {
        "process"
    }
    fn description(&self) -> &str {
        "Manage TPI-owned background processes started with `bash(background=true)`. \
Actions: `list` (all processes), `status` (state/command/runtime/workspace + tail), `output` \
(recent output; full output in the @artifact reference), `wait` (block up to timeout_ms; \
completes with the terminal state, still-running returns `running` — not an error), \
`cancel` (terminate the whole process tree). \
Cost: list/status/output/cancel ~local I/O; wait blocks up to timeout_ms. \
Example: process action=status id=\"p17\"; process action=wait id=\"p17\" timeout_ms=10000"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::process::ProcessArgs>("process")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::process::ProcessArgs>(args) {
            Ok(a) => crate::tool::process::process(a, ctx).await,
            Err(e) => rejected("process", e),
        }
    }
}

pub struct TerminalTool;
#[async_trait]
impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "terminal"
    }
    fn description(&self) -> &str {
        "Manage a persistent interactive PTY terminal (tagged action schema). \
Actions: open (create new PTY), write (send raw input bytes), read (read incremental output), \
resize (change terminal dimensions), signal (send interrupt), close (terminate and release). \
Examples: terminal action=open rows=24 cols=80; terminal action=write id=\"t1\" data=\"cargo test\" submit=true; \
terminal action=read id=\"t1\" after=42; terminal action=close id=\"t1\""
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::terminal::TerminalArgs>("terminal")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::terminal::TerminalArgs>(args) {
            Ok(a) => crate::tool::terminal::terminal(a, ctx).await,
            Err(e) => rejected("terminal", e),
        }
    }
}

pub struct RequestInputTool;
#[async_trait]
impl Tool for RequestInputTool {
    fn name(&self) -> &str {
        "request_input"
    }
    fn description(&self) -> &str {
        "Request input from the user. Use when you need a decision, clarification, \
permission, or additional information that only the user can provide. \
The run SUSPENDS at this point (it does not end). \
Supports multiple questions in one call: pass a `questions` array. \
Example: request_input questions=[{\"question\": \"deploy target?\", \"header\": \"release\", \
\"options\": [{\"label\": \"Local\"}, {\"label\": \"LAN\"}]}]"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::request_input::RequestInputArgs>("request_input")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::request_input::RequestInputArgs>(args) {
            Ok(a) => crate::tool::request_input::request_input(a, ctx).await,
            Err(e) => rejected("request_input", e),
        }
    }
}

pub struct RuntimeInspectTool;
#[async_trait]
impl Tool for RuntimeInspectTool {
    fn name(&self) -> &str {
        "runtime_inspect"
    }
    fn description(&self) -> &str {
        "Inspect the current runtime capabilities: available tools (with provider/\
origin), discovered skills, workspace kind/identity, and managed processes. \
Read-only; takes no arguments. Example: runtime_inspect"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::inspect::InspectArgs>("runtime_inspect")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn access_class(&self) -> ToolAccessClass {
        ToolAccessClass::ReadOnly
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::inspect::InspectArgs>(args) {
            Ok(a) => crate::tool::inspect::runtime_inspect(a, ctx).await,
            Err(e) => rejected("runtime_inspect", e),
        }
    }
}

pub struct UpdatePlanTool;
#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn description(&self) -> &str {
        "Update the plan with a complete explicit snapshot (max 7 total items). Every item \
MUST be an object {\"text\": ..., \"status\": \"pending|in_progress|completed|cancelled|blocked\"}. \
Example: update_plan items=[{\"text\": \"inspect build\", \"status\": \"completed\"}]"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<tpi_core::plan::UpdatePlanArgs>("update_plan")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<tpi_core::plan::UpdatePlanArgs>(args) {
            Ok(a) => crate::tool::plan_exec::update_plan(a, ctx),
            Err(e) => rejected("update_plan", e),
        }
    }
}

pub struct WebSearchTool;
#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web (DuckDuckGo HTML endpoint, free, no API key) to discover sources. \
Results are for discovery only. \
Example: web_search query=\"rust tokio select\" count=5"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::web::WebSearchArgs>("web_search")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::web::WebSearchArgs>(args) {
            Ok(a) => crate::tool::web::web_search(a, ctx).await,
            Err(e) => rejected("web_search", e),
        }
    }
}

pub struct WebFetchTool;
#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a URL and convert HTML to plain text (bounded to 48KiB body). \
The `url` MUST come from a web_search result (or the user's explicit link). \
Example: web_fetch url=\"https://docs.rs/tokio\""
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::web::WebFetchArgs>("web_fetch")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::web::WebFetchArgs>(args) {
            Ok(a) => crate::tool::web::web_fetch(a, ctx).await,
            Err(e) => rejected("web_fetch", e),
        }
    }
}

pub struct ActivateSkillTool;
#[async_trait]
impl Tool for ActivateSkillTool {
    fn name(&self) -> &str {
        "activate_skill"
    }
    fn description(&self) -> &str {
        "Activate a skill by name (progressive disclosure, Level 2): reads the \
full SKILL.md body and returns it (with references/scripts listings). \
Example: activate_skill name=\"rust-review\""
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::skills::activate::ActivateSkillArgs>("activate_skill")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::skills::activate::ActivateSkillArgs>(args) {
            Ok(a) => crate::skills::activate::activate_skill(a, ctx),
            Err(e) => rejected("activate_skill", e),
        }
    }
}

pub struct UndoTool;
#[async_trait]
impl Tool for UndoTool {
    fn name(&self) -> &str {
        "undo"
    }
    fn description(&self) -> &str {
        "Undo or redo file changes committed by this session's edit/write tools, \
based on the Mutation Journal (before/after snapshots recorded on every successful \
edit/write). CAS semantics: a file is restored only if its current content matches \
the journal snapshot; if any file conflicts (externally modified), NOTHING is written. \
Use it to revert your own mistaken edits without git. Does NOT cover changes made \
through bash. Actions: undo (default) / redo; scope: last (default) / all. \
Example: undo; undo action=redo scope=all"
    }
    fn input_schema(&self) -> serde_json::Value {
        crate::tool::schema_value::<crate::tool::undo::UndoArgs>("undo")
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    /// undo 是 journal CAS 回滚（写目标文件，路径在 journal 中）：串行保守。
    fn access_class(&self) -> crate::tool::registry::ToolAccessClass {
        crate::tool::registry::ToolAccessClass::WorkspaceUnknown
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        match serde_json::from_str::<crate::tool::undo::UndoArgs>(args) {
            Ok(a) => crate::tool::undo::undo(a, ctx),
            Err(e) => rejected("undo", e),
        }
    }
}

// ===================================================================
// §11：注册所有 builtin 工具——直接走 Tool trait，不再经过
// BuiltinToolAdapter / ValidatedArgs / execute_builtin 双轨。
// ===================================================================

pub fn register_all_builtin(registry: &mut crate::tool::registry::ToolRegistry) {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool),
        Arc::new(EditTool),
        Arc::new(WriteTool),
        Arc::new(BashTool),
        Arc::new(ProcessTool),
        Arc::new(TerminalTool),
        Arc::new(RequestInputTool),
        Arc::new(RuntimeInspectTool),
        Arc::new(UpdatePlanTool),
        Arc::new(WebSearchTool),
        Arc::new(WebFetchTool),
        Arc::new(ActivateSkillTool),
        Arc::new(UndoTool),
    ];
    for tool in tools {
        let _ = registry.register_validated(tool);
    }
}
