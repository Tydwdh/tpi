//! 命令执行工具（文档 §11）：`bash`（M2）是唯一执行通道。
//!
//! `bash` 通过随包/系统 Git Bash 执行（§11.1），wrapper 统一 `set -o pipefail`；
//! 状态判定：exit_code==0 → succeeded，非零 → failed；stderr 只是一条输出流，
//! 不能单独决定失败（§11.3）；timeout/cancellation 是独立状态，不伪装成 exit code 1。

use std::collections::HashMap;
use std::time::Duration;

use crate::tool::ToolContext;
use crate::tool::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};
use schemars::JsonSchema;
use serde::Deserialize;

/// 命令输出的模型预算（§8.4：24 KiB，保留错误相关 tail）。
pub const DEFAULT_RUN_MAX_BYTES: usize = 24 * 1024;
/// 默认超时（120 秒，§11.1 示例）。
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// stderr 最小保留预算（§14/BUG-007：失败原因优先于 stdout 刷屏；
/// stdout 灌满总预算时 stderr 仍至少保留这一段）。
pub const STDERR_MIN_BUDGET: usize = 4 * 1024;

/// `bash` 参数（§11.1：唯一执行工具，覆盖程序执行与 shell 复合命令）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// Bash 命令（Bash 语法；wrapper 统一启用 `set -o pipefail`）。
    pub command: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

/// bash 工具内部使用的启动规格（由 `command::bash` 构造，不暴露为工具 schema）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    pub program: String,
    pub args: Vec<String>,
    /// 工作目录；默认为 workspace root。
    pub cwd: String,
    pub timeout_ms: u64,
    /// 附加环境变量（值按字面传递）。
    pub env: HashMap<String, String>,
}

fn default_cwd() -> String {
    ".".to_string()
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// `bash` 工具（§11.2：Git Bash 解析固定顺序；wrapper 统一 `set -o pipefail`）。
pub async fn bash(args: BashArgs, ctx: &ToolContext) -> ToolOutcome {
    let timeout = Duration::from_millis(args.timeout_ms.max(1));
    let start = std::time::Instant::now();
    let bash_exe = locate_git_bash(ctx);
    let Some(bash_exe) = bash_exe else {
        return ToolOutcome::failed(
            "bash",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: failed\ntool: bash\nerror: git_bash_not_found\n\n未找到 Git Bash（§11.2 解析顺序：shell.path → Program Files\\Git\\bin\\bash.exe → usr\\bin → PATH）。".to_string(),
                effect: None,
                artifact: None,
            },
        );
    };

    // §11.1：wrapper 统一启用 pipefail，不要求模型每次重复书写。
    let wrapped = format!(
        "set -o pipefail
{}",
        args.command
    );
    let mut artifact = crate::session::artifact::ArtifactWriter::create(
        &ctx.artifacts_root,
        &ctx.session_id,
        "bash",
        "text/plain",
    )
    .ok();
    let exec_cwd = match crate::tool::resolve_workspace_path(&ctx.workspace_root, &args.cwd) {
        Ok(path) => path.to_string(),
        Err(error) => {
            return crate::tool::path_rejected_outcome("bash", error);
        }
    };
    let run_args = RunArgs {
        program: bash_exe,
        args: vec!["--noprofile".into(), "--norc".into(), "-c".into(), wrapped],
        cwd: exec_cwd,
        timeout_ms: args.timeout_ms,
        env: Default::default(),
    };
    // 实时输出：进程层读帧时转发到 UI 通道（call_id 匹配工具卡片）。
    let stream_sink = ctx.output_tx.as_ref().map(|tx| {
        let call_id = ctx.call_id;
        let tx = tx.clone();
        move |stream: u8, bytes: &[u8]| {
            // BUG-012：有界通道 + try_send——UI 消费慢时丢弃新帧（lossy telemetry），
            // 绝不阻塞进程读循环，也不允许无限堆积。
            let _ = tx.try_send(crate::tool::ToolStreamEvent {
                call_id,
                stream,
                text: String::from_utf8_lossy(bytes).into_owned(),
            });
        }
    });
    let result = crate::process::run_in_host(
        &run_args,
        &std::path::PathBuf::from(&run_args.program),
        Some("git-bash"),
        ctx.cancel.clone(),
        timeout,
        &ctx.session_id,
        artifact.as_mut(),
        stream_sink
            .as_ref()
            .map(|sink| sink as &(dyn Fn(u8, &[u8]) + Sync)),
    )
    .await;
    let record = artifact.and_then(|writer| writer.finish().ok());
    let artifact_ref = record.map(|record| crate::tool::outcome::ArtifactRef {
        session: ctx.session_id.clone(),
        id: record.id,
    });

    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return ToolOutcome::failed(
                "bash",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed
tool: bash
error: process_isolation_unavailable

{error}"
                    ),
                    effect: None,
                    artifact: artifact_ref,
                },
            );
        }
    };
    let tool_status = match result.ended_by {
        crate::process::EndReason::Cancelled => ToolStatus::Cancelled,
        crate::process::EndReason::TimedOut => ToolStatus::TimedOut,
        crate::process::EndReason::Exited => match result.exit_code {
            Some(0) => ToolStatus::Succeeded,
            _ => ToolStatus::Failed,
        },
    };
    let mut outcome = outcome_for(OutcomeInput {
        program: "bash".into(),
        args: RunArgs {
            program: "bash".into(),
            args: vec![],
            cwd: args.cwd.clone(),
            timeout_ms: args.timeout_ms,
            env: Default::default(),
        },
        exit_code: result.exit_code,
        elapsed: start.elapsed(),
        status: tool_status,
        stdout_bytes: &result.stdout,
        stderr_bytes: &result.stderr,
    });
    // §8.4：opaque 引用必须同时进入结构化字段与模型可见文本
    //（模型读完整输出的唯一入口是 `read @artifact/...`）。
    if let Some(reference) = &artifact_ref {
        outcome.model_payload.artifact = Some(reference.clone());
        outcome
            .model_payload
            .output
            .push_str(&format!("\nartifact: {reference}"));
    }
    outcome.artifacts = artifact_ref.into_iter().collect();
    outcome
}

/// Git Bash 定位（§11.2 解析顺序固定且记录实际选择）。
///
/// 顺序：
/// 1. 配置 `shell.path`；
/// 2. 随包 Git Bash：`tpi.exe` 同目录下的 `git/bin/bash.exe`、`git/usr/bin/bash.exe`、
///    `git/bash.exe`、`bash.exe`（便携版安装位置，§11.2 安装说明）；
/// 3. `Program Files\Git\bin\bash.exe`、`usr\bin\bash.exe`；
/// 4. PATH 中的 bash.exe（排除 WSL launcher）。
pub fn locate_git_bash(ctx: &ToolContext) -> Option<String> {
    if let Some(path) = &ctx.shell_path {
        return Some(path.to_string());
    }
    // 随包位置：tpi.exe 同目录的 git 便携版。
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [
            dir.join("git").join("bin").join("bash.exe"),
            dir.join("git").join("usr").join("bin").join("bash.exe"),
            dir.join("git").join("bash.exe"),
            dir.join("bash.exe"),
        ] {
            if is_git_bash(&candidate) {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    let candidates = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ];
    for candidate in candidates {
        if is_git_bash(std::path::Path::new(candidate)) {
            return Some(candidate.to_string());
        }
    }
    // PATH 中的 bash.exe。
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("bash.exe");
        if is_git_bash(&candidate) {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

/// 判定候选是否为 Git Bash 的 bash.exe（排除 WSL launcher）。
///
/// `C:\Windows\system32\bash.exe` 与 WindowsApps 下的 bash.exe 是 Linux 子系统
/// launcher，不是 msys 的 Git Bash；误用时冷启动会卡住或弹窗（§11.2）。
fn is_git_bash(exe: &std::path::Path) -> bool {
    if !exe.is_file() {
        return false;
    }
    let lower = exe.to_string_lossy().to_lowercase();
    !lower.contains("\\system32\\") && !lower.contains("\\windowsapps\\")
}

/// outcome_for 的输入打包（避免 8 参数函数）。
struct OutcomeInput<'a> {
    program: String,
    args: RunArgs,
    exit_code: Option<i32>,
    elapsed: Duration,
    status: ToolStatus,
    stdout_bytes: &'a [u8],
    stderr_bytes: &'a [u8],
}

fn outcome_for(input: OutcomeInput<'_>) -> ToolOutcome {
    let OutcomeInput {
        program,
        args,
        exit_code,
        elapsed,
        status,
        stdout_bytes,
        stderr_bytes,
    } = input;
    let duration_ms = elapsed.as_millis() as u64;
    let total = stdout_bytes.len() + stderr_bytes.len();
    // §8.4：保留错误相关 tail。
    let budget = DEFAULT_RUN_MAX_BYTES;
    let mut output = String::new();
    let mut truncated = false;

    // BUG-007：失败原因（stderr tail）优先级必须高于普通 stdout——
    // stdout 灌满 24 KiB 预算时，stderr 不能因为总预算耗尽而完全消失。
    // 非空 stderr 至少保留 STDERR_MIN_BUDGET；stdout 使用剩余预算，
    // stdout 未用完的部分再返还给 stderr。
    let stderr_guarantee = if stderr_bytes.is_empty() {
        0
    } else {
        STDERR_MIN_BUDGET.min(stderr_bytes.len())
    };
    let mut stdout_left = budget.saturating_sub(stderr_guarantee);
    push_stream(
        "stdout",
        stdout_bytes,
        &mut stdout_left,
        &mut output,
        &mut truncated,
    );
    let mut stderr_budget = stderr_guarantee + stdout_left;
    push_stream(
        "stderr",
        stderr_bytes,
        &mut stderr_budget,
        &mut output,
        &mut truncated,
    );

    let program_display = &program;
    let cwd_line = if args.cwd == "." {
        String::new()
    } else {
        format!("cwd: {}\n", args.cwd)
    };
    let exit_code_text = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "none".into());
    let output_meta = if truncated {
        format!("truncated ({}/{total} bytes)", budget)
    } else {
        format!("{total} bytes")
    };
    let separator = if output.is_empty() { "" } else { "\n" };
    let summary = format!(
        "status: {}\nprogram: {program_display}\n{cwd_line}exit_code: {exit_code_text}\nduration_ms: {duration_ms}\noutput: {output_meta}{separator}{output}",
        status_name(status),
    );

    ToolOutcome {
        status,
        model_payload: ModelPayload {
            status,
            program: Some(program.clone()),
            exit_code,
            duration_ms,
            output: summary,
            effect: None,
            artifact: None,
        },
        display_payload: Default::default(),
        session_metadata: ToolMetadata {
            tool: "bash".into(),
            program: Some(program),
            target: Some(args.cwd),
            timeout_ms: Some(args.timeout_ms),
        },
        evidence: Vec::new(),
        observed_resources: Vec::new(),
        artifacts: Vec::new(),
        timing: crate::tool::outcome::ToolTiming { duration_ms },
    }
}

/// 把一段输出流按预算追加到 `output`（保留尾部；起点对齐 UTF-8 边界）。
/// `budget_left` 是剩余预算（会扣减）；`truncated` 标记是否发生截断。
fn push_stream(
    name: &str,
    bytes: &[u8],
    budget_left: &mut usize,
    output: &mut String,
    truncated: &mut bool,
) {
    if bytes.is_empty() {
        return;
    }
    let keep = bytes.len().min(*budget_left);
    if keep < bytes.len() {
        *truncated = true;
    }
    *budget_left -= keep;
    if keep == 0 {
        return;
    }
    let head = utf8_tail(bytes, keep);
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(&format!("--- {name} ---\n"));
    output.push_str(&String::from_utf8_lossy(head));
}

/// 从 `bytes` 尾部取最多 `max` 字节的窗口，起点推进到 UTF-8 序列起点
/// （避免把多字节字符切出 replacement char；非法字节由 from_utf8_lossy 兜底）。
fn utf8_tail(bytes: &[u8], max: usize) -> &[u8] {
    if bytes.len() <= max {
        return bytes;
    }
    let mut start = bytes.len() - max;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    &bytes[start..]
}
fn status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Succeeded => "succeeded",
        ToolStatus::Failed => "failed",
        ToolStatus::TimedOut => "timed_out",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Interrupted => "interrupted",
        ToolStatus::Rejected => "rejected",
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_name_mapping() {
        assert_eq!(status_name(ToolStatus::Succeeded), "succeeded");
        assert_eq!(status_name(ToolStatus::TimedOut), "timed_out");
    }

    fn run_outcome(stdout: &[u8], stderr: &[u8], status: ToolStatus) -> ToolOutcome {
        outcome_for(OutcomeInput {
            program: "bash".into(),
            args: RunArgs {
                program: "bash".into(),
                args: vec![],
                cwd: ".".into(),
                timeout_ms: 1000,
                env: Default::default(),
            },
            exit_code: Some(1),
            elapsed: std::time::Duration::from_millis(1),
            status,
            stdout_bytes: stdout,
            stderr_bytes: stderr,
        })
    }

    /// BUG-007：stdout 灌满预算时，stderr（失败原因）必须仍然保留。
    #[test]
    fn stderr_survives_when_stdout_fills_budget() {
        let stdout = vec![b'a'; DEFAULT_RUN_MAX_BYTES + 4096];
        let stderr = b"error: something failed\n";
        let outcome = run_outcome(&stdout, stderr, ToolStatus::Failed);
        let output = &outcome.model_payload.output;
        assert!(
            output.contains("--- stderr ---"),
            "stderr 段必须存在: {output}"
        );
        assert!(
            output.contains("error: something failed"),
            "stderr 关键错误必须保留: {output}"
        );
        assert!(output.contains("--- stdout ---"), "stdout tail 也应保留");
    }

    /// BUG-007：无 stderr 时 stdout 使用全部预算（不回归旧行为）。
    #[test]
    fn stdout_gets_full_budget_when_no_stderr() {
        let stdout = vec![b'x'; DEFAULT_RUN_MAX_BYTES + 100];
        let outcome = run_outcome(&stdout, &[], ToolStatus::Succeeded);
        let output = &outcome.model_payload.output;
        assert!(output.contains("--- stdout ---"));
        assert!(!output.contains("--- stderr ---"));
        assert!(output.contains("truncated"), "stdout 应标记截断");
    }

    /// BUG-007：stderr 小于预留预算时完整保留。
    #[test]
    fn small_stderr_kept_in_full() {
        let stdout = vec![b'a'; DEFAULT_RUN_MAX_BYTES + 1000];
        let stderr = b"short error";
        let outcome = run_outcome(&stdout, stderr, ToolStatus::Failed);
        assert!(outcome.model_payload.output.contains("short error"));
    }

    /// BUG-007：尾部窗口起点对齐 UTF-8 边界（不切出 replacement char）。
    #[test]
    fn utf8_tail_never_splits_multibyte_char() {
        // 20000 个“中”= 60000 字节；取 59998 字节窗口 → 起点落在字符中间，
        // 必须推进到字符起点。
        let s = "中".repeat(20_000);
        let bytes = s.as_bytes();
        let tail = utf8_tail(bytes, 59_998);
        assert!(tail.len() <= 59_998);
        assert!(std::str::from_utf8(tail).is_ok(), "窗口必须是合法 UTF-8");
        assert!(!String::from_utf8_lossy(tail).contains('\u{FFFD}'));
    }
}
