//! 取消与超时契约测试（§11.3/§11.5；§21 M1：Ctrl-C 取消 provider/tool）。
//!
//! timeout 和 cancellation 是独立状态，不伪装成 exit code 1（§11.3）；
//! Ctrl-C 第一次取消当前请求/工具，保留 session（§11.5）。

mod fixtures;

use camino::Utf8PathBuf;
use tokio_util::sync::CancellationToken;
use tpi::tool::command::{RunArgs, run};
use tpi::tool::outcome::ToolStatus;

/// 取消 token 触发后，长时间运行的命令以 Cancelled 结束（§11.5：取消是独立状态）。
#[tokio::test]
async fn cancellation_terminates_running_command_with_cancelled_status() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let cancel = CancellationToken::new();
    let mut ctx = fixtures::test_tool_context(&workspace);
    ctx.cancel = cancel.clone();
    let args = RunArgs {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 30".into(),
        ],
        cwd: ".".into(),
        timeout_ms: 60_000,
        env: Default::default(),
    };

    let handle = tokio::spawn(async move { run(args, &ctx).await });
    // 等命令真正启动后再取消（模拟第一次 Ctrl-C）。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel();
    let outcome = handle.await.expect("run task completes");

    assert_eq!(
        outcome.status,
        ToolStatus::Cancelled,
        "取消必须产生独立状态，不能伪装成 exit code 1（§11.3）"
    );
    assert!(outcome.model_payload.output.contains("status: cancelled"));
}

/// timeout 是独立状态：超时后命令以 TimedOut 结束。
#[tokio::test]
async fn timeout_terminates_command_with_timed_out_status() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    let args = RunArgs {
        program: "powershell.exe".into(),
        args: vec![
            "-NoProfile".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 30".into(),
        ],
        cwd: ".".into(),
        timeout_ms: 500,
        env: Default::default(),
    };

    let outcome = run(args, &ctx).await;
    assert_eq!(outcome.status, ToolStatus::TimedOut);
    assert!(outcome.model_payload.output.contains("status: timed_out"));
}
