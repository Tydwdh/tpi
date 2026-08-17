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
    /// 原始字节写入（原样传输，不做转义解释）——含反斜杠的命令/正则/脚本
    /// 原样保留。需要让 shell 执行时用 `submit`，由 TPI 在 PTY 边界追加 Enter。
    pub data: String,
    /// 写入后立即在原 PTY 边界追加一个平台适用的 Enter 字节（交互终端规范行
    /// 结束符）。这样把"原样输入"与"提交执行"解耦，模型无需用 \r 猜测换行。
    #[serde(default)]
    pub submit: bool,
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
    // PTY 终端与 `bash` 工具保持一致：Windows 下定位 Git Bash（§11.2 解析顺序：
    // shell.path → 随包 Git → Program Files → PATH），而非 cmd.exe。这样终端与
    // 日常命令执行通道同源，避免 cmd/bash 语法分裂。非 Windows 回退 /bin/sh。
    let shell = if cfg!(windows) {
        match crate::tool::command::locate_git_bash(ctx) {
            Some(path) => path,
            None => {
                return outcome(
                    "terminal_open",
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
                return outcome("terminal_open", Err(format!("workspace_tracking: {error}")));
            }
        };
    let result = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry").open_tracked(
        &shell,
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

/// 原样写入的字节序列。`submit` 时追加一个 Enter（交互 TTY 规范模式行结束
/// 符 CR 0x0D）以触发 shell 执行；`data` 本身不做任何转义解释。把"输入"与
/// "提交"解耦，模型无需用 \r/\n 猜测换行。
fn write_bytes(data: &str, submit: bool) -> Vec<u8> {
    let mut bytes = data.as_bytes().to_vec();
    if submit {
        bytes.push(b'\r');
    }
    bytes
}

pub async fn write(args: TerminalWriteArgs, ctx: &ToolContext) -> ToolOutcome {
    let mut terminals = tpi_core::util::lock_mutex(&ctx.terminals, "terminal_registry");
    let bytes = write_bytes(&args.data, args.submit);
    let result = terminals
        .checkpoint_workspace(&args.id, &ctx.artifacts_root, &ctx.session_id)
        .and_then(|_| terminals.write(&args.id, &bytes))
        .map(|_| format!("status: written\nterminal_id: {}", args.id));
    outcome("terminal_write", result)
}

#[cfg(test)]
mod tests {
    use super::write_bytes;

    #[test]
    fn submit_appends_single_enter_without_mutating_data() {
        // 原样写入：反斜杠/正则/脚本内容一律不改（不被误解释为控制字节）。
        assert_eq!(write_bytes(r"echo \d+", false), b"echo \\d+");
        // submit=true 只追加一个 CR（0x0D），不改写 data 内容。
        assert_eq!(write_bytes("echo ok", true), b"echo ok\r");
        assert_eq!(write_bytes("", true), b"\r");
    }
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
