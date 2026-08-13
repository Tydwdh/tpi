//! Remote bash 执行器（§35-§40；SshShellExecutor）。
//!
//! bash 工具按 ActiveWorkspace 分发：Remote → 本执行器。与 Local 共享同一
//! ShellSessionState 语义（§36）：
//!
//! ```text
//! ShellSessionState(v) ──注入──▶ fresh exec channel ──执行──▶ 捕获 cwd/env
//!        │                                                         │
//!        └─────────────────── commit(v+1) ◀───────────────────────┘
//! ```
//!
//! 约束：
//! - §38：每次 fresh exec channel（复用 transport 连接，不持久 shell 进程）；
//! - §39：cancellation 是 best-effort（关闭 channel 通知远端，不伪装 Windows
//!   Job Object 的进程树 guarantee）；
//! - §40：网络中断时命令结果视为未知（不自动 replay，由上层记录 Effect）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::remote::ssh::{ConnectionState, SshClient, SshError};
use crate::tool::command::BashArgs;
use crate::tool::outcome::{ModelPayload, ToolOutcome, ToolStatus};
use crate::tool::ToolContext;

/// Remote bash 入口（§35：SshShellExecutor）。
pub async fn remote_bash(args: BashArgs, ctx: &ToolContext) -> ToolOutcome {
    if args.command.trim().is_empty() {
        return rejected("empty_command", "command 不能为空。");
    }
    if args.timeout_ms == 0 || args.timeout_ms > crate::tool::command::MAX_TIMEOUT_MS {
        return rejected(
            "invalid_timeout",
            &format!(
                "timeout_ms 必须在 1..={} 范围内。",
                crate::tool::command::MAX_TIMEOUT_MS
            ),
        );
    }
    // 从 ActiveWorkspace 取 RemoteWorkspace（bash 分发已确认是 Remote）。
    let (shell, client) = {
        let ws = crate::util::lock_mutex(&ctx.workspace, "workspace");
        match &ws.workspace {
            crate::workspace::Workspace::Remote(remote) => {
                (remote.shell.clone(), remote.client.clone())
            }
            _ => return rejected("not_remote", "当前 workspace 不是 Remote。"),
        }
    };

    // 状态注入（§36：与本地同一语义）。
    let (session_cwd, overlay_set, overlay_unset) = {
        let state = crate::util::lock_mutex(&shell, "shell");
        (
            state.cwd.to_string(),
            state.env_overlay.set.clone(),
            state.env_overlay.unset.clone(),
        )
    };
    let exec_cwd = match &args.cwd {
        Some(path) => path.clone(), // 远端路径按字面（POSIX）
        None => session_cwd.clone(),
    };

    // 远端命令：export/unset/cd 前缀 + 用户命令 + 无条件 capture 段，
    // 保留用户命令的真实 exit code（同本地 wrapper 语义）。
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let mut prefix = String::new();
    for (k, v) in &overlay_set {
        prefix.push_str(&format!("export {k}={}; ", shell_quote(v)));
    }
    for k in &overlay_unset {
        prefix.push_str(&format!("unset {k}; "));
    }
    prefix.push_str(&format!("cd {}; ", shell_quote(&exec_cwd)));
    let capture = format!(
        "printf '\\n__TPI_CAPTURE_BEGIN_{nonce}__\\n'; printf '%s\\n' \"$PWD\"; env; printf '__TPI_CAPTURE_END_{nonce}__\\n'"
    );
    let full = format!(
        "{prefix}{}\n__tpi_s=$?\n{capture}\nexit $__tpi_s",
        args.command
    );

    // §20：首次执行前捕获远端初始环境（baseline），供 diff_env 使用。
    // baseline 只存内存（可能含 secret，不落盘 §21）。
    {
        let need_baseline = {
            let state = crate::util::lock_mutex(&shell, "shell");
            state.baseline.is_none()
        };
        if need_baseline {
            capture_remote_baseline(&shell, &client).await;
        }
    }
    // 执行（未连接则先连接；带取消/超时）。
    let start = Instant::now();
    let result = {
        let mut client = client.lock().await;
        if client.connection_state() != ConnectionState::Connected {
            match client.connect().await {
                Ok(crate::remote::ssh::HostKeyDecision::Accepted) => {}
                Ok(decision) => {
                    return ToolOutcome::failed(
                        "bash",
                        ModelPayload {
                            status: ToolStatus::Failed,
                            program: Some("ssh".into()),
                            exit_code: None,
                            duration_ms: 0,
                            output: format!(
                                "status: failed\ntool: bash\nerror: ssh_host_key\n\nhost key 未确认（{decision:?}）：请先由用户确认后再重试。"
                            ),
                            effect: None,
                            artifact: None,
                        },
                    );
                }
                Err(e) => {
                    return ToolOutcome::failed(
                        "bash",
                        ModelPayload {
                            status: ToolStatus::Failed,
                            program: Some("ssh".into()),
                            exit_code: None,
                            duration_ms: 0,
                            output: format!(
                                "status: failed\ntool: bash\nerror: ssh_connect_failed\n\n{e}"
                            ),
                            effect: None,
                            artifact: None,
                        },
                    );
                }
            }
        }
        let empty_env: HashMap<String, String> = HashMap::new();
        let timeout = tokio::time::timeout(
            std::time::Duration::from_millis(args.timeout_ms),
            client.exec(&full, None, &empty_env, Some(&ctx.cancel)),
        )
        .await;
        match timeout {
            Err(_) => {
                return ToolOutcome::failed(
                    "bash",
                    ModelPayload {
                        status: ToolStatus::TimedOut,
                        program: Some("ssh".into()),
                        exit_code: None,
                        duration_ms: 0,
                        output: "status: timed_out\ntool: bash\nerror: remote_timeout\n\n远端命令超时（best-effort 取消：远端进程可能仍在运行，§39）。".into(),
                        effect: None,
                        artifact: None,
                    },
                );
            }
            Ok(Err(SshError::Exec(e))) if e == "cancelled" => {
                return ToolOutcome::failed(
                    "bash",
                    ModelPayload {
                        status: ToolStatus::Cancelled,
                        program: Some("ssh".into()),
                        exit_code: None,
                        duration_ms: 0,
                        output: "status: cancelled\ntool: bash\nerror: remote_cancelled\n\n用户取消（best-effort：远端进程可能仍在运行，§39）。".into(),
                        effect: None,
                        artifact: None,
                    },
                );
            }
            Ok(Err(e)) => {
                return ToolOutcome::failed(
                    "bash",
                    ModelPayload {
                        status: ToolStatus::Failed,
                        program: Some("ssh".into()),
                        exit_code: None,
                        duration_ms: 0,
                        output: format!(
                            "status: failed\ntool: bash\nerror: ssh_exec_failed\n\n{e}"
                        ),
                        effect: None,
                        artifact: None,
                    },
                );
            }
            Ok(Ok(result)) => result,
        }
    };

    // 剥离 capture 段（control plane 不进模型输出，§22 语义在远端同样适用）。
    let mut scanner = crate::process::capture::CaptureScanner::new(&nonce);
    let mut user_stdout = scanner.feed(&result.stdout);
    user_stdout.extend_from_slice(&scanner.finish());
    let capture = scanner.take_capture();
    let (captured_cwd, captured_env) = match capture {
        Some(c) => parse_capture(&c),
        None => (None, HashMap::new()),
    };

    // 事务 commit（§12-§14 同本地语义）：cwd 或 env 任一变化才递增 version。
    // 远端 cwd 无沙箱边界（ssh 目标即用户自己的机器，§17 不适用）。
    if let Some(new_cwd) = captured_cwd {
        let mut state = crate::util::lock_mutex(&shell, "shell");
        let mut changed = false;
        if new_cwd != exec_cwd {
            state.cwd = camino::Utf8PathBuf::from(new_cwd);
            changed = true;
        }
        if let Some(baseline) = &state.baseline {
            let overlay = crate::shell::diff_env(baseline, &captured_env);
            if overlay != state.env_overlay {
                state.env_overlay = overlay;
                changed = true;
            }
        }
        if changed {
            state.version += 1;
        }
    }

    // 构造 ToolOutcome（有界 tail；完整输出留 R4 artifact）。
    let tool_status = if result.exit_code == Some(0) {
        ToolStatus::Succeeded
    } else {
        ToolStatus::Failed
    };
    let total = user_stdout.len() + result.stderr.len();
    let budget = crate::tool::command::DEFAULT_RUN_MAX_BYTES;
    let mut body = String::new();
    let mut truncated = false;
    push_tail(&mut body, "stdout", &user_stdout, budget, &mut truncated);
    push_tail(&mut body, "stderr", &result.stderr, budget, &mut truncated);
    let output_meta = if truncated {
        format!("truncated ({}/{total} bytes)", budget)
    } else {
        format!("{total} bytes")
    };
    let status_name = status_str(tool_status);
    let exit_text = result
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "none".into());
    let separator = if body.is_empty() { "" } else { "\n" };
    let summary = format!(
        "status: {status_name}\nprogram: ssh\nexit_code: {exit_text}\nduration_ms: {}\noutput: {output_meta}{separator}{body}",
        start.elapsed().as_millis(),
    );
    ToolOutcome::failed(
        "bash",
        ModelPayload {
            status: tool_status,
            program: Some("ssh".into()),
            exit_code: result.exit_code.map(|c| c as i32),
            duration_ms: start.elapsed().as_millis() as u64,
            output: summary,
            effect: None,
            artifact: None,
        },
    )
}

/// 解析 capture 段：第一行 cwd（POSIX 路径，远端不转换）+ 其余 env。
fn parse_capture(capture: &[u8]) -> (Option<String>, HashMap<String, String>) {
    let text = String::from_utf8_lossy(capture);
    let mut lines = text.lines();
    let cwd = lines
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    let mut env = HashMap::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            env.insert(k.to_string(), v.to_string());
        }
    }
    (cwd, env)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 捕获远端初始环境（baseline，§20）：跑一次**不注入 overlay** 的 exec，
/// 只输出 capture 段。结果存入 shell.baseline（仅内存，可能含 secret，
/// 不落盘 §21）。失败（进程异常/capture 无效）只记 warn，下次重试。
async fn capture_remote_baseline(shell: &Arc<std::sync::Mutex<crate::shell::ShellSessionState>>, client: &Arc<tokio::sync::Mutex<SshClient>>) {
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let capture = format!(
        "printf '\\n__TPI_CAPTURE_BEGIN_{nonce}__\\n'; printf '%s\\n' \"$PWD\"; env; printf '__TPI_CAPTURE_END_{nonce}__\\n'"
    );
    let result = {
        let mut client = client.lock().await;
        if client.connection_state() != ConnectionState::Connected {
            match client.connect().await {
                Ok(crate::remote::ssh::HostKeyDecision::Accepted) => {}
                _ => {
                    tracing::warn!("remote baseline capture 未连接；env 跟踪跳过");
                    return;
                }
            }
        }
        let empty: HashMap<String, String> = HashMap::new();
        match client.exec(&capture, None, &empty, None).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "remote baseline capture 执行失败；env 跟踪跳过");
                return;
            }
        }
    };
    // 剥离 capture 段并解析 env。
    let mut scanner = crate::process::capture::CaptureScanner::new(&nonce);
    let _ = scanner.feed(&result.stdout);
    let _ = scanner.finish();
    let Some(capture_bytes) = scanner.take_capture() else {
        tracing::warn!("remote baseline capture 无有效捕获段；env 跟踪跳过");
        return;
    };
    let (_, env) = parse_capture(&capture_bytes);
    let mut state = crate::util::lock_mutex(shell, "shell");
    state.baseline = Some(env);
}

/// 追加一段输出到 `body`（保留尾部；总预算 budget）。
fn push_tail(body: &mut String, name: &str, bytes: &[u8], budget: usize, truncated: &mut bool) {
    if bytes.is_empty() {
        return;
    }
    let keep = bytes.len().min(budget);
    if keep < bytes.len() {
        *truncated = true;
    }
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(&format!("--- {name} ---\n"));
    body.push_str(&String::from_utf8_lossy(&bytes[bytes.len() - keep..]));
}

fn status_str(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Succeeded => "succeeded",
        ToolStatus::Failed => "failed",
        ToolStatus::TimedOut => "timed_out",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Interrupted => "interrupted",
        ToolStatus::Rejected => "rejected",
    }
}

fn rejected(code: &str, detail: &str) -> ToolOutcome {
    ToolOutcome::failed(
        "bash",
        ModelPayload {
            status: ToolStatus::Rejected,
            program: Some("ssh".into()),
            exit_code: None,
            duration_ms: 0,
            output: format!("status: rejected\ntool: bash\nerror: {code}\n\n{detail}"),
            effect: None,
            artifact: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capture_splits_cwd_and_env() {
        let capture = b"/home/dev/project/src\nPATH=/usr/bin\nFOO=bar=baz\n";
        let (cwd, env) = parse_capture(capture);
        assert_eq!(cwd.as_deref(), Some("/home/dev/project/src"));
        assert_eq!(env["PATH"], "/usr/bin");
        assert_eq!(env["FOO"], "bar=baz", "值含 = 按第一个 = 分割");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/home/dev/my dir"), "'/home/dev/my dir'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn push_tail_keeps_tail_and_sets_truncated() {
        let mut body = String::new();
        let mut truncated = false;
        let big = vec![b'x'; 100];
        push_tail(&mut body, "stdout", &big, 10, &mut truncated);
        assert!(truncated);
        assert!(body.ends_with("xxxxxxxxxx"));
    }
}
