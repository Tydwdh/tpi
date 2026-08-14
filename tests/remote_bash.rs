//! R1 Remote bash 集成测试（§35-§36：bash 按 ActiveWorkspace 分发到远端，
//! 远端 cwd/env 持久，与 Local 同一 ShellSessionState 语义）。
//!
//! 测试 server 的 exec 在测试进程内 `bash -c` 执行，cwd 前缀指向 tempdir，
//! 因此"远端文件系统"即 tempdir。

mod fixtures;

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use tokio_util::sync::CancellationToken;
use tpi::remote::ssh::{HostKeyDecision, RemoteHost};
use tpi::remote::RemoteWorkspace;
use tpi::tool::command::{bash, BashArgs};
use tpi::tool::outcome::ToolStatus;
use tpi::workspace::ActiveWorkspace;

/// 启动 server + 确认 host key + 构造 Remote ToolContext。
async fn setup_remote_ctx() -> (tempfile::TempDir, tpi::tool::ToolContext) {
    fixtures::point_host_at_real_tpi();
    let (port, root, known_hosts) = fixtures::remote_server::start_test_server().await;

    // 先确认 host key（§34：未知 host 由"用户"确认一次）。
    let mut probe = fixtures::remote_server::test_client(port, &known_hosts).await;
    assert_eq!(probe.connect().await.unwrap(), HostKeyDecision::UnknownPending);
    probe.confirm_host_key().unwrap();
    probe.disconnect().await;

    let mut host = RemoteHost::direct("127.0.0.1", port, "test");
    host.known_hosts_path = known_hosts;
    host.password = Some(fixtures::remote_server::TEST_PASSWORD.into());
    // 远端 root 用 POSIX 形式（远端是 Linux，路径本就 POSIX；测试 server 的
    // exec 在本地 Git Bash 执行，cd 用 msys 路径才能与 $PWD 保持一致）。
    let posix = std::process::Command::new("cygpath")
        .arg("-u")
        .arg(root.path())
        .output()
        .expect("cygpath 可用");
    let root_posix = String::from_utf8(posix.stdout).expect("utf8").trim().to_string();
    let root_path = Utf8PathBuf::from(root_posix);
    let remote = RemoteWorkspace::new(host, root_path.clone());
    let active = ActiveWorkspace::remote(remote.clone());

    let ctx = tpi::tool::ToolContext {
        workspace_root: root_path,
        allow_outside_workspace: true,
        cancel: CancellationToken::new(),
        artifacts_root: root.path().join("artifacts"),
        session_id: "remote-test".into(),
        call_id: tpi::ids::ToolCallId::new_v7(),
        output_tx: None,
        scan_snapshots: Arc::new(Mutex::new(std::collections::HashMap::new())),
        shell_path: None,
        snapshot_store: Arc::new(Mutex::new(tpi::tool::edit::SnapshotStore::new(16, 4))),
        current_plan: Arc::new(Mutex::new(None)),
        shell: remote.shell.clone(),
        workspace: Arc::new(Mutex::new(active)),
        processes: Arc::new(Mutex::new(tpi::process::managed::ProcessRegistry::new())),
        registry: tpi::tool::registry::global_registry(),
        interactive: false,
    };
    (root, ctx)
}

async fn run_bash(
    ctx: &tpi::tool::ToolContext,
    command: &str,
    cancel: CancellationToken,
) -> tpi::tool::outcome::ToolOutcome {
    let mut ctx = ctx.clone();
    ctx.cancel = cancel;
    bash(
        BashArgs {
            command: command.into(),
            cwd: None,
            timeout_ms: 60_000,
            background: false,
        },
        &ctx,
    )
    .await
}

fn stdout_part(output: &str) -> &str {
    output.split("--- stdout ---").nth(1).unwrap_or("")
}

/// §36：远端 `cd` 跨调用保持；`pwd` 反映远端 cwd。
#[tokio::test]
async fn remote_cd_persists_across_calls() {
    let (root, ctx) = setup_remote_ctx().await;
    let cancel = CancellationToken::new();
    let base = root.path().file_name().unwrap().to_string_lossy().into_owned();

    assert!(run_bash(&ctx, "mkdir -p scripts", cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "cd scripts", cancel.clone()).await.status == ToolStatus::Succeeded);

    let r = run_bash(&ctx, "pwd", cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    let out = stdout_part(&r.model_payload.output);
    assert!(
        out.contains(&base) && (out.contains("/scripts") || out.contains("\\scripts")),
        "远端 pwd 应指向 scripts：{out}"
    );

    // session cwd 已更新为远端 scripts。
    let state = ctx.shell.lock().unwrap();
    assert!(state.cwd.as_str().ends_with("scripts"), "session cwd = {}", state.cwd);
}

/// §36：远端 `export` 跨调用保持；unset 后消失。
#[tokio::test]
async fn remote_export_and_unset_persist() {
    let (_root, ctx) = setup_remote_ctx().await;
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "export TPI_FOO=abc", cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "echo \"$TPI_FOO\"", cancel.clone()).await;
    assert!(stdout_part(&r.model_payload.output).contains("abc"));

    assert!(run_bash(&ctx, "unset TPI_FOO", cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "echo \"${TPI_FOO-unset}\"", cancel.clone()).await;
    assert!(
        stdout_part(&r.model_payload.output).contains("unset"),
        "unset 后应消失：{}",
        r.model_payload.output
    );
}

/// §22 语义在远端：capture 段（env/cwd）不得泄漏到模型输出。
#[tokio::test]
async fn remote_capture_does_not_leak() {
    let (_root, ctx) = setup_remote_ctx().await;
    let cancel = CancellationToken::new();

    let r = run_bash(&ctx, "echo hello", cancel.clone()).await;
    assert!(r.status == ToolStatus::Succeeded);
    let output = &r.model_payload.output;
    assert!(!output.contains("__TPI_CAPTURE_"), "捕获标记不得泄漏：{output}");
    assert!(!output.contains("PATH="), "完整 env 不得泄漏：{output}");
}

/// §13：失败命令（exit 非 0）仍 commit 远端 cwd。
#[tokio::test]
async fn remote_failed_command_still_commits_cwd() {
    let (_root, ctx) = setup_remote_ctx().await;
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p a/b", cancel.clone()).await.status == ToolStatus::Succeeded);
    let r = run_bash(&ctx, "cd a/b; false", cancel.clone()).await;
    assert_eq!(r.status, ToolStatus::Failed);
    let state = ctx.shell.lock().unwrap();
    assert!(
        state.cwd.as_str().ends_with("a/b"),
        "失败命令也应 commit cwd：{}",
        state.cwd
    );
}

/// §35：bash.cwd 显式 override 只对本次生效（远端同样语义）。
#[tokio::test]
async fn remote_explicit_cwd_is_one_shot() {
    let (root, ctx) = setup_remote_ctx().await;
    let cancel = CancellationToken::new();

    assert!(run_bash(&ctx, "mkdir -p a", cancel.clone()).await.status == ToolStatus::Succeeded);
    assert!(run_bash(&ctx, "cd a", cancel.clone()).await.status == ToolStatus::Succeeded);

    // 显式 cwd = root：本次在 root 执行（POSIX 形式，与远端一致）。
    let mut ctx2 = ctx.clone();
    ctx2.cancel = cancel.clone();
    let root_posix = ctx.workspace_root.to_string();
    let r = bash(
        BashArgs {
            command: "pwd".into(),
            cwd: Some(root_posix),
            timeout_ms: 60_000,
            background: false,
        },
        &ctx2,
    )
    .await;
    let out = stdout_part(&r.model_payload.output);
    let base = root.path().file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        out.contains(&base) && !out.contains("/a") && !out.contains("\\a"),
        "override 应在 root 执行：{out}"
    );

    // session cwd 仍是 a。
    let state = ctx.shell.lock().unwrap();
    assert!(state.cwd.as_str().ends_with("a"), "session cwd 不变：{}", state.cwd);
}
