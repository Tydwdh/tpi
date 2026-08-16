//! Agent-facing operations for the persistent PTY registry.

use crate::tool::ToolContext;
use schemars::JsonSchema;
use serde::Deserialize;
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TerminalOpenArgs {
    /// Initial terminal rows; defaults to 24.
    #[serde(default)]
    pub rows: Option<u16>,
    /// Initial terminal columns; defaults to 80.
    #[serde(default)]
    pub cols: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TerminalWriteArgs {
    pub id: String,
    pub data: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TerminalReadArgs {
    pub id: String,
    /// Read only bytes appended after this cursor.
    #[serde(default)]
    pub after: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TerminalResizeArgs {
    pub id: String,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TerminalIdArgs {
    pub id: String,
}

pub async fn open(args: TerminalOpenArgs, ctx: &ToolContext) -> ToolOutcome {
    let shell = ctx
        .shell_path
        .as_ref()
        .map(|path| path.as_str())
        .unwrap_or(if cfg!(windows) { "cmd.exe" } else { "/bin/sh" });
    let workspace =
        match crate::workspace::tracked::TrackedWorkspace::capture(ctx.workspace_root.clone()) {
            Ok(workspace) => workspace,
            Err(error) => {
                return outcome("terminal_open", Err(format!("workspace_tracking: {error}")));
            }
        };
    let result = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry").open_tracked(
        shell,
        ctx.workspace_root.as_std_path(),
        args.rows.unwrap_or(24),
        args.cols.unwrap_or(80),
        workspace,
        ctx.artifacts_root.clone(),
        ctx.session_id.clone(),
    );
    outcome(
        "terminal_open",
        result.map(|id| format!("status: opened\nterminal_id: {id}")),
    )
}

pub async fn write(args: TerminalWriteArgs, ctx: &ToolContext) -> ToolOutcome {
    let mut terminals = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry");
    let result = terminals
        .checkpoint_workspace(&args.id, &ctx.artifacts_root, &ctx.session_id)
        .and_then(|_| terminals.write(&args.id, args.data.as_bytes()))
        .map(|_| format!("status: written\nterminal_id: {}", args.id));
    outcome("terminal_write", result)
}

pub async fn read(args: TerminalReadArgs, ctx: &ToolContext) -> ToolOutcome {
    let mut terminals = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry");
    let result = terminals
        .read(&args.id, args.after.unwrap_or(0))
        .and_then(|read| {
            terminals
                .checkpoint_workspace(&args.id, &ctx.artifacts_root, &ctx.session_id)
                .map(|_| read)
        })
        .map(|read| {
            format!(
                "status: read\nterminal_id: {}\nnext_cursor: {}\ntruncated: {}\nclosed: {}\n{}",
                args.id,
                read.next_cursor,
                read.truncated,
                read.closed,
                String::from_utf8_lossy(&read.data)
            )
        });
    outcome("terminal_read", result)
}

pub async fn resize(args: TerminalResizeArgs, ctx: &ToolContext) -> ToolOutcome {
    let mut terminals = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry");
    let result = terminals
        .checkpoint_workspace(&args.id, &ctx.artifacts_root, &ctx.session_id)
        .and_then(|_| terminals.resize(&args.id, args.rows, args.cols))
        .map(|_| format!("status: resized\nterminal_id: {}", args.id));
    outcome("terminal_resize", result)
}

pub async fn signal(args: TerminalIdArgs, ctx: &ToolContext) -> ToolOutcome {
    let mut terminals = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry");
    let result = terminals
        .signal(&args.id)
        .and_then(|_| {
            terminals.checkpoint_workspace(&args.id, &ctx.artifacts_root, &ctx.session_id)
        })
        .map(|_| format!("status: signalled\nterminal_id: {}", args.id));
    outcome("terminal_signal", result)
}

pub async fn close(args: TerminalIdArgs, ctx: &ToolContext) -> ToolOutcome {
    let mut terminals = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry");
    let result = terminals
        .signal(&args.id)
        .and_then(|_| {
            terminals.checkpoint_workspace(&args.id, &ctx.artifacts_root, &ctx.session_id)
        })
        .and_then(|_| terminals.close(&args.id))
        .map(|_| format!("status: closed\nterminal_id: {}", args.id));
    outcome("terminal_close", result)
}

fn outcome(tool: &str, result: Result<String, String>) -> ToolOutcome {
    match result {
        Ok(text) => ToolOutcome::succeeded(tool, text),
        Err(error) => ToolOutcome::failed(
            tool,
            ModelPayload {
                status: ToolStatus::Rejected,
                program: Some(tool.into()),
                exit_code: None,
                duration_ms: 0,
                output: format!("status: rejected\ntool: {tool}\nerror: {error}"),
                effect: None,
                artifact: None,
            },
        ),
    }
}
