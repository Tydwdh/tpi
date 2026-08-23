//! Agent-facing operations for the persistent PTY registry.
//!
//! §8：六个 terminal_* 工具合并为一个 `terminal` 工具，使用 tagged action schema。
//! 每个 action 只暴露该 action 合法的字段。

use crate::tool::ToolContext;
use schemars::JsonSchema;
use serde::Deserialize;
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};
use tpi_core::resource::{ResourceLifetime, WorkspaceAccess};

/// §8：统一 terminal 工具参数——tagged action schema。
///
/// 每个 action 只暴露该 action 合法的字段；无效组合在反序列化时即被拒绝。
#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TerminalArgs {
    /// 打开一个新的持久化 PTY 终端。
    Open {
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        cols: Option<u16>,
        /// Resource lifetime; owner is assigned by the runtime, never by the
        /// model.
        #[serde(default)]
        lifetime: ResourceLifetime,
    },
    /// 向终端写入原始字节（不做转义解释）。
    Write {
        id: String,
        /// 原始字节写入（原样传输，不做转义解释）。
        data: String,
        /// 写入后追加平台 Enter 字节以触发 shell 执行。
        #[serde(default)]
        submit: bool,
    },
    /// 读取终端输出（增量读取）。
    Read {
        id: String,
        /// Read only bytes appended after this cursor.
        #[serde(default)]
        after: Option<u64>,
    },
    /// 调整终端窗口大小。
    Resize { id: String, rows: u16, cols: u16 },
    /// 向终端发送中断信号（SIGINT 等效）。
    Signal { id: String },
    /// 关闭终端并释放资源。
    Close { id: String },
}

pub async fn terminal(args: TerminalArgs, ctx: &ToolContext) -> ToolOutcome {
    match args {
        TerminalArgs::Open {
            rows,
            cols,
            lifetime,
        } => do_open(rows, cols, lifetime, ctx).await,
        TerminalArgs::Write { id, data, submit } => do_write(&id, &data, submit, ctx).await,
        TerminalArgs::Read { id, after } => do_read(&id, after, ctx).await,
        TerminalArgs::Resize { id, rows, cols } => do_resize(&id, rows, cols, ctx).await,
        TerminalArgs::Signal { id } => do_signal(&id, ctx).await,
        TerminalArgs::Close { id } => do_close(&id, ctx).await,
    }
}

// ---- 内部实现 ----

async fn do_open(
    rows: Option<u16>,
    cols: Option<u16>,
    lifetime: ResourceLifetime,
    ctx: &ToolContext,
) -> ToolOutcome {
    let shell = if cfg!(windows) {
        match crate::tool::command::locate_git_bash(ctx) {
            Some(path) => path,
            None => {
                return outcome(
                    "terminal",
                    Err(
                        "git_bash_not_found: 未找到 Git Bash（§11.2 解析顺序：shell.path → Program Files\\Git\\bin\\bash.exe → usr\\bin → PATH）。可运行 scripts/install-bash.ps1 或配置 shell.path。".into(),
                    ),
                );
            }
        }
    } else {
        "/bin/sh".into()
    };
    let workspace =
        match crate::workspace::tracked::TrackedWorkspace::capture(ctx.workspace_root.clone()) {
            Ok(workspace) => workspace,
            Err(error) => {
                return outcome("terminal", Err(format!("workspace_tracking: {error}")));
            }
        };
    let result = ctx.resource_manager().open_terminal(
        &shell,
        ctx.workspace_root.as_std_path(),
        rows.unwrap_or(24),
        cols.unwrap_or(80),
        workspace,
        ctx.artifacts_root.clone(),
        ctx.session_id.clone(),
        ctx.resource_meta(lifetime, WorkspaceAccess::ExternallyMutable),
    );
    outcome(
        "terminal",
        result.map(|id| format!("action: open\nstatus: opened\nterminal_id: {id}")),
    )
}

/// 原样写入的字节序列。`submit` 时追加一个 Enter（CR 0x0D）。
fn write_bytes(data: &str, submit: bool) -> Vec<u8> {
    let mut bytes = data.as_bytes().to_vec();
    if submit {
        bytes.push(b'\r');
    }
    bytes
}

async fn do_write(id: &str, data: &str, submit: bool, ctx: &ToolContext) -> ToolOutcome {
    let resources = ctx.resource_manager();
    let identity = ctx.resource_identity();
    let bytes = write_bytes(data, submit);
    let result = resources
        .checkpoint_terminal(&identity, id, &ctx.artifacts_root, &ctx.session_id)
        .and_then(|_| resources.write_terminal(&identity, id, &bytes))
        .map(|_| format!("action: write\nstatus: written\nterminal_id: {id}"));
    outcome("terminal", result)
}

async fn do_read(id: &str, after: Option<u64>, ctx: &ToolContext) -> ToolOutcome {
    let resources = ctx.resource_manager();
    let identity = ctx.resource_identity();
    let result = resources
        .read_terminal(&identity, id, after.unwrap_or(0))
        .and_then(|read| {
            resources
                .checkpoint_terminal(&identity, id, &ctx.artifacts_root, &ctx.session_id)
                .map(|_| read)
        })
        .map(|read| {
            format!(
                "action: read\nstatus: read\nterminal_id: {id}\nnext_cursor: {}\ntruncated: {}\nclosed: {}\n{}",
                read.next_cursor,
                read.truncated,
                read.closed,
                String::from_utf8_lossy(&read.data)
            )
        });
    outcome("terminal", result)
}

async fn do_resize(id: &str, rows: u16, cols: u16, ctx: &ToolContext) -> ToolOutcome {
    let resources = ctx.resource_manager();
    let identity = ctx.resource_identity();
    let result = resources
        .checkpoint_terminal(&identity, id, &ctx.artifacts_root, &ctx.session_id)
        .and_then(|_| resources.resize_terminal(&identity, id, rows, cols))
        .map(|_| format!("action: resize\nstatus: resized\nterminal_id: {id}"));
    outcome("terminal", result)
}

async fn do_signal(id: &str, ctx: &ToolContext) -> ToolOutcome {
    let resources = ctx.resource_manager();
    let identity = ctx.resource_identity();
    let result = resources
        .signal_terminal(&identity, id)
        .and_then(|_| {
            resources.checkpoint_terminal(&identity, id, &ctx.artifacts_root, &ctx.session_id)
        })
        .map(|_| format!("action: signal\nstatus: signalled\nterminal_id: {id}"));
    outcome("terminal", result)
}

async fn do_close(id: &str, ctx: &ToolContext) -> ToolOutcome {
    let resources = ctx.resource_manager();
    let identity = ctx.resource_identity();
    let result = resources
        .signal_terminal(&identity, id)
        .and_then(|_| {
            resources.checkpoint_terminal(&identity, id, &ctx.artifacts_root, &ctx.session_id)
        })
        .and_then(|_| resources.close_terminal(&identity, id))
        .map(|_| format!("action: close\nstatus: closed\nterminal_id: {id}"));
    outcome("terminal", result)
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

#[cfg(test)]
mod tests {
    use super::write_bytes;

    #[test]
    fn submit_appends_single_enter_without_mutating_data() {
        // 原样写入：反斜杠/正则/脚本内容一律不改。
        assert_eq!(write_bytes(r"echo \d+", false), b"echo \\d+");
        // submit=true 只追加一个 CR（0x0D），不改写 data 内容。
        assert_eq!(write_bytes("echo ok", true), b"echo ok\r");
        assert_eq!(write_bytes("", true), b"\r");
    }
}
