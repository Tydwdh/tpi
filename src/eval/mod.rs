//! Eval Harness：对真实 coding task 的自动评测。
//!
//! 目录结构（默认 `<workspace>/evals/`）：
//! ```text
//! evals/<task-id>/
//!   task.md        —— 任务描述（作为 run 的 user message）
//!   expected.toml  —— name/suite/base_commit/timeout_sec/verify（验收断言）
//!   repo/          —— 可重置的 git 仓库（每次评测前 reset --hard + clean）
//! ```
//!
//! 流程：reset repo → 创建独立 session → agent::run(task.md) →
//! 统计 session 事件指标 → 执行验收断言 → JSON 结果。
//!
//! 结果写入 `~/.tpi/evals/results/<task-id>.json`（最近一次）+ `runs.jsonl`
//! （历史累计）。评测调用真实 provider（花钱）——只在用户显式运行
//! `tpi eval` 时发生。

use std::path::{Path, PathBuf};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::agent;
use crate::config::Config;
use crate::ids::RunId;
use crate::outcome::ToolStatus;
use crate::session::{SessionEvent, SessionLog};

/// Eval 根目录名（相对 workspace）。
pub const EVALS_DIR: &str = "evals";
/// 结果根目录（相对 ~/.tpi）。
pub const RESULTS_DIR: &str = "evals/results";
/// 评测 session 根目录（相对 ~/.tpi）。
pub const SESSIONS_DIR: &str = "evals/sessions";
const MAX_EVAL_TASK_BYTES: usize = 1024 * 1024;
const MAX_EXPECTED_BYTES: usize = 1024 * 1024;
const MAX_VERIFY_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVAL_TASKS: usize = 10_000;
const MAX_VERIFY_STEPS: usize = 1_000;
const MAX_EVAL_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const MAX_VERIFY_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// 验收断言（expected.toml 的 `[[verify]]`）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerifyStep {
    /// 在 repo 根执行 bash 命令并断言。
    Bash {
        command: String,
        #[serde(default)]
        expect_exit: Option<i32>,
        #[serde(default)]
        expect_stdout_contains: Vec<String>,
        #[serde(default)]
        expect_stderr_contains: Vec<String>,
    },
    /// 断言文件存在（repo 相对路径）。
    FileExists { path: String },
    /// 断言文件内容包含子串（repo 相对路径）。
    FileContains { path: String, contains: String },
}

impl VerifyStep {
    /// 人类可读描述（--list / 结果展示）。
    fn describe(&self) -> String {
        match self {
            VerifyStep::Bash { command, .. } => format!("bash: {command}"),
            VerifyStep::FileExists { path } => format!("file_exists: {path}"),
            VerifyStep::FileContains { path, .. } => format!("file_contains: {path}"),
        }
    }
}

/// expected.toml。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub name: String,
    /// 套件名（`tpi eval --suite <name>` 批量运行）。
    #[serde(default)]
    pub suite: String,
    /// 可选任务标题（--list 展示）。
    #[serde(default)]
    pub title: Option<String>,
    /// 评测前 reset 到的 commit（默认 HEAD）。
    #[serde(default)]
    pub base_commit: Option<String>,
    /// 单任务超时（秒；默认 900）。
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: u64,
    /// 验收断言（全部通过 → verification_passed）。
    #[serde(default)]
    pub verify: Vec<VerifyStep>,
}

fn default_timeout_sec() -> u64 {
    900
}

/// 发现的评测任务。
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub id: String,
    pub dir: PathBuf,
    pub repo_dir: PathBuf,
    pub task_md: String,
    pub expected: Expected,
}

/// 在 evals 根下发现全部任务（`<root>/<id>/task.md + expected.toml + repo/`）。
pub fn discover(evals_root: &Path) -> Result<Vec<TaskEntry>, String> {
    if !evals_root.is_dir() {
        return Err(format!("evals 根目录不存在: {}", evals_root.display()));
    }
    if crate::util::is_symlink_or_reparse(evals_root)
        .map_err(|error| format!("检查 evals 根目录失败: {error}"))?
    {
        return Err("evals 根目录不能是符号链接或 reparse point".into());
    }
    let canonical_root = evals_root
        .canonicalize()
        .map_err(|error| format!("解析 evals 根目录失败: {error}"))?;
    let mut tasks = Vec::new();
    for entry in std::fs::read_dir(evals_root).map_err(|e| format!("读取 evals 失败: {e}"))? {
        let entry = entry.map_err(|e| format!("读取 evals 失败: {e}"))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir = entry.path();
        if crate::util::is_symlink_or_reparse(&dir)
            .map_err(|error| format!("检查 {} 失败: {error}", dir.display()))?
        {
            return Err(format!(
                "评测任务目录不能是符号链接或 reparse point: {}",
                dir.display()
            ));
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("评测任务目录名不是有效 Unicode: {}", dir.display()))?;
        let task_md_path = dir.join("task.md");
        let expected_path = dir.join("expected.toml");
        let repo_dir = dir.join("repo");
        let has_any_task_component =
            task_md_path.exists() || expected_path.exists() || repo_dir.exists();
        if !has_any_task_component {
            continue;
        }
        if !task_md_path.is_file() || !expected_path.is_file() || !repo_dir.is_dir() {
            return Err(format!(
                "评测任务目录结构不完整: {}（需要 task.md、expected.toml 与 repo/）",
                dir.display()
            ));
        }
        for path in [&task_md_path, &expected_path, &repo_dir] {
            if crate::util::is_symlink_or_reparse(path)
                .map_err(|error| format!("检查 {} 失败: {error}", path.display()))?
            {
                return Err(format!(
                    "评测任务组件不能是符号链接或 reparse point: {}",
                    path.display()
                ));
            }
        }
        let canonical_dir = dir
            .canonicalize()
            .map_err(|error| format!("解析任务目录失败: {error}"))?;
        let canonical_repo = repo_dir
            .canonicalize()
            .map_err(|error| format!("解析评测 repo 失败: {error}"))?;
        if !canonical_dir.starts_with(&canonical_root)
            || !canonical_repo.starts_with(&canonical_dir)
        {
            return Err(format!(
                "评测 repo 必须位于任务目录内: {}",
                repo_dir.display()
            ));
        }
        let task_md = crate::util::read_utf8_file_bounded(&task_md_path, MAX_EVAL_TASK_BYTES)
            .map_err(|e| format!("读取 {} 失败: {e}", task_md_path.display()))?;
        let raw = crate::util::read_utf8_file_bounded(&expected_path, MAX_EXPECTED_BYTES)
            .map_err(|e| format!("读取 {} 失败: {e}", expected_path.display()))?;
        let expected: Expected = toml::from_str(&raw)
            .map_err(|e| format!("解析 {} 失败: {e}", expected_path.display()))?;
        if expected.name != id {
            return Err(format!(
                "{}: expected.toml 的 name 必须等于目录名（{}）",
                expected_path.display(),
                id
            ));
        }
        if task_md.trim().is_empty() {
            return Err(format!("{}: task.md 不能为空", task_md_path.display()));
        }
        if expected.timeout_sec == 0 || expected.timeout_sec > MAX_EVAL_TIMEOUT_SECS {
            return Err(format!(
                "{}: timeout_sec 必须在 1..={MAX_EVAL_TIMEOUT_SECS} 范围内",
                expected_path.display(),
            ));
        }
        if expected.verify.is_empty() || expected.verify.len() > MAX_VERIFY_STEPS {
            return Err(format!(
                "{}: 至少需要一条 [[verify]]，且最多 {MAX_VERIFY_STEPS} 条",
                expected_path.display(),
            ));
        }
        if let Some(base_commit) = expected.base_commit.as_deref() {
            validate_git_revision(base_commit).map_err(|error| {
                format!("{}: base_commit 无效: {error}", expected_path.display())
            })?;
        }
        for step in &expected.verify {
            match step {
                VerifyStep::FileExists { path } | VerifyStep::FileContains { path, .. } => {
                    validate_verify_relative_path(path).map_err(|error| {
                        format!("{}: verify 路径无效: {error}", expected_path.display())
                    })?;
                }
                VerifyStep::Bash { .. } => {}
            }
        }
        tasks.push(TaskEntry {
            id,
            dir,
            repo_dir,
            task_md,
            expected,
        });
        if tasks.len() > MAX_EVAL_TASKS {
            return Err(format!("评测任务数量超过 {MAX_EVAL_TASKS} 上限"));
        }
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tasks)
}

/// 全部套件名（去重排序）。
pub fn list_suites(evals_root: &Path) -> Result<Vec<String>, String> {
    let mut suites: Vec<String> = discover(evals_root)?
        .into_iter()
        .map(|t| t.expected.suite)
        .filter(|s| !s.is_empty())
        .collect();
    suites.sort();
    suites.dedup();
    Ok(suites)
}

/// 把 repo reset 到 base commit（`git reset --hard` + `git clean -fdx`）。
/// 默认 base_commit = HEAD；保证每次评测从干净现场开始（可重复）。
pub fn reset_repo(repo_dir: &Path, base_commit: Option<&str>) -> Result<(), String> {
    let git = |args: &[&str]| -> Result<(), String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo_dir)
            .output()
            .map_err(|e| format!("git 执行失败（repo: {}）: {e}", repo_dir.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "git {} 失败: {stderr}",
                args.first().copied().unwrap_or("")
            ));
        }
        Ok(())
    };
    let target = base_commit.unwrap_or("HEAD");
    validate_git_revision(target)?;
    git(&["reset", "--hard", target])?;
    git(&["clean", "-fdx"])?;
    Ok(())
}

fn validate_git_revision(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(
            "Git revision 必须是 1..=256 字节、不能以 '-' 开头或包含空白边界/控制字符".into(),
        );
    }
    Ok(())
}

/// 单个任务的评测结果（JSON 序列化）。
#[derive(Debug, Clone, Serialize)]
pub struct EvalResult {
    pub task_id: String,
    pub suite: String,
    pub success: bool,
    pub reason: String,
    pub verification_passed: bool,
    pub verify: Vec<VerifyResult>,
    pub wall_time_ms: u64,
    pub turns: u32,
    pub tool_calls: u32,
    pub run_calls: u32,
    pub bash_calls: u32,
    pub read_calls: u32,
    pub search_calls: u32,
    pub edit_calls: u32,
    pub write_calls: u32,
    pub web_search_calls: u32,
    pub web_fetch_calls: u32,
    pub other_calls: u32,
    pub repeated_actions: u32,
    pub edit_failures: u32,
    pub stale_failures: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub first_edit_time_ms: Option<u64>,
    pub compaction_count: u32,
    pub session_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 单条验收断言结果。
#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
    pub step: String,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl VerifyResult {
    fn new(step: &VerifyStep, passed: bool, detail: Option<String>) -> Self {
        Self {
            step: step.describe(),
            passed,
            detail,
        }
    }
}

/// 从 session 事件统计的指标（seq, timestamp_ms, event）。
#[derive(Default)]
struct EvalStats {
    turns: u32,
    tool_calls: u32,
    run_calls: u32,
    bash_calls: u32,
    read_calls: u32,
    search_calls: u32,
    edit_calls: u32,
    write_calls: u32,
    web_search_calls: u32,
    web_fetch_calls: u32,
    other_calls: u32,
    repeated_actions: u32,
    edit_failures: u32,
    stale_failures: u32,
    input_tokens: u64,
    output_tokens: u64,
    first_edit_time_ms: Option<u64>,
    compaction_count: u32,
}

/// 读取并严格验证 session JSONL，再生成 (timestamp_ms, SessionEvent) 列表。
/// session 协议知识只由持久层维护；eval 不再自行跳过坏行后生成误导性指标。
fn read_events_with_ts(path: &Path) -> Result<Vec<(i128, SessionEvent)>, String> {
    let envelopes = crate::session::read_envelopes(path)
        .map_err(|error| format!("读取 session 失败: {error}"))?;
    let mut events = Vec::new();
    for envelope in envelopes {
        let ts = time::OffsetDateTime::parse(
            &envelope.timestamp,
            &time::format_description::well_known::Rfc3339,
        )
        .map(|dt| dt.unix_timestamp_nanos() / 1_000_000)
        .map_err(|error| format!("session timestamp 无效: {error}"))?;
        events.push((ts, envelope.to_session_event()));
    }
    Ok(events)
}

/// 从事件序列统计指标。
fn stats_from_events(events: &[(i128, SessionEvent)]) -> EvalStats {
    let mut stats = EvalStats::default();
    let mut tool_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut start_ts: Option<i128> = None;
    let mut first_edit_ts: Option<i128> = None;

    for (ts, event) in events {
        match event {
            SessionEvent::UserSubmitted { .. } => start_ts = Some(*ts),
            // §13：挂起请求/回答不是独立指标（run 挂起不计入 turns）。
            SessionEvent::UserInputRequested { .. } | SessionEvent::UserInputReceived { .. } => {}
            SessionEvent::RunStarted { .. } => stats.run_calls += 1,
            SessionEvent::AssistantMessageCommitted { .. } => stats.turns += 1,
            // 中断的 attempt 不是完整 turn：不计入 turns（§4.3 语义区分）。
            SessionEvent::AssistantAttemptInterrupted { .. } => {}
            SessionEvent::ToolRequested { call } => {
                stats.tool_calls += 1;
                tool_name.insert(call.call_id.to_string(), call.name.clone());
                match call.name.as_str() {
                    "bash" => stats.bash_calls += 1,
                    "read" => stats.read_calls += 1,
                    "list" | "search" => stats.search_calls += 1,
                    "edit" => stats.edit_calls += 1,
                    "write" => stats.write_calls += 1,
                    "web_search" => stats.web_search_calls += 1,
                    "web_fetch" => stats.web_fetch_calls += 1,
                    _ => stats.other_calls += 1,
                }
            }
            SessionEvent::ToolStarted {
                recovery: Some(recovery),
                ..
            } if recovery.tool == "edit" && first_edit_ts.is_none() => {
                first_edit_ts = Some(*ts);
            }
            SessionEvent::ToolStarted { .. } => {}
            SessionEvent::ToolCompleted { call_id, outcome } => {
                let name = tool_name
                    .get(&call_id.to_string())
                    .cloned()
                    .unwrap_or_default();
                let text = outcome.model_payload.output.clone();
                if text.contains("repeated_without_progress") {
                    stats.repeated_actions += 1;
                }
                if name == "edit" && outcome.status == ToolStatus::Failed {
                    stats.edit_failures += 1;
                    if text.contains("stale revision") {
                        stats.stale_failures += 1;
                    }
                }
            }
            SessionEvent::CompactionCommitted { .. } => stats.compaction_count += 1,
            SessionEvent::RunCompleted { usage, .. } => {
                stats.input_tokens = stats.input_tokens.saturating_add(usage.input_tokens);
                stats.output_tokens = stats.output_tokens.saturating_add(usage.output_tokens);
            }
            SessionEvent::PlanReplaced { .. } => {}
        }
    }

    if let (Some(start), Some(first)) = (start_ts, first_edit_ts) {
        stats.first_edit_time_ms = Some(u64::try_from((first - start).max(0)).unwrap_or(u64::MAX));
    }
    stats
}

/// 运行单个任务（真实 provider，花钱——仅在用户显式 `tpi eval` 时调用）。
pub async fn run_task(
    task: &TaskEntry,
    results_dir: &Path,
    config: &Config,
) -> Result<EvalResult, String> {
    let started = std::time::Instant::now();

    // 1. 重置 repo（可重复性）。
    reset_repo(&task.repo_dir, task.expected.base_commit.as_deref())?;

    // 2. 独立 session 环境（不污染用户会话）。
    let home = crate::config::tpi_home();
    let sessions_root = home.join(SESSIONS_DIR);
    let artifacts_root = home.join("evals/artifacts");
    std::fs::create_dir_all(&sessions_root).map_err(|e| format!("创建 session 目录失败: {e}"))?;
    std::fs::create_dir_all(&artifacts_root).map_err(|e| format!("创建 artifacts 失败: {e}"))?;

    // 3. 以 repo 为 workspace 的独立配置。
    let repo_root = Utf8PathBuf::from_path_buf(task.repo_dir.clone())
        .map_err(|e| format!("repo 路径非法: {:?}", e))?;
    let mut eval_config = config.clone();
    eval_config.workspace_root = repo_root.clone();
    eval_config.sessions_root = sessions_root.clone();
    eval_config.artifacts_root = artifacts_root.clone();

    // 4. 创建 session 并运行 agent。
    let mut session = SessionLog::create(&sessions_root, repo_root.as_std_path(), RunId::new_v7())
        .map_err(|e| format!("创建 session 失败: {e}"))?;
    let api_key = crate::config::read_api_key(config)?;
    let mut provider = crate::provider::openai_compat::OpenAiCompatClient::new(
        config.model.base_url.clone(),
        config.model.name.clone(),
        api_key,
        config.model.reasoning.clone(),
        config.model.max_output_tokens,
        config.model.context_window,
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel(128);
    tokio::spawn(async move { while ui_rx.recv().await.is_some() {} });

    let cancel_run = cancel.clone();
    let run_result = tokio::time::timeout(
        std::time::Duration::from_secs(task.expected.timeout_sec),
        agent::run(
            &mut provider,
            &mut session,
            &eval_config,
            agent::RunInput {
                history: &[],
                user_message: task.task_md.clone(),
                ui: ui_tx,
                cancel,
                interactive: false,
                force_compaction: false,
                workspace: None,
                registry: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::tool::registry::builtin_registry(),
                )),
            },
        ),
    )
    .await;

    // §PointerHit 5：超时时显式 cancel token，再等 grace period——
    // 否则 agent 内部 watchdog/provider task 未被明确取消（可能泄漏/残留）。
    let (success, reason, error) = match run_result {
        Err(_) => {
            cancel_run.cancel();
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                std::future::pending::<()>(),
            )
            .await;
            (false, "timeout".to_string(), Some("评测超时".to_string()))
        }
        Ok(Err(failure)) => (false, format!("error:{failure}"), Some(failure.to_string())),
        Ok(Ok(outcome)) => (
            outcome.reason == crate::session::CompletionReason::Stop,
            format!("{:?}", outcome.reason),
            None,
        ),
    };

    // 5. 事件统计（读取 session 文件）。
    let events =
        read_events_with_ts(session.path()).map_err(|e| format!("读取 session 事件失败: {e}"))?;
    let stats = stats_from_events(&events);

    // 6. 验收断言。
    let verify = run_verify(
        &task.expected.verify,
        &task.repo_dir,
        eval_config.shell_path.as_ref(),
    )
    .await;
    let verification_passed = verify.iter().all(|v| v.passed);

    // 7. 汇总。
    let result = EvalResult {
        task_id: task.id.clone(),
        suite: task.expected.suite.clone(),
        success,
        reason,
        verification_passed,
        verify,
        wall_time_ms: started.elapsed().as_millis() as u64,
        turns: stats.turns,
        tool_calls: stats.tool_calls,
        run_calls: stats.run_calls,
        bash_calls: stats.bash_calls,
        read_calls: stats.read_calls,
        search_calls: stats.search_calls,
        edit_calls: stats.edit_calls,
        write_calls: stats.write_calls,
        web_search_calls: stats.web_search_calls,
        web_fetch_calls: stats.web_fetch_calls,
        other_calls: stats.other_calls,
        repeated_actions: stats.repeated_actions,
        edit_failures: stats.edit_failures,
        stale_failures: stats.stale_failures,
        input_tokens: stats.input_tokens,
        output_tokens: stats.output_tokens,
        first_edit_time_ms: stats.first_edit_time_ms,
        compaction_count: stats.compaction_count,
        session_path: session.path().display().to_string(),
        error,
    };

    // 8. 写结果：<task-id>.json（最近一次）+ runs.jsonl（累计）。
    persist_result(&result, results_dir)?;
    Ok(result)
}

/// 探测 bash（eval 无 ToolContext；优先常见 Git Bash 路径，其次 PATH）。
fn locate_bash(configured: Option<&Utf8PathBuf>) -> Option<String> {
    if let Some(path) = configured
        && path.is_file()
    {
        return Some(path.to_string());
    }
    let candidates = [
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files\\Git\\usr\\bin\\bash.exe",
    ];
    for candidate in candidates {
        if std::path::Path::new(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("bash.exe");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

async fn run_verify_bash(
    shell: &str,
    command: &str,
    repo_dir: &Path,
) -> Result<crate::process::HostRunOutput, String> {
    let cwd = repo_dir
        .to_str()
        .ok_or_else(|| "eval repo 路径不是有效 Unicode".to_string())?;
    let args = crate::tool::command::RunArgs {
        program: shell.to_string(),
        args: vec![
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            format!("set -o pipefail\n{command}"),
        ],
        cwd: cwd.to_string(),
        timeout_ms: 60_000,
        env: Default::default(),
        env_remove: Vec::new(),
    };
    let resolved_program = PathBuf::from(shell);
    crate::process::run_in_host(crate::process::HostRunRequest {
        args: &args,
        resolved_program: &resolved_program,
        launcher: Some("git-bash"),
        cancel: tokio_util::sync::CancellationToken::new(),
        timeout: std::time::Duration::from_secs(60),
        output_budget: MAX_VERIFY_OUTPUT_BYTES,
        artifact: None,
        stream_sink: None,
        capture_nonce: None,
    })
    .await
}

/// 执行验收断言（bash 步骤在 repo 根运行，timeout 60s/步）。
async fn run_verify(
    steps: &[VerifyStep],
    repo_dir: &Path,
    configured_shell: Option<&Utf8PathBuf>,
) -> Vec<VerifyResult> {
    let bash = locate_bash(configured_shell);
    let mut results = Vec::new();
    for step in steps {
        let result = match step {
            VerifyStep::Bash {
                command,
                expect_exit,
                expect_stdout_contains,
                expect_stderr_contains,
            } => {
                let shell = bash.clone().unwrap_or_else(|| "bash".to_string());
                match run_verify_bash(&shell, command, repo_dir).await {
                    Err(error) => VerifyResult::new(step, false, Some(error)),
                    Ok(output) if output.ended_by == crate::process::EndReason::TimedOut => {
                        VerifyResult::new(step, false, Some("超时（>60s）".into()))
                    }
                    Ok(output) if output.ended_by == crate::process::EndReason::Cancelled => {
                        VerifyResult::new(step, false, Some("验收命令被取消".into()))
                    }
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let expected = expect_exit.unwrap_or(0);
                        let mut failures: Vec<String> = Vec::new();
                        if output.exit_code != Some(expected) {
                            failures
                                .push(format!("exit code {:?} != {expected}", output.exit_code));
                        }
                        if output.stdout_total > output.stdout.len() as u64 {
                            failures.push(format!(
                                "stdout 超过 {MAX_VERIFY_OUTPUT_BYTES} 字节验收上限"
                            ));
                        }
                        if output.stderr_total > output.stderr.len() as u64 {
                            failures.push(format!(
                                "stderr 超过 {MAX_VERIFY_OUTPUT_BYTES} 字节验收上限"
                            ));
                        }
                        for needle in expect_stdout_contains {
                            if !stdout.contains(needle) {
                                failures.push(format!("stdout 缺少: {needle:?}"));
                            }
                        }
                        for needle in expect_stderr_contains {
                            if !stderr.contains(needle) {
                                failures.push(format!("stderr 缺少: {needle:?}"));
                            }
                        }
                        if failures.is_empty() {
                            VerifyResult::new(step, true, None)
                        } else {
                            VerifyResult::new(step, false, Some(failures.join("；")))
                        }
                    }
                }
            }
            VerifyStep::FileExists { path } => match resolve_verify_path(repo_dir, path) {
                Ok(full) if full.is_file() => VerifyResult::new(step, true, None),
                Ok(_) => VerifyResult::new(step, false, Some("文件不存在".into())),
                Err(error) => VerifyResult::new(step, false, Some(error)),
            },
            VerifyStep::FileContains { path, contains } => {
                match resolve_verify_path(repo_dir, path) {
                    Ok(full) => {
                        match crate::util::read_utf8_file_bounded(&full, MAX_VERIFY_FILE_BYTES) {
                            Ok(text) if text.contains(contains) => {
                                VerifyResult::new(step, true, None)
                            }
                            Ok(_) => VerifyResult::new(
                                step,
                                false,
                                Some(format!("文件缺少内容: {contains:?}")),
                            ),
                            Err(e) => {
                                VerifyResult::new(step, false, Some(format!("读取失败: {e}")))
                            }
                        }
                    }
                    Err(error) => VerifyResult::new(step, false, Some(error)),
                }
            }
        };
        results.push(result);
    }
    results
}

fn validate_verify_relative_path(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() {
        return Err("路径不能为空".into());
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => return Err("路径必须位于 eval repo 内，不能是绝对路径或包含 '..'".into()),
        }
    }
    Ok(())
}

fn resolve_verify_path(repo_dir: &Path, path: &str) -> Result<PathBuf, String> {
    validate_verify_relative_path(path)?;
    let root = repo_dir
        .canonicalize()
        .map_err(|error| format!("解析 repo 路径失败: {error}"))?;
    let candidate = repo_dir.join(path);
    let mut existing = candidate.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| "verify 路径没有可解析的祖先".to_string())?;
    }
    let resolved_ancestor = existing
        .canonicalize()
        .map_err(|error| format!("解析 verify 路径失败: {error}"))?;
    if !resolved_ancestor.starts_with(&root) {
        return Err("verify 路径通过链接逃出了 eval repo".into());
    }
    Ok(candidate)
}

/// 把结果写入 `<results_dir>/<task-id>.json` 并追加 `runs.jsonl`。
pub fn persist_result(result: &EvalResult, results_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(results_dir).map_err(|e| format!("创建结果目录失败: {e}"))?;
    let json = serde_json::to_string_pretty(result).map_err(|e| format!("结果序列化失败: {e}"))?;
    let file = results_dir.join(format!("{}.json", result.task_id));
    std::fs::write(&file, json).map_err(|e| format!("写入结果失败: {e}"))?;
    // runs.jsonl 追加（累计历史）。
    let line = serde_json::to_string(result).map_err(|e| format!("结果序列化失败: {e}"))?;
    let runs = results_dir.join("runs.jsonl");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&runs)
        .map_err(|e| format!("打开 runs.jsonl 失败: {e}"))?;
    f.write_all(line.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .map_err(|e| format!("写入 runs.jsonl 失败: {e}"))?;
    Ok(())
}

/// stdout 摘要（人类可读）。
pub fn render_summary(result: &EvalResult) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "task: {} [{}]\n",
        result.task_id,
        if result.suite.is_empty() {
            "-"
        } else {
            &result.suite
        }
    ));
    out.push_str(&format!(
        "run:   {} ({})\n",
        if result.success { "SUCCESS" } else { "FAILED" },
        result.reason
    ));
    out.push_str(&format!(
        "verify: {} ({}/{})\n",
        if result.verification_passed {
            "PASSED"
        } else {
            "FAILED"
        },
        result.verify.iter().filter(|v| v.passed).count(),
        result.verify.len()
    ));
    for v in &result.verify {
        out.push_str(&format!(
            "  [{}] {}{}\n",
            if v.passed { "PASS" } else { "FAIL" },
            v.step,
            v.detail
                .as_ref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default()
        ));
    }
    out.push_str(&format!(
        "metrics: turns={} tools={} bash={} read={} search={} edit={} write={} web={}/{} repeats={} edit_fail={} stale={} compact={} first_edit={:?}ms tokens={}/{} wall={}ms\n",
        result.turns,
        result.tool_calls,
        result.bash_calls,
        result.read_calls,
        result.search_calls,
        result.edit_calls,
        result.write_calls,
        result.web_search_calls,
        result.web_fetch_calls,
        result.repeated_actions,
        result.edit_failures,
        result.stale_failures,
        result.compaction_count,
        result.first_edit_time_ms,
        result.input_tokens,
        result.output_tokens,
        result.wall_time_ms,
    ));
    if let Some(error) = &result.error {
        out.push_str(&format!("error: {error}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 task 目录（tempdir 内）。
    fn make_task(evals_root: &Path, id: &str, suite: &str, task_md: &str) {
        let dir = evals_root.join(id);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("task.md"), task_md).unwrap();
        std::fs::write(
            dir.join("expected.toml"),
            format!(
                "name = \"{id}\"\nsuite = \"{suite}\"\n\n[[verify]]\ntype = \"file_exists\"\npath = \"hello.txt\"\n"
            ),
        )
        .unwrap();
        // repo：初始化 git 并提交一个文件。
        let repo = dir.join("repo");
        std::fs::write(repo.join("hello.txt"), "hello\n").unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .output()
            .unwrap();
        // 显式身份（测试环境可能没有全局 user.name/email）。
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=eval",
                "-c",
                "user.email=eval@test.local",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .current_dir(&repo)
            .output()
            .unwrap();
    }

    #[test]
    fn discover_lists_tasks_and_suites() {
        let dir = tempfile::tempdir().unwrap();
        make_task(dir.path(), "aaa-001", "core", "fix it");
        make_task(dir.path(), "bbb-001", "ext", "do it");
        std::fs::create_dir(dir.path().join("unrelated-empty-dir")).unwrap();

        let tasks = discover(dir.path()).unwrap();
        assert_eq!(tasks.len(), 2, "不含任务组件的目录应被忽略");
        assert_eq!(tasks[0].id, "aaa-001");
        assert_eq!(tasks[1].id, "bbb-001");
        assert_eq!(list_suites(dir.path()).unwrap(), vec!["core", "ext"]);
    }

    #[test]
    fn discover_rejects_incomplete_or_unverifiable_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let incomplete = dir.path().join("incomplete");
        std::fs::create_dir(&incomplete).unwrap();
        std::fs::write(incomplete.join("task.md"), "do it").unwrap();
        assert!(discover(dir.path()).unwrap_err().contains("结构不完整"));

        std::fs::remove_dir_all(&incomplete).unwrap();
        let no_verify = dir.path().join("no-verify");
        std::fs::create_dir_all(no_verify.join("repo")).unwrap();
        std::fs::write(no_verify.join("task.md"), "do it").unwrap();
        std::fs::write(no_verify.join("expected.toml"), "name = \"no-verify\"\n").unwrap();
        assert!(discover(dir.path()).unwrap_err().contains("至少需要一条"));
    }

    #[tokio::test]
    async fn file_verification_cannot_escape_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        let steps = vec![VerifyStep::FileContains {
            path: "../secret.txt".into(),
            contains: "secret".into(),
        }];

        let results = run_verify(&steps, &repo, None).await;

        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert!(results[0].detail.as_deref().unwrap().contains("repo 内"));
    }

    #[test]
    fn configured_bash_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let bash = dir.path().join("custom-bash.exe");
        std::fs::write(&bash, "stub").unwrap();
        let bash = Utf8PathBuf::from_path_buf(bash).unwrap();
        assert_eq!(locate_bash(Some(&bash)).as_deref(), Some(bash.as_str()));
    }

    #[test]
    fn reset_repo_restores_base_commit() {
        let dir = tempfile::tempdir().unwrap();
        make_task(dir.path(), "t1", "core", "fix it");
        let repo = dir.path().join("t1/repo");
        // 破坏现场：删文件 + 未提交修改 + 未跟踪文件。
        std::fs::remove_file(repo.join("hello.txt")).unwrap();
        std::fs::write(repo.join("dirty.txt"), "dirty\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "x\n").unwrap();
        reset_repo(&repo, None).unwrap();
        assert!(repo.join("hello.txt").is_file(), "被删文件必须恢复");
        assert!(!repo.join("dirty.txt").exists(), "未提交修改必须被清除");
        assert!(!repo.join("untracked.txt").exists(), "未跟踪文件必须被清除");
    }

    #[test]
    fn stats_count_events() {
        use crate::ids::ToolCallId;
        use crate::outcome::{ModelPayload, StoredToolOutcome};
        use crate::provider::ToolCall;
        use crate::session::{
            AssistantMessage, CompactSummary, CompletionReason, EventRange, ModelRef, RunLimits,
            Usage,
        };

        let mut events: Vec<(i128, SessionEvent)> = Vec::new();
        let mut ts = 1_000_000i128;
        let mut push = |event: SessionEvent| {
            ts += 1000;
            events.push((ts, event));
        };
        push(SessionEvent::UserSubmitted {
            content: "task".into(),
        });
        push(SessionEvent::RunStarted {
            model: ModelRef {
                name: "m".into(),
                provider: "p".into(),
            },
            limits: RunLimits {
                max_turns: 10,
                max_tool_calls: 20,
            },
        });
        let call = ToolCall {
            call_id: ToolCallId::new_v7(),
            provider_id: "p1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        push(SessionEvent::ToolRequested { call: call.clone() });
        // 第一次 edit 的 write-ahead（first_edit_time 基准）。
        let edit_call = ToolCall {
            call_id: ToolCallId::new_v7(),
            provider_id: "p2".into(),
            name: "edit".into(),
            arguments: "{}".into(),
        };
        push(SessionEvent::ToolRequested {
            call: edit_call.clone(),
        });
        push(SessionEvent::ToolStarted {
            call_id: edit_call.call_id,
            recovery: Some(crate::session::RecoveryMetadata {
                tool: "edit".into(),
                target_path: "a.rs".into(),
                expected_revision: String::new(),
                candidate_revision: None,
                temp_path: String::new(),
                backup_path: Some(String::new()),
            }),
        });
        push(SessionEvent::ToolCompleted {
            call_id: edit_call.call_id,
            outcome: StoredToolOutcome {
                status: ToolStatus::Failed,
                model_payload: ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: "status: failed\nstale revision: x expected y".into(),
                    effect: None,
                    artifact: None,
                },
                session_metadata: Default::default(),
            },
        });
        push(SessionEvent::ToolCompleted {
            call_id: call.call_id,
            outcome: StoredToolOutcome {
                status: ToolStatus::Failed,
                model_payload: ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: "error: repeated_without_progress\n".into(),
                    effect: None,
                    artifact: None,
                },
                session_metadata: Default::default(),
            },
        });
        push(SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: "ok".into(),
                tool_calls: vec![],
            },
        });
        push(SessionEvent::CompactionCommitted {
            covered: EventRange {
                start: crate::ids::EventId::new_v7(),
                end: crate::ids::EventId::new_v7(),
            },
            summary: CompactSummary { text: "s".into() },
        });
        push(SessionEvent::RunCompleted {
            reason: CompletionReason::Stop,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
            },
        });

        let stats = stats_from_events(&events);
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.tool_calls, 2);
        assert_eq!(stats.bash_calls, 1);
        assert_eq!(stats.edit_calls, 1);
        assert_eq!(stats.repeated_actions, 1);
        assert_eq!(stats.edit_failures, 1);
        assert_eq!(stats.stale_failures, 1);
        assert_eq!(stats.compaction_count, 1);
        assert_eq!(stats.input_tokens, 100);
        assert_eq!(stats.output_tokens, 50);
        // first_edit_time：UserSubmitted 后第 4 个事件（ToolStarted(edit)，+4000ms）。
        assert_eq!(stats.first_edit_time_ms, Some(4000));
    }

    #[test]
    fn parse_expected_toml_accepts_verify_variants() {
        let raw = r#"
name = "x-001"
suite = "core"
title = "Fix the thing"
base_commit = "abc123"

[[verify]]
type = "bash"
command = "cargo test"
expect_exit = 0
expect_stdout_contains = ["test result: ok"]

[[verify]]
type = "file_exists"
path = "src/main.rs"

[[verify]]
type = "file_contains"
path = "src/main.rs"
contains = "fn main"
"#;
        let expected: Expected = toml::from_str(raw).unwrap();
        assert_eq!(expected.name, "x-001");
        assert_eq!(expected.base_commit.as_deref(), Some("abc123"));
        assert_eq!(expected.verify.len(), 3);
        assert!(matches!(
            expected.verify[0],
            VerifyStep::Bash {
                ref command,
                expect_exit: Some(0),
                ..
            } if command == "cargo test"
        ));
        assert!(matches!(
            expected.verify[1],
            VerifyStep::FileExists { ref path } if path == "src/main.rs"
        ));
        assert!(matches!(
            expected.verify[2],
            VerifyStep::FileContains { ref contains, .. } if contains == "fn main"
        ));
    }

    #[test]
    fn expected_toml_rejects_misplaced_or_unknown_fields() {
        let misplaced = r#"
name = "task"

[[verify]]
type = "file_exists"
path = "src/main.rs"
base_commit = "abc123"
"#;
        assert!(toml::from_str::<Expected>(misplaced).is_err());

        let root_unknown = r#"
name = "task"
typo_timeout = 10

[[verify]]
type = "file_exists"
path = "src/main.rs"
"#;
        assert!(toml::from_str::<Expected>(root_unknown).is_err());
    }
}
