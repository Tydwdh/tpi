//! Logical Shell Session 本地集成测试（任务书 §59 本地矩阵 + §66 DoD）。
//!
//! 依赖真实 Git Bash（`locate_git_bash`）与 `TPI_PROCESS_HOST` 指向真实
//! tpi.exe（单二进制 process-host 握手，§11.5）。

mod fixtures;

use camino::Utf8PathBuf;
use tokio_util::sync::CancellationToken;
use tpi::tool::command::{bash, BashArgs};
use tpi::tool::outcome::ToolStatus;

/// Windows 路径比较：盘符大小写不敏感 + 分隔符归一化（session cwd 来自
/// cygpath -w 是反斜杠，workspace.join() 是正斜杠）。
fn path_eq(a: &Utf8PathBuf, b: &Utf8PathBuf) -> bool {
    let norm = |s: &str| s.to_lowercase().replace('\\', "/");
    norm(a.as_str()) == norm(b.as_str())
}

async fn run_bash(
    ctx: &tpi::tool::ToolContext,
    command: &str,
    timeout_ms: u64,
    cancel: CancellationToken,
) -> tpi::tool::outcome::ToolOutcome {
    let mut ctx = ctx.clone();
    ctx.cancel = cancel;
    bash(
        BashArgs {
            command: command.into(),
            cwd: None,
            timeout_ms,
        },
        &ctx,
    )
    .await
}

/// 单测公共起点：tempdir workspace + 指向真实 tpi.exe 的 host。
fn setup() -> (tempfile::TempDir, Utf8PathBuf, tpi::tool::ToolContext) {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    (dir, workspace, ctx)
}

/// 未传 cwd → 使用逻辑 shell cwd；`cd` 跨调用保持（任务书 §10）。
#[tokio::test]
async fn cd_persists_across_calls() {
    let (_dir, workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    let r = run_bash(&ctx, "mkdir -p a/b", 60_000, cancel.clone()).await;
    assert_eq!(r.status, ToolStatus::Succeeded, "mkdir: {}", r.model_payload.output);

    let r = run_bash(&ctx, "cd a/b", 60_000, cancel.clone()).await;
    assert_eq!(r.status, ToolStatus::Succeeded, "cd: {}", r.model_payload.output);

    let state = ctx.shell.lock().unwrap();
    assert!(
        path_eq(&state.cwd, &workspace.join("a/b")),
        "cd 后 session cwd = {}，期望 {}",
        state.cwd,
        workspace.join("a/b")
    );
    drop(state);

    // 未传 cwd 的下一条命令在 session cwd 执行（pwd 应输出 a/b 目录）。
    let r = run_bash(&ctx, "pwd", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    assert!(
        r.model_payload.output.contains("a\\b") || r.model_payload.output.contains("a/b"),
        "pwd 应指向 a/b：{}",
        r.model_payload.output
    );
}

/// `cd ..` 回退；模型输出中不得出现捕获段标记（§22 control plane 剥离）。
#[tokio::test]
async fn cd_up_and_no_capture_leak() {
    let (_dir, workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p a/b", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "cd a/b", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "cd ..", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);

    let state = ctx.shell.lock().unwrap();
    assert!(path_eq(&state.cwd, &workspace.join("a")));
    drop(state);

    let r = run_bash(&ctx, "echo hello", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    assert!(r.model_payload.output.contains("hello"));
    assert!(
        !r.model_payload.output.contains("__TPI_CAPTURE_"),
        "捕获段不得泄漏到模型输出：{}",
        r.model_payload.output
    );
}

/// 失败命令仍 commit 最终 cwd（`cd src; false` 后 cwd 已是 src，任务书 §13）。
#[tokio::test]
async fn failed_command_still_commits_cwd() {
    let (_dir, workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p a/b", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "cd a/b; false", 60_000, cancel.clone()).await;
    assert_eq!(r.status, ToolStatus::Failed, "exit 非 0 → Failed");
    let state = ctx.shell.lock().unwrap();
    assert!(
        path_eq(&state.cwd, &workspace.join("a/b")),
        "正常结束（即使失败）后 cwd 更新：{}",
        state.cwd
    );
}

/// timeout 不 commit：保持 last confirmed 状态（任务书 §14/§59）。
#[tokio::test]
async fn timeout_does_not_commit_unknown_state() {
    let (_dir, workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p a", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "cd a", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "cd b; sleep 1000", 400, cancel.clone()).await;
    assert_eq!(r.status, ToolStatus::TimedOut);

    let state = ctx.shell.lock().unwrap();
    assert!(
        path_eq(&state.cwd, &workspace.join("a")),
        "timeout 后保持 last confirmed cwd：{}",
        state.cwd
    );
    drop(state);

    // 下一条 bash 仍正常，且沿用已确认 cwd（pwd 指向 a 目录，非 b）。
    let r = run_bash(&ctx, "pwd", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    let stdout_part = r.model_payload.output.split("--- stdout ---").nth(1).unwrap_or("");
    assert!(
        stdout_part.contains("/a") || stdout_part.contains("\\a"),
        "pwd 应指向 a 目录（stdout 段）：{}",
        r.model_payload.output
    );
}

/// cancel 不 commit；之后可用（任务书 §25 Ctrl-C 语义）。
#[tokio::test]
async fn cancel_keeps_last_confirmed_and_recovers() {
    let (_dir, workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p a", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "cd a", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);

    // 长命令，执行中取消。
    let cancel_for_run = CancellationToken::new();
    let cancel_inner = cancel_for_run.clone();
    let handle = {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            run_bash(&ctx, "cd b; sleep 60", 120_000, cancel_inner).await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    cancel_for_run.cancel();
    let r = handle.await.unwrap();
    assert_eq!(r.status, ToolStatus::Cancelled);

    let state = ctx.shell.lock().unwrap();
    assert!(
        path_eq(&state.cwd, &workspace.join("a")),
        "cancel 后保持 last confirmed cwd：{}",
        state.cwd
    );
    drop(state);

    // 下一条 bash 仍正常。
    let r = run_bash(&ctx, "pwd", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
}

/// bash.cwd 显式 override：本次生效，不改变 session cwd（任务书 §15/§16）。
#[tokio::test]
async fn explicit_cwd_is_one_shot_override() {
    let (_dir, workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p a", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "cd a", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);

    // 显式 cwd = workspace root：本次在 root 执行，session cwd 不变。
    let mut ctx2 = ctx.clone();
    ctx2.cancel = cancel.clone();
    let r = bash(
        BashArgs {
            command: "pwd".into(),
            cwd: Some(".".into()),
            timeout_ms: 60_000,
        },
        &ctx2,
    )
    .await;
    assert!(r.status == ToolStatus::Succeeded);
    assert!(
        !r.model_payload.output.contains("a\\b") && !r.model_payload.output.contains("a/b"),
        "override 后应从 workspace root 执行：{}",
        r.model_payload.output
    );

    let state = ctx.shell.lock().unwrap();
    assert!(
        path_eq(&state.cwd, &workspace.join("a")),
        "override 不改变 session cwd：{}",
        state.cwd
    );
}

/// 严格模式（allow_outside_workspace=false）：`cd` 逃出 workspace 不 commit（§17）。
#[tokio::test]
async fn strict_mode_rejects_escape_from_workspace() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = fixtures::test_tool_context(&workspace);
    ctx.allow_outside_workspace = false;
    let cancel = CancellationToken::new();

    // 逃出：cd 到 temp 上级（workspace 外）。
    let _r = run_bash(&ctx, "cd ..", 60_000, cancel.clone()).await;
    // 命令本身会成功（bash 无所谓），但 session cwd 不得更新。
    let state = ctx.shell.lock().unwrap();
    assert!(
        path_eq(&state.cwd, &workspace),
        "严格模式逃逸不得 commit：{}",
        state.cwd
    );
    drop(state);
}

/// 取 model_payload.output 中 `--- stdout ---` 段（避免 program 行等干扰断言）。
fn stdout_part(output: &str) -> &str {
    output.split("--- stdout ---").nth(1).unwrap_or("")
}

/// §18/§59：`export FOO=abc` 后 `echo "$FOO"` 跨调用输出 abc。
#[tokio::test]
async fn exported_env_persists_across_calls() {
    let (_dir, _workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(
        run_bash(&ctx, "export TPI_FOO=abc", 60_000, cancel.clone())
            .await
            .status
            == ToolStatus::Succeeded
    );
    let r = run_bash(&ctx, "echo \"$TPI_FOO\"", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    assert!(
        stdout_part(&r.model_payload.output).contains("abc"),
        "export 后的变量应保持：{}",
        r.model_payload.output
    );

    // overlay 状态确认：TPI_FOO 在 set 中。
    let state = ctx.shell.lock().unwrap();
    assert_eq!(state.env_overlay.set.get("TPI_FOO").map(String::as_str), Some("abc"));
}

/// §18/§59：`unset TPI_FOO` 后变量消失（`${TPI_FOO-unset}` 输出 unset）。
#[tokio::test]
async fn unset_env_persists_across_calls() {
    let (_dir, _workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "export TPI_FOO=abc", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "unset TPI_FOO", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "echo \"${TPI_FOO-unset}\"", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    assert!(
        stdout_part(&r.model_payload.output).contains("unset"),
        "unset 后变量应消失：{}",
        r.model_payload.output
    );

    let state = ctx.shell.lock().unwrap();
    assert!(
        !state.env_overlay.set.contains_key("TPI_FOO"),
        "unset 后 overlay.set 不得残留：{:?}",
        state.env_overlay.set
    );
}

/// 首次执行就 export：baseline 在命令前捕获，overlay 仍能正确记录（§20）。
#[tokio::test]
async fn export_on_first_call_persists() {
    let (_dir, _workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "export TPI_FIRST=yes", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "echo \"$TPI_FIRST\"", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    assert!(
        stdout_part(&r.model_payload.output).contains("yes"),
        "首次 export 也应保持：{}",
        r.model_payload.output
    );
}

/// §20：动态变量（SHLVL/PWD/BASHPID/_ 等）不得污染 overlay。
#[tokio::test]
async fn dynamic_env_vars_do_not_pollute_overlay() {
    let (_dir, _workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    for _ in 0..3 {
        assert!(run_bash(&ctx, "echo hi", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    }
    let state = ctx.shell.lock().unwrap();
    assert!(
        state.env_overlay.set.is_empty(),
        "动态变量不得进 overlay：{:?}",
        state.env_overlay.set
    );
    assert!(
        state.env_overlay.unset.is_empty(),
        "动态变量不得进 unset：{:?}",
        state.env_overlay.unset
    );
}

/// §22：env 捕获段（含 PATH= 等完整 env）不得泄漏到模型输出/artifact。
#[tokio::test]
async fn env_capture_does_not_leak_to_model_output() {
    let (_dir, _workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    let r = run_bash(&ctx, "echo hello", 60_000, cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    let output = &r.model_payload.output;
    assert!(!output.contains("__TPI_CAPTURE_"), "捕获标记不得泄漏：{output}");
    assert!(
        !output.contains("PATH="),
        "完整 env（含 secret 可能）不得泄漏到模型输出：{output}"
    );
}

/// §59 cwd 矩阵完整序列：pwd / cd src / pwd / cd .. / pwd。
#[tokio::test]
async fn cwd_matrix_full_sequence() {
    let (_dir, workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p src", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    // tempdir 名（msys /tmp/.tmpXXX 或 Windows C:\...\.tmpXXX 都以它结尾）。
    let base = workspace.file_name().unwrap().to_string();

    let r = run_bash(&ctx, "pwd", 60_000, cancel.clone()).await;
    assert!(
        stdout_part(&r.model_payload.output).contains(&base),
        "初始 pwd 应指向 workspace：{}",
        r.model_payload.output
    );

    assert!(run_bash(&ctx, "cd src", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "pwd", 60_000, cancel.clone()).await;
    assert!(
        stdout_part(&r.model_payload.output).contains("/src") || stdout_part(&r.model_payload.output).contains("\\src"),
        "cd src 后 pwd 应指向 src：{}",
        r.model_payload.output
    );

    assert!(run_bash(&ctx, "cd ..", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "pwd", 60_000, cancel.clone()).await;
    assert!(
        stdout_part(&r.model_payload.output).contains(&base),
        "cd .. 后应回 workspace root：{}",
        r.model_payload.output
    );

    let state = ctx.shell.lock().unwrap();
    assert!(path_eq(&state.cwd, &workspace), "最终 cwd = workspace root");
}

/// §59 timeout 矩阵的 env 维度：`export FOO=bad; sleep 1000` timeout 后
/// overlay 不得被未完成命令污染（保持 last confirmed env）。
#[tokio::test]
async fn timeout_does_not_commit_unknown_env() {
    let (_dir, _workspace, ctx) = setup();
    let cancel = CancellationToken::new();

    // 先建立已知 overlay。
    assert!(run_bash(&ctx, "export TPI_GOOD=keep", 60_000, cancel.clone()).await.status == ToolStatus::Succeeded);

    // timeout 命令：export 了 TPI_BAD 但命令未正常结束 → 不得 commit。
    let r = run_bash(&ctx, "export TPI_BAD=leak; sleep 1000", 400, cancel.clone()).await;
    assert_eq!(r.status, ToolStatus::TimedOut);

    let state = ctx.shell.lock().unwrap();
    assert_eq!(
        state.env_overlay.set.get("TPI_GOOD").map(String::as_str),
        Some("keep"),
        "已确认 env 必须保留"
    );
    assert!(
        !state.env_overlay.set.contains_key("TPI_BAD"),
        "timeout 命令的 export 不得 commit：{:?}",
        state.env_overlay.set
    );

    // 下一条命令仍正常，且 TPI_GOOD 保持。
    drop(state);
    let r = run_bash(&ctx, "echo \"$TPI_GOOD\"", 60_000, cancel.clone()).await;
    assert!(stdout_part(&r.model_payload.output).contains("keep"));
}

