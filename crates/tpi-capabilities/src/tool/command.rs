//! 命令执行工具：`bash` 是唯一执行通道。
//!
//! `bash` 通过随包/系统 Git Bash 执行（§11.1），wrapper 统一 `set -o pipefail`；
//! 状态判定：exit_code==0 → succeeded，非零 → failed；stderr 只是一条输出流，
//! 不能单独决定失败（§11.3）；timeout/cancellation 是独立状态，不伪装成 exit code 1。

use std::collections::HashMap;
use std::time::Duration;

use crate::tool::ToolContext;
use schemars::JsonSchema;
use serde::Deserialize;
use tpi_core::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

/// 命令输出的模型预算（§8.4：24 KiB，保留错误相关 tail）。
pub const DEFAULT_RUN_MAX_BYTES: usize = 24 * 1024;
/// 默认超时（120 秒，§11.1 示例）。
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;
pub const MAX_COMMAND_BYTES: usize = 1024 * 1024;
/// stderr 最小保留预算（§14/BUG-007：失败原因优先于 stdout 刷屏；
/// stdout 灌满总预算时 stderr 仍至少保留这一段）。
pub const STDERR_MIN_BUDGET: usize = 4 * 1024;

/// `bash` 参数（§11.1：唯一执行工具，覆盖程序执行与 shell 复合命令）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// Bash 命令（Bash 语法；wrapper 统一启用 `set -o pipefail`）。
    /// 示例："cargo test"、"git status"、"python -c \"print(1)\""。
    /// 注意：`sed -i` / `perl -i` 等就地修改会被拦截（改用 edit/write）；
    /// 流处理 sed（无 -i）与重定向写（> file）不受影响。
    pub command: String,
    /// 工作目录（可选，任务书 §15/§16）：未传 → 使用当前逻辑 shell cwd
    /// （`cd` 跨调用保持）；显式传入 → 仅本次 invocation 生效的 override，
    /// 不改变 session cwd（除非命令自身执行了 `cd`）。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 超时毫秒（默认 120000，上限 24h）。长任务（构建/测试）显式调大。
    /// 仅 foreground 生效；`background=true` 时忽略（后台无默认短 timeout，
    /// 任务书 §45/§46）。
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// 后台模式（任务书 §3/§4）：默认 false = foreground（等待退出）；
    /// true = 启动由 TPI 拥有的 ManagedProcess，立即返回逻辑 ProcessId，
    /// 之后用 `process` 工具管理（status/output/wait/cancel）。
    /// 不要用 shell `&`/`nohup` 代替本字段。
    #[serde(default)]
    pub background: bool,
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
    /// 需从 target 环境中移除的变量（§S3：unset 注入）。
    pub env_remove: Vec<String>,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// `bash` 工具（§11.2：Git Bash 解析固定顺序；wrapper 统一 `set -o pipefail`）。
pub async fn bash(args: BashArgs, ctx: &ToolContext) -> ToolOutcome {
    if args.command.trim().is_empty() {
        return rejected_bash("empty_command", "command 不能为空。");
    }
    if args.command.len() > MAX_COMMAND_BYTES {
        return rejected_bash(
            "command_too_large",
            format!("command 最多 {MAX_COMMAND_BYTES} 字节。"),
        );
    }
    if args.timeout_ms == 0 || args.timeout_ms > MAX_TIMEOUT_MS {
        return rejected_bash(
            "invalid_timeout",
            format!("timeout_ms 必须在 1..={MAX_TIMEOUT_MS} 范围内。"),
        );
    }
    // 就地修改拦截（in-place edit guard）：`sed -i` / `perl -i` 等与 edit/write
    // 职责重叠却绕过 revision 校验与 diff 记录，统一在入口拒绝（覆盖
    // foreground/background/remote 全部路径，无需在各执行器重复）。
    if let Some(match_text) = find_in_place_edit(&args.command) {
        return rejected_bash(
            "in_place_edit",
            format!(
                "命令包含就地修改（{match_text}），这类操作会绕过 edit/write 的 revision 校验与 diff 记录，已被拦截。\n请改用 edit（局部替换）或 write（整文件重写）；需要查看内容用 read。\n如需无副作用的流处理（如 `sed -n '1,5p' file` 只读输出），去掉 -i 即可正常执行。"
            ),
        );
    }
    // §35：bash 按 ActiveWorkspace 分发。当前只有 Local（R1 加 Remote 分支
    // → SshShellExecutor）。
    let kind = {
        let ws = tpi_core::util::lock_mutex(&ctx.workspace, "workspace");
        ws.kind()
    };
    match kind {
        crate::workspace::WorkspaceKind::Local => {
            if args.background {
                local_bash_background(args, ctx).await
            } else {
                // §新架构：使用 WorkspaceManager 增量 tracking。
                // 如果 workspace_session 不可用（测试/doctor 等），
                // 跳过 tracking 但仍执行命令（BestEffort 语义）。
                let ws_session = ctx.workspace_session.clone();
                let outcome = local_bash(args, ctx).await;
                // After command execution, reconcile workspace mutations via
                // the new incremental system (if available).
                if let Some(session) = ws_session {
                    let cause = tpi_core::workspace::transaction::MutationCause::Command {
                        command_id: uuid::Uuid::now_v7().to_string(),
                    };
                    if let Err(error) =
                        session.reconcile_after_execution(cause, ctx.workspace_root.as_std_path())
                    {
                        tracing::warn!(%error, "workspace reconcile failed after bash");
                    }
                }
                outcome
            }
        }
        crate::workspace::WorkspaceKind::Remote => {
            if args.background {
                ToolOutcome::failed(
                    "bash",
                    ModelPayload {
                        status: ToolStatus::Rejected,
                        program: Some("ssh".into()),
                        exit_code: None,
                        duration_ms: 0,
                        output: "status: rejected\ntool: bash\nerror: remote_background_unsupported\n\nRemote ManagedProcess 尚未实现（任务书 §62 Phase P8：先由当前 SSH backend 实际能力定义 guarantee）。".into(),
                        effect: None,
                        artifact: None,
                    },
                )
            } else {
                crate::remote::executor::remote_bash(args, ctx).await
            }
        }
    }
}

/// 本地执行器（LocalShellExecutor，§35）：fresh Git Bash + process-host +
/// Job Object 进程树；ShellSessionState 由 ctx.shell 读写（= ActiveWorkspace
/// 内 LocalWorkspace.shell 的同一状态）。
async fn local_bash(args: BashArgs, ctx: &ToolContext) -> ToolOutcome {
    if ctx.cancel.is_cancelled() {
        return ToolOutcome::failed(
            "bash",
            ModelPayload {
                status: ToolStatus::Cancelled,
                program: Some("bash".into()),
                exit_code: None,
                duration_ms: 0,
                output: "status: cancelled\ntool: bash\nerror: cancelled".into(),
                effect: None,
                artifact: None,
            },
        );
    }
    let timeout = Duration::from_millis(args.timeout_ms);
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
    // §22：命令后追加高熵 nonce 包裹的状态捕获段（control plane）——
    // 捕获执行后的真实 cwd（cygpath -w 转 Windows 路径，可作下次 current_dir）；
    // 读端（process 层）剥离，不进模型输出/artifact/UI。
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let wrapped = format!(
        "set -o pipefail
{}
__tpi_status=$?
printf '\\n__TPI_CAPTURE_BEGIN_{nonce}__\\n'
printf '%s\\n' \"$(cygpath -w \"$PWD\" 2>/dev/null || printf '%s' \"$PWD\")\"
env
printf '__TPI_CAPTURE_END_{nonce}__\\n'
exit $__tpi_status",
        args.command
    );
    // 执行起点（任务书 §15/§16）：未传 cwd → 逻辑 shell cwd；显式 → 本次 override。
    let session_cwd = {
        let state = tpi_core::util::lock_mutex(&ctx.shell, "shell");
        state.cwd.clone()
    };
    let exec_cwd = match &args.cwd {
        Some(path) => match crate::tool::resolve_tool_path(ctx, path) {
            Ok(path) => path.to_string(),
            Err(error) => return crate::tool::path_rejected_outcome("bash", error),
        },
        None => session_cwd.to_string(),
    };
    // §20：首次执行前捕获 Workspace 初始环境（baseline）。之后每次
    // diff(baseline, new) 得到 overlay。baseline 只存内存（可能含 secret，
    // 不落盘 §21）；捕获失败则跳过 env 跟踪（cwd 仍工作），下次重试。
    {
        let need_baseline = {
            let state = tpi_core::util::lock_mutex(&ctx.shell, "shell");
            state.baseline.is_none()
        };
        if need_baseline {
            capture_baseline(ctx, &bash_exe, &exec_cwd).await;
        }
    }
    let mut artifact = match tpi_session::artifact::ArtifactWriter::create(
        &ctx.artifacts_root,
        &ctx.session_id,
        "bash",
        "text/plain",
    ) {
        Ok(writer) => writer,
        Err(error) => {
            return ToolOutcome::failed(
                "bash",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: Some("bash".into()),
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: bash\nerror: artifact_create_failed\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    // overlay 注入（§S3）：set → env，unset → env_remove（process-host 移除）。
    let (overlay_set, overlay_unset) = {
        let state = tpi_core::util::lock_mutex(&ctx.shell, "shell");
        (
            state.env_overlay.set.clone(),
            state.env_overlay.unset.clone(),
        )
    };
    let run_args = RunArgs {
        program: bash_exe,
        args: vec!["--noprofile".into(), "--norc".into(), "-c".into(), wrapped],
        cwd: exec_cwd.clone(),
        timeout_ms: args.timeout_ms,
        env: overlay_set,
        env_remove: overlay_unset.into_iter().collect(),
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
    let resolved_program = std::path::PathBuf::from(&run_args.program);
    let result = crate::process::run_in_host(crate::process::HostRunRequest {
        args: &run_args,
        resolved_program: &resolved_program,
        launcher: Some("git-bash"),
        cancel: ctx.cancel.clone(),
        timeout,
        output_budget: crate::process::OUTPUT_BUDGET,
        artifact: Some(&mut artifact),
        stream_sink: stream_sink
            .as_ref()
            .map(|sink| sink as &(dyn Fn(u8, &[u8]) + Sync)),
        capture_nonce: Some(&nonce),
    })
    .await;
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
error: process_execution_failed

{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    // §12-§14 事务：仅正常结束（Exited，无论 exit code）且捕获有效时 commit。
    // timeout / cancellation / capture 失败 → discard，保持 last confirmed 状态。
    // §15/§16：只有命令**实际改变**了 cwd（终点 != 执行起点）才 commit——
    // 显式 cwd override 的调用（如 `pwd` 在 override 目录执行）不得把 override
    // 的执行终点误写成 session cwd；命令自身 `cd` 才构成 state mutation。
    // §20：env 用 diff(baseline, new) 得到新 overlay；无 baseline（捕获失败）
    // 时跳过 env 更新。cwd 或 env 任一变化才递增 version。
    if result.ended_by == crate::process::EndReason::Exited
        && let Some(capture) = result.capture.as_deref()
    {
        let (new_cwd, captured_env) = parse_capture(capture);
        let mut state = tpi_core::util::lock_mutex(&ctx.shell, "shell");
        let mut changed = false;
        if let Some(new_cwd) = new_cwd {
            let changed_cwd = norm_path_for_compare(&new_cwd) != norm_path_for_compare(&exec_cwd);
            if changed_cwd {
                match validate_session_cwd(ctx, &new_cwd) {
                    Ok(validated) => {
                        state.cwd = validated;
                        changed = true;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "shell cwd 越界；保持 last confirmed cwd");
                    }
                }
            }
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
    let artifact_result = artifact.finish();
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
            cwd: exec_cwd.clone(),
            timeout_ms: args.timeout_ms,
            env: Default::default(),
            env_remove: Vec::new(),
        },
        exit_code: result.exit_code,
        elapsed: start.elapsed(),
        status: tool_status,
        stdout_bytes: &result.stdout,
        stderr_bytes: &result.stderr,
    });
    let artifact_ref = match artifact_result {
        Ok(record) => Some(tpi_core::outcome::ArtifactRef {
            session: ctx.session_id.clone(),
            id: record.id,
        }),
        Err(error) => {
            let original_status = status_name(outcome.status);
            outcome.status = ToolStatus::Failed;
            outcome.model_payload.status = ToolStatus::Failed;
            outcome.model_payload.output = format!(
                "status: failed\ntool: bash\nerror: artifact_finalize_failed\noriginal_status: {original_status}\n\n{error}"
            );
            None
        }
    };
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

fn rejected_bash(code: &str, detail: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::failed(
        "bash",
        ModelPayload {
            status: ToolStatus::Rejected,
            program: Some("bash".into()),
            exit_code: None,
            duration_ms: 0,
            output: format!("status: rejected\ntool: bash\nerror: {code}\n\n{detail}"),
            effect: None,
            artifact: None,
        },
    )
}

/// 检测命令中的「就地修改」（in-place edit）模式：`sed -i` / `perl -i` 等。
///
/// 这类操作与 `edit`/`write` 的职责完全重叠，却绕过 revision 校验与 diff
/// 记录（§10.3 写路径一致性），因此静态拦截并引导改用专用工具。
/// 只拦截「就地修改已存在文件」：无 `-i` 的流处理 sed（只读管道）与
/// 重定向写（`> file`，有合法用途）不受影响。
fn find_in_place_edit(command: &str) -> Option<String> {
    for segment in split_command_segments(command) {
        let Some(program) = segment.first() else {
            continue;
        };
        // 兼容路径形式（/usr/bin/sed）与 Windows 扩展名（sed.exe）。
        let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
        let base = name.strip_suffix(".exe").unwrap_or(name);
        if base != "sed" && base != "perl" {
            continue;
        }
        for arg in segment.iter().skip(1) {
            if arg == "--" {
                // `--` 之后是文件名而非选项（GNU 约定）。
                break;
            }
            if is_in_place_flag(arg, base) {
                return Some(format!("{base} {arg}"));
            }
        }
    }
    None
}

/// 判定单个参数是否为就地修改 flag。
/// - sed：`-i`、`-i<后缀>`、`--in-place`、`--in-place=<后缀>`（GNU sed）。
/// - perl：`-i`、`-i<后缀>`，以及组合选项 `-p`/`-n` + `i`（`-pi`、`-npi.bak`）。
///   `-I`（大写，perl 库路径）与 grep 的 `-i`（ignore case）不受影响——
///   程序名白名单已限定只有 sed/perl 进入此判定。
fn is_in_place_flag(arg: &str, program: &str) -> bool {
    match program {
        "sed" => {
            arg == "-i"
                || arg.starts_with("-i.")
                || arg == "--in-place"
                || arg.starts_with("--in-place=")
        }
        "perl" => {
            let Some(rest) = arg.strip_prefix('-') else {
                return false;
            };
            let rest = rest.trim_start_matches(['p', 'n']);
            rest == "i" || rest.starts_with("i.")
        }
        _ => false,
    }
}

/// 把命令静态切分为「命令段」：每个段 = 程序名 + 其参数。
///
/// 分隔符：`;` `&&` `||` `|` `&` `(` `)` 换行——管道/顺序/后台/子 shell
/// 连接的每个程序独立成段，因此 `cat f | sed -i ...` 这类管道内的就地
/// 修改也能命中。引号（`'`/`"`）与反斜杠转义内的内容作为整体 token 保留，
/// 不会被误判为命令名（`echo "sed -i"` 不命中）。
fn split_command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    token.push(c);
                }
            }
            '"' => {
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    token.push(c);
                }
            }
            '\\' => {
                token.push('\\');
                if let Some(next) = chars.next() {
                    token.push(next);
                }
            }
            ';' | '|' | '&' | '(' | ')' | '\n' => {
                flush_token(&mut token, &mut current);
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
            c if c.is_whitespace() => {
                flush_token(&mut token, &mut current);
            }
            _ => token.push(ch),
        }
    }
    flush_token(&mut token, &mut current);
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn flush_token(token: &mut String, current: &mut Vec<String>) {
    if !token.is_empty() {
        current.push(std::mem::take(token));
    }
}

/// 本地后台执行器（P2，任务书 §56）：`bash(background=true)`。
///
/// 只读取 ShellSessionState 快照（cwd + env overlay）构造启动规格，
/// **绝不 commit 回 ShellSessionState**（§10/§44 硬不变量：background 不修改
/// session 状态，多个后台进程之间不可能竞争写 cwd/env）。
///
/// 返回：`status: running` + 逻辑 ProcessId（任务书 §49/§51 文本格式）；
/// 工具调用本身很快完成（start succeeded），进程仍在后台运行（§50 分离）。
async fn local_bash_background(args: BashArgs, ctx: &ToolContext) -> ToolOutcome {
    if ctx.cancel.is_cancelled() {
        return ToolOutcome::failed(
            "bash",
            ModelPayload {
                status: ToolStatus::Cancelled,
                program: Some("bash".into()),
                exit_code: None,
                duration_ms: 0,
                output: "status: cancelled\ntool: bash\nerror: cancelled".into(),
                effect: None,
                artifact: None,
            },
        );
    }
    let Some(bash_exe) = locate_git_bash(ctx) else {
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
    // 启动时快照（§9：process creation 时继承；之后 session 变化不影响已运行进程）。
    let session_cwd = {
        let state = tpi_core::util::lock_mutex(&ctx.shell, "shell");
        state.cwd.clone()
    };
    let exec_cwd = match &args.cwd {
        Some(path) => match crate::tool::resolve_tool_path(ctx, path) {
            Ok(path) => path.to_string(),
            Err(error) => return crate::tool::path_rejected_outcome("bash", error),
        },
        None => session_cwd.to_string(),
    };
    let (overlay_set, overlay_unset) = {
        let state = tpi_core::util::lock_mutex(&ctx.shell, "shell");
        (
            state.env_overlay.set.clone(),
            state.env_overlay.unset.clone(),
        )
    };
    let workspace_id = {
        let ws = tpi_core::util::lock_mutex(&ctx.workspace, "workspace");
        ws.id().to_string()
    };
    let workspace_tracker =
        match crate::workspace::tracked::TrackedWorkspace::capture(ctx.workspace_root.clone()) {
            Ok(snapshot) => snapshot,
            Err(error) => return rejected_bash("workspace_tracking", error),
        };
    // background 命令原样执行（无 capture wrapper：不捕获、不 commit §10）。
    let run_args = RunArgs {
        program: bash_exe,
        args: vec![
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            args.command.clone(),
        ],
        cwd: exec_cwd,
        // §45/§46：background 无默认短 timeout（进程寿命由 lifecycle 管理）。
        timeout_ms: 0,
        env: overlay_set,
        env_remove: overlay_unset.into_iter().collect(),
    };
    let request = crate::process::managed::BackgroundStartRequest {
        args: run_args,
        launcher: Some("git-bash"),
        workspace: workspace_id.clone(),
        command: args.command.clone(),
        artifacts_root: ctx.artifacts_root.clone(),
        session_id: ctx.session_id.clone(),
        workspace_tracker: Some(workspace_tracker),
        registry: ctx.processes.clone(),
    };
    match crate::process::managed::start_background(request).await {
        Ok(id) => ToolOutcome::succeeded(
            "bash",
            format!(
                "status: running\nprocess_id: {id}\ncommand: {}\nworkspace: {}\n\nThe process was started successfully and continues in the background. Use `process` to inspect, wait for, or cancel it.",
                args.command, workspace_id
            ),
        ),
        Err(message) => ToolOutcome::failed(
            "bash",
            ModelPayload {
                status: ToolStatus::Failed,
                program: Some("bash".into()),
                exit_code: None,
                duration_ms: 0,
                output: message,
                effect: None,
                artifact: None,
            },
        ),
    }
}

/// 解析状态捕获段（任务书 §22）：第一行 = cwd（cygpath -w 输出的 Windows
/// 绝对路径；cygpath 不可用时为 bash `PWD`，msys POSIX 路径如 `/c/foo`，
/// 此处转回 Windows 风格供下次 current_dir 使用）；其余行 = `env` 输出的
/// `KEY=value`（value 可能含 `=`，按第一个 `=` 分割）。
fn parse_capture(capture: &[u8]) -> (Option<String>, std::collections::HashMap<String, String>) {
    let text = String::from_utf8_lossy(capture);
    let mut lines = text.lines();
    let cwd = lines
        .next()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(msys_path_to_windows);
    let mut env = std::collections::HashMap::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            env.insert(key.to_string(), value.to_string());
        }
    }
    (cwd, env)
}

/// 捕获 Workspace 初始环境（baseline，§20）：跑一次**不注入 overlay** 的
/// fresh bash，只输出 env（控制段剥离，不进模型/artifact）。结果存入
/// `ctx.shell.baseline`（仅内存，可能含 secret，不落盘 §21）。
/// 失败（进程异常/capture 无效）只记 warn，不阻塞调用方；下次 bash 重试。
async fn capture_baseline(ctx: &ToolContext, bash_exe: &str, cwd: &str) {
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    // 与用户命令 wrapper 的 capture 格式完全一致（第一行 cwd，其余 env），
    // 否则 parse_capture 会把 env 首行误当 cwd 导致 baseline 缺变量（§20）。
    let wrapped = format!(
        "printf '\\n__TPI_CAPTURE_BEGIN_{nonce}__\\n'
printf '%s\\n' \"$(cygpath -w \"$PWD\" 2>/dev/null || printf '%s' \"$PWD\")\"
env
printf '__TPI_CAPTURE_END_{nonce}__\\n'"
    );
    let run_args = RunArgs {
        program: bash_exe.to_string(),
        args: vec!["--noprofile".into(), "--norc".into(), "-c".into(), wrapped],
        cwd: cwd.to_string(),
        timeout_ms: DEFAULT_TIMEOUT_MS,
        env: Default::default(),
        env_remove: Vec::new(),
    };
    let resolved = std::path::PathBuf::from(&run_args.program);
    let result = crate::process::run_in_host(crate::process::HostRunRequest {
        args: &run_args,
        resolved_program: &resolved,
        launcher: Some("git-bash"),
        cancel: ctx.cancel.clone(),
        timeout: std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS),
        output_budget: crate::process::OUTPUT_BUDGET,
        artifact: None,
        stream_sink: None,
        capture_nonce: Some(&nonce),
    })
    .await;
    let Ok(result) = result else {
        tracing::warn!("baseline capture 执行失败；env 跟踪跳过（cwd 仍工作）");
        return;
    };
    if result.ended_by != crate::process::EndReason::Exited {
        tracing::warn!("baseline capture 未正常结束；env 跟踪跳过");
        return;
    }
    let Some(capture) = result.capture.as_deref() else {
        tracing::warn!("baseline capture 无有效捕获段；env 跟踪跳过");
        return;
    };
    let (_, env) = parse_capture(capture);
    let mut state = tpi_core::util::lock_mutex(&ctx.shell, "shell");
    state.baseline = Some(env);
}

/// msys POSIX 路径 → Windows 路径：`/c/foo` → `C:\\foo`；`/c`（根目录）→
/// `C:\`；已是 Windows 风格（含盘符冒号）或 UNC 则原样返回。
/// ISSUE-043：`/c` 只有 2 字节，此前不转换——cygpath 缺失时 session cwd 被
/// 写成 `/c`，Windows `Command::current_dir("/c")` 解析成 `C:\c`（不存在），
/// 后续所有 bash 调用 spawn 失败。
fn msys_path_to_windows(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        let mut out = String::with_capacity(path.len());
        out.push(bytes[1].to_ascii_uppercase() as char);
        out.push(':');
        out.push('\\');
        out.push_str(&path[3..].replace('/', "\\"));
        out
    } else if bytes.len() == 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() {
        // `/c`：盘根目录。
        let mut out = String::with_capacity(3);
        out.push(bytes[1].to_ascii_uppercase() as char);
        out.push(':');
        out.push('\\');
        out
    } else {
        path.to_string()
    }
}

/// session cwd 边界校验（任务书 §17）：严格模式（`allow_outside_workspace=false`）
/// 下逻辑 cwd 必须位于 workspace root 内（大小写不敏感 + 分隔符边界，防 `C:\\proj2`
/// 误匹配 `C:\\proj`）；自由模式（默认）接受任意绝对路径。
fn validate_session_cwd(ctx: &ToolContext, candidate: &str) -> Result<camino::Utf8PathBuf, String> {
    let path = camino::Utf8PathBuf::from(candidate);
    if ctx.allow_outside_workspace {
        return Ok(path);
    }
    let root = norm_path_for_compare(ctx.workspace_root.as_str());
    let cand = norm_path_for_compare(candidate);
    if cand.starts_with(&root) {
        let rest = &cand[root.len()..];
        if rest.is_empty() || rest.starts_with('/') {
            return Ok(path);
        }
    }
    Err(format!(
        "shell cwd 逃出 workspace（strict sandbox）：{candidate} ∉ {}",
        ctx.workspace_root
    ))
}

/// 路径比较归一化：转小写 + `\`→`/` + 剥离 `\\?\` 前缀（Windows UNC）。
fn norm_path_for_compare(path: &str) -> String {
    let lower = path.to_lowercase();
    let lower = lower.strip_prefix("\\\\?\\").unwrap_or(&lower);
    lower.replace('\\', "/")
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
            ..Default::default()
        },
        evidence: Vec::new(),
        observed_resources: Vec::new(),
        artifacts: Vec::new(),
        timing: tpi_core::outcome::ToolTiming { duration_ms },
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
                env_remove: Vec::new(),
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

    // ---- 就地修改拦截（in-place edit guard）----

    #[test]
    fn in_place_edit_detects_sed_forms() {
        for (cmd, expected) in [
            ("sed -i 's/a/b/' file", "sed -i"),
            ("sed -i.bak 's/a/b/' file", "sed -i.bak"),
            ("sed --in-place 's/a/b/' file", "sed --in-place"),
            ("sed --in-place=.bak 's/a/b/' file", "sed --in-place=.bak"),
        ] {
            assert_eq!(
                find_in_place_edit(cmd).as_deref(),
                Some(expected),
                "应拦截并报告: {cmd}"
            );
        }
    }

    #[test]
    fn in_place_edit_detects_perl_forms() {
        for cmd in [
            "perl -i -pe 's/a/b/' file",
            "perl -pi -e 's/a/b/' file",
            "perl -npi.bak -e 's/a/b/' file",
            "perl -i.bak -pe 's/a/b/' file",
        ] {
            assert!(find_in_place_edit(cmd).is_some(), "应拦截: {cmd}");
        }
    }

    #[test]
    fn in_place_edit_detects_path_and_exe_forms() {
        assert!(find_in_place_edit("/usr/bin/sed -i 's/a/b/' file").is_some());
        assert!(find_in_place_edit("sed.exe -i 's/a/b/' file").is_some());
    }

    #[test]
    fn in_place_edit_detects_in_pipeline_and_sequence() {
        assert!(find_in_place_edit("cat f | sed -i 's/a/b/' f").is_some());
        assert!(find_in_place_edit("cmd1 && perl -i -pe 's/a/b/' f").is_some());
        assert!(find_in_place_edit("(sed -i 's/a/b/' f)").is_some());
    }

    #[test]
    fn in_place_edit_allows_stream_sed() {
        for cmd in [
            "sed -n '1,5p' file",
            "sed 's/a/b/' file > out",
            "sed -n 's/x/y/p' file | grep y",
            "sed -n '1,5p' file | head",
            "sed -- -i", // `--` 之后是文件名，不是就地修改 flag
        ] {
            assert!(find_in_place_edit(cmd).is_none(), "不应拦截: {cmd}");
        }
    }

    #[test]
    fn in_place_edit_ignores_other_commands_and_quotes() {
        for cmd in [
            "grep -i pattern file",
            "rg -i pattern",
            "echo \"sed -i is bad\"",
            "git commit -m \"perl -i\"",
            "awk '{print $1}' file",
            "sed_cmd -i file", // 程序名不是 sed
        ] {
            assert!(find_in_place_edit(cmd).is_none(), "不应拦截: {cmd}");
        }
    }

    /// 入口集成：拦截返回 Rejected，引导 edit/write（不经过真实执行）。
    #[test]
    fn bash_entry_rejects_in_place_edit_without_running() {
        // find_in_place_edit 是 bash() 入口的唯一判定源；入口逻辑为
        // `if let Some(...) => rejected_bash(...)`。此处验证返回值形态。
        let cmd = "sed -i 's/a/b/' file";
        let matched = find_in_place_edit(cmd).expect("应命中就地修改");
        let outcome = rejected_bash("in_place_edit", matched);
        assert_eq!(outcome.model_payload.status, ToolStatus::Rejected);
        assert!(outcome.model_payload.output.contains("in_place_edit"));
        assert!(outcome.model_payload.output.contains("edit"));
    }
}
