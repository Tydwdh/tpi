//! 命令执行工具（文档 §11）：`run`（M1）；`bash` 属 M2。
//!
//! `run` 不做 shell interpolation（§11.1），直接执行 program + args；
//! 状态判定：exit_code==0 → succeeded，非零 → failed；stderr 只是一条输出流，
//! 不能单独决定失败（§11.3）；timeout/cancellation 是独立状态，不伪装成 exit code 1。

use std::collections::HashMap;
use std::time::Duration;

use crate::tool::ToolContext;
use crate::tool::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};
use schemars::JsonSchema;
use serde::Deserialize;

/// `run` 的模型输出预算（§8.4：24 KiB，保留错误相关 tail）。
pub const DEFAULT_RUN_MAX_BYTES: usize = 24 * 1024;
/// 默认超时（120 秒，§11.1 示例）。
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// `bash` 参数（§11.1：只有需要管道、重定向、glob 或复合条件时才使用）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// Bash 命令（Bash 语法；wrapper 统一启用 `set -o pipefail`）。
    pub command: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RunArgs {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 工作目录；默认为 workspace root。
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// 附加环境变量（值按字面传递）。
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_cwd() -> String {
    ".".to_string()
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

pub async fn run(args: RunArgs, ctx: &ToolContext) -> ToolOutcome {
    let program = args.program.clone();
    let timeout = Duration::from_millis(args.timeout_ms.max(1));
    let start = std::time::Instant::now();

    // §11.1：PATH/PATHEXT 解析；`.cmd/.bat` 使用受控 cmd launcher（标记 launcher=cmd-script）。
    let resolved = crate::process::resolver::resolve(&program);
    let (exec_program, exec_args) = match resolved.launcher {
        Some("cmd-script") => {
            let (cmd, args) =
                crate::process::resolver::build_cmd_launcher(&resolved.path, &args.args);
            (cmd, args)
        }
        _ => (
            resolved.path.to_string_lossy().to_string(),
            args.args.clone(),
        ),
    };
    // §11.5：host 的 cwd 是 tpi.exe 启动目录，必须把 cwd 解析为 workspace 绝对路径。
    let exec_cwd = match crate::tool::resolve_workspace_path(&ctx.workspace_root, &args.cwd) {
        Ok(path) => path.to_string(),
        Err(error) => {
            return crate::tool::path_rejected_outcome("run", error);
        }
    };
    let run_args = RunArgs {
        program: exec_program,
        args: exec_args,
        cwd: exec_cwd,
        timeout_ms: args.timeout_ms,
        env: args.env.clone(),
    };

    // §8.4：完整输出写入 artifact；模型只见有界摘要。
    let mut artifact = crate::session::artifact::ArtifactWriter::create(
        &ctx.artifacts_root,
        &ctx.session_id,
        "run",
        "text/plain",
    )
    .ok();

    // §11.5：process-host + Job Object 执行（取消/超时终止整棵进程树）。
    let result = crate::process::run_in_host(
        &run_args,
        &std::path::PathBuf::from(&run_args.program),
        resolved.launcher,
        ctx.cancel.clone(),
        timeout,
        &ctx.session_id,
        artifact.as_mut(),
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
                "run",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: Some(program.clone()),
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: run\nerror: process_isolation_unavailable\n\n{error}"
                    ),
                    effect: None,
                    artifact: artifact_ref,
                },
            )
            .with_metadata(ToolMetadata {
                tool: "run".into(),
                program: Some(program),
                target: Some(args.cwd),
                timeout_ms: Some(args.timeout_ms),
            });
        }
    };

    // §11.3：timeout/cancellation 是独立状态，不伪装成 exit code。
    let tool_status = match result.ended_by {
        crate::process::EndReason::Cancelled => ToolStatus::Cancelled,
        crate::process::EndReason::TimedOut => ToolStatus::TimedOut,
        crate::process::EndReason::Exited => match result.exit_code {
            Some(0) => ToolStatus::Succeeded,
            _ => ToolStatus::Failed,
        },
    };
    let elapsed = start.elapsed();
    let mut outcome = outcome_for(OutcomeInput {
        program: program.clone(),
        args,
        exit_code: result.exit_code,
        elapsed,
        status: tool_status,
        stdout_bytes: &result.stdout,
        stderr_bytes: &result.stderr,
    });
    outcome.artifacts = artifact_ref.into_iter().collect();
    outcome
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
    let result = crate::process::run_in_host(
        &run_args,
        &std::path::PathBuf::from(&run_args.program),
        Some("git-bash"),
        ctx.cancel.clone(),
        timeout,
        &ctx.session_id,
        artifact.as_mut(),
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

    let mut push_stream = |name: &str, bytes: &[u8], budget_left: &mut usize| {
        if bytes.is_empty() {
            return;
        }
        let head = if bytes.len() > *budget_left {
            truncated = true;
            let keep = *budget_left;
            *budget_left = 0;
            &bytes[bytes.len() - keep..]
        } else {
            *budget_left -= bytes.len();
            bytes
        };
        if head.is_empty() {
            return;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("--- {name} ---\n"));
        output.push_str(&String::from_utf8_lossy(head));
    };
    let mut budget_left = budget;
    push_stream("stdout", stdout_bytes, &mut budget_left);
    push_stream("stderr", stderr_bytes, &mut budget_left);

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
            tool: "run".into(),
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
}
