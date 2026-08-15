//! `process` 工具（任务书 §5：统一管理 ManagedProcess）。
//!
//! 五个 action（第一版不实现 stdin/PTY/resize/attach/detach/signal arbitrary，
//! 任务书 §5/§68）：
//! - list：列出所有进程；
//! - status：单进程详细状态（state/command/runtime/workspace/tail 摘要）；
//! - output：查看更多最近输出（bounded live tail；完整内容在 artifact）；
//! - wait：最多等 `timeout_ms`，完成返回终态，仍运行返回 running（不是错误）；
//! - cancel：请求取消（drain task 执行 TerminateJobObject，整棵进程树终止）。
//!
//! 模型文本第一行是**进程状态**（running/exited/...），工具本身以 Succeeded
//! 返回（查询/控制成功）——与 bash 的 `status: running` 文本风格一致（§49-§51）。

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::process::managed::{ManagedProcessState, ProcessId, wait_process};
use crate::tool::ToolContext;
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};

/// 默认 wait 上限（§20：建议 5 秒窗口）。
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 5_000;
/// status 摘要附带的 tail 预算（避免模型上下文爆炸，§18）。
const STATUS_TAIL_BUDGET: usize = 4 * 1024;
/// process output 返回的 tail 预算（第一版返回全部 live tail，其本身已 bounded）。
pub const OUTPUT_TAIL_BUDGET: usize = 64 * 1024;

fn default_wait_timeout() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

/// `process` 操作（§5：第一版支持）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProcessAction {
    List,
    Status,
    Output,
    Wait,
    Cancel,
}

/// `process` 参数（§5 示例：`{"id": "p17", "action": "status"}`）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ProcessArgs {
    /// 操作：list / status / output / wait / cancel。
    pub action: ProcessAction,
    /// 逻辑进程 id（`p17`）；list 之外必填。
    #[serde(default)]
    pub id: Option<String>,
    /// wait 的最长等待毫秒（默认 5000，最大 24h）。其余 action 忽略。
    #[serde(default = "default_wait_timeout")]
    pub timeout_ms: u64,
}

/// 进程工具入口。
pub async fn process(args: ProcessArgs, ctx: &ToolContext) -> ToolOutcome {
    if args.timeout_ms > crate::tool::command::MAX_TIMEOUT_MS {
        return rejected(
            "invalid_timeout",
            &format!(
                "timeout_ms 必须在 1..={} 范围内。",
                crate::tool::command::MAX_TIMEOUT_MS
            ),
        );
    }
    match args.action {
        ProcessAction::List => list_processes(ctx),
        ProcessAction::Status => {
            let Some(id) = parse_id(&args) else {
                return rejected("missing_id", "process status 需要 id（如 p17）。");
            };
            status_process(ctx, id)
        }
        ProcessAction::Output => {
            let Some(id) = parse_id(&args) else {
                return rejected("missing_id", "process output 需要 id（如 p17）。");
            };
            output_process(ctx, id)
        }
        ProcessAction::Wait => {
            let Some(id) = parse_id(&args) else {
                return rejected("missing_id", "process wait 需要 id（如 p17）。");
            };
            wait_process_tool(ctx, id, Duration::from_millis(args.timeout_ms)).await
        }
        ProcessAction::Cancel => {
            let Some(id) = parse_id(&args) else {
                return rejected("missing_id", "process cancel 需要 id（如 p17）。");
            };
            cancel_process(ctx, id).await
        }
    }
}

fn parse_id(args: &ProcessArgs) -> Option<ProcessId> {
    args.id
        .as_deref()
        .and_then(|text| text.trim().parse::<ProcessId>().ok())
}

fn list_processes(ctx: &ToolContext) -> ToolOutcome {
    let reg = tpi_core::util::lock_mutex(&ctx.processes, "process_registry");
    let lines: Vec<String> = reg.iter().map(|p| p.status_line()).collect();
    let body = if lines.is_empty() {
        "（当前无 managed process）".to_string()
    } else {
        lines.join("\n")
    };
    ToolOutcome::succeeded(
        "process",
        format!("status: succeeded\ntool: process\naction: list\nprocesses:\n{body}"),
    )
}

fn status_process(ctx: &ToolContext, id: ProcessId) -> ToolOutcome {
    let reg = tpi_core::util::lock_mutex(&ctx.processes, "process_registry");
    let Some(process) = reg.get(id) else {
        return not_found(id);
    };
    let runtime = match process.finished_at {
        Some(end) => end.duration_since(process.started_at),
        None => process.started_at.elapsed(),
    };
    let tail = tail_text(&process.tail, STATUS_TAIL_BUDGET);
    let mut out = String::new();
    out.push_str(&format!(
        "status: {}\nprocess_id: {}\ncommand: {}\nworkspace: {}\nruntime: {:.1}s\nexit_code: {}\noutput: {} bytes (live tail)",
        process.state.name(),
        process.id,
        process.command,
        process.workspace,
        runtime.as_secs_f64(),
        process.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
        process.total_bytes,
    ));
    if let Some(reference) = &process.artifact {
        out.push_str(&format!("\nartifact: {reference}"));
    }
    if !tail.is_empty() {
        out.push_str(&format!("\n--- tail ---\n{tail}"));
    }
    ToolOutcome::succeeded("process", out)
}

fn output_process(ctx: &ToolContext, id: ProcessId) -> ToolOutcome {
    let reg = tpi_core::util::lock_mutex(&ctx.processes, "process_registry");
    let Some(process) = reg.get(id) else {
        return not_found(id);
    };
    let tail = tail_text(&process.tail, OUTPUT_TAIL_BUDGET);
    let mut out = format!(
        "status: {}\nprocess_id: {}\noutput: {} bytes (live tail of {})",
        process.state.name(),
        process.id,
        process.tail.len(),
        process.total_bytes,
    );
    if let Some(reference) = &process.artifact {
        out.push_str(&format!("\nartifact: {reference}"));
    }
    if !tail.is_empty() {
        out.push_str(&format!("\n--- output ---\n{tail}"));
    } else {
        out.push_str("\n（尚无输出；进程结束时完整输出进入 artifact）");
    }
    ToolOutcome::succeeded("process", out)
}

/// wait（任务书 §20）：最多等 `timeout`；完成返回终态，仍运行返回 running。
async fn wait_process_tool(ctx: &ToolContext, id: ProcessId, timeout: Duration) -> ToolOutcome {
    let Some(state) = wait_process(&ctx.processes, id, timeout).await else {
        return not_found(id);
    };
    let reg = tpi_core::util::lock_mutex(&ctx.processes, "process_registry");
    let process = reg.get(id);
    let runtime = process
        .map(|p| match p.finished_at {
            Some(end) => end.duration_since(p.started_at),
            None => p.started_at.elapsed(),
        })
        .unwrap_or_default();
    let exit_code = process.and_then(|p| p.exit_code);
    let mut out = format!(
        "status: {}\nprocess_id: {}\nruntime: {:.1}s",
        state.name(),
        id,
        runtime.as_secs_f64()
    );
    if let Some(code) = exit_code {
        out.push_str(&format!("\nexit_code: {code}"));
    }
    if let Some(reference) = process.and_then(|p| p.artifact.clone()) {
        out.push_str(&format!("\nartifact: {reference}"));
    }
    if !state.is_terminal() {
        out.push_str("\n（仍在运行；需要时再次 wait 或 status）");
    }
    ToolOutcome::succeeded("process", out)
}

/// cancel（任务书 §22）：请求取消 → drain task 执行 TerminateJobObject。
/// 等待状态实际迁移（最多 3 秒）后返回真实结果；无法确认时如实 Unknown。
async fn cancel_process(ctx: &ToolContext, id: ProcessId) -> ToolOutcome {
    let requested = tpi_core::util::lock_mutex(&ctx.processes, "process_registry").cancel(id);
    if !requested {
        return not_found_or_ended(id);
    }
    // 等待 drain task 完成终止并迁移状态（TerminateJobObject + EOF + 状态补丁）。
    let state = wait_process(&ctx.processes, id, Duration::from_secs(3)).await;
    match state {
        Some(ManagedProcessState::Cancelled) => ToolOutcome::succeeded(
            "process",
            format!(
                "status: cancelled\nprocess_id: {id}\neffect: process_tree_terminated\n\n进程树已终止（TerminateJobObject）。"
            ),
        ),
        Some(state) => {
            // 终止已发生但状态分类不同（如进程恰在取消窗口内自行退出）。
            ToolOutcome::succeeded(
                "process",
                format!(
                    "status: cancelled\nprocess_id: {id}\nobserved_state: {}\neffect: process_tree_terminated",
                    state.name()
                ),
            )
        }
        None => not_found(id),
    }
}

fn tail_text(tail: &[u8], budget: usize) -> String {
    if tail.is_empty() {
        return String::new();
    }
    let keep = tail.len().min(budget);
    let head = &tail[tail.len() - keep..];
    // 对齐 UTF-8 边界，避免截出 replacement char。
    let mut start = 0;
    while start < head.len() && (head[start] & 0xC0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&head[start..]).into_owned()
}

fn not_found(id: ProcessId) -> ToolOutcome {
    rejected(
        "not_found",
        &format!("进程 {id} 不存在。用 `process` action=list 查看当前 managed processes。"),
    )
}

fn not_found_or_ended(id: ProcessId) -> ToolOutcome {
    rejected(
        "not_running",
        &format!("进程 {id} 不存在或已结束，无法取消。用 `process` action=list 查看。"),
    )
}

fn rejected(code: &str, detail: &str) -> ToolOutcome {
    ToolOutcome::failed(
        "process",
        ModelPayload {
            status: ToolStatus::Rejected,
            program: Some("process".into()),
            exit_code: None,
            duration_ms: 0,
            output: format!("status: rejected\ntool: process\nerror: {code}\n\n{detail}"),
            effect: None,
            artifact: None,
        },
    )
}
