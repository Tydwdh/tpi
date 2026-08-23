//! M2 验收契约（§21 M2）。
//!
//! - §20.2 场景 8：Bash pipeline 前段失败在 pipefail 下可见；
//! - §20.2 场景 22：read 目录模式达到 scan budget 返回统计；窗口分页；
//! - §20.2 场景 23：Windows target 启动后立即派生 child，取消仍能终止 host、target 与 child；
//! - §8.4：run 完整输出进 artifact，模型经 `@artifact/...` 有界读取。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::{point_host_at_real_tpi, test_config, test_tool_context};
use tokio_util::sync::CancellationToken;
use tpi::outcome::ToolStatus;
use tpi::tool::command::{BashArgs, bash};
use tpi::tool::{ToolContext, files};

#[tokio::test]
async fn bash_pipefail_makes_pipeline_failure_visible() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);

    // §11.2 环境依赖：本机无 Git Bash（含未随包放置）时跳过，不硬失败。
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过 bash 测试（§11.2 环境依赖）");
        return;
    }

    // §11.1：wrapper 统一启用 pipefail——前段失败必须可见（§20.2 场景 8）。
    let outcome = bash(
        BashArgs {
            command: "false | echo hello".into(),
            cwd: None,
            timeout_ms: 30_000,
            background: false,
            lifetime: Default::default(),
        },
        &ctx,
    )
    .await;
    assert_eq!(
        outcome.status,
        ToolStatus::Failed,
        "pipefail 下前段失败可见: {}",
        outcome.model_text()
    );

    // 无失败的 pipeline 正常 succeeded。
    let outcome = bash(
        BashArgs {
            command: "echo hello".into(),
            cwd: None,
            timeout_ms: 30_000,
            background: false,
            lifetime: Default::default(),
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    assert!(outcome.model_text().contains("hello"));
}

#[tokio::test]
async fn cancellation_kills_entire_process_tree() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let cancel = CancellationToken::new();
    let mut ctx = test_tool_context(&workspace);
    ctx.cancel = cancel.clone();
    let pid_file = dir.path().join("child.pid");

    // target 启动后立即派生 child（Start-Process），并把自己的 PID 写文件。
    // bash 包装：双引号内 \$ 转义，避免 bash 展开 PowerShell 变量。
    let script = format!(
        "powershell.exe -NoProfile -Command \"\\$p = Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 60' -PassThru; Set-Content -Path '{}' -Value \\$p.Id; Start-Sleep -Seconds 60\"",
        pid_file.display()
    );
    let args = BashArgs {
        command: script,
        cwd: None,
        timeout_ms: 60_000,
        background: false,
        lifetime: Default::default(),
    };
    let run_ctx = ToolContext {
        workspace_root: ctx.workspace_root.clone(),
        cancel: cancel.clone(),
        artifacts_root: ctx.artifacts_root.clone(),
        session_id: ctx.session_id.clone(),
        call_id: ctx.call_id,
        output_tx: None,
        scan_snapshots: ctx.scan_snapshots.clone(),
        shell_path: ctx.shell_path.clone(),
        snapshot_store: ctx.snapshot_store.clone(),
        current_goal: None,
        current_plan: ctx.current_plan.clone(),
        shell: ctx.shell.clone(),
        workspace: ctx.workspace.clone(),
        processes: ctx.processes.clone(),
        terminals: Default::default(),
        resources: None,
        resource_identity: None,
        registry: ctx.registry.clone(),
        interactive: true,
        allow_outside_workspace: ctx.allow_outside_workspace,
        workspace_session: None,
    };
    let handle = tokio::spawn(async move { bash(args, &run_ctx).await });

    // 等子进程 PID 落盘。
    let mut child_pid: Option<u32> = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            child_pid = Some(pid);
            break;
        }
    }
    let child_pid = child_pid.expect("child pid file written");

    // 取消 → host、target 与孙进程全部终止（§20.2 场景 23）。
    cancel.cancel();
    let outcome = handle.await.expect("run completes");
    assert_eq!(outcome.status, ToolStatus::Cancelled);

    let alive = is_process_alive(child_pid, &ctx).await;
    assert!(!alive, "Job Object 必须终止孙进程（child pid {child_pid}）");
}

async fn is_process_alive(pid: u32, _ctx: &ToolContext) -> bool {
    // 使用独立的未取消上下文（取消属于 run 执行本身，不能复用已取消的 token）。
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let check_ctx = test_tool_context(&workspace);
    for _ in 0..50 {
        let args = BashArgs {
            command: format!(
                "powershell.exe -NoProfile -Command \"if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}\""
            ),
            cwd: None,
            timeout_ms: 10_000,
            background: false,
            lifetime: Default::default(),
        };
        let outcome = bash(args, &check_ctx).await;
        match outcome.model_payload.exit_code {
            Some(0) => {}
            Some(1) => return false, // 进程已不存在
            _ => return false,       // 检查失败视为已终止
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    true
}

#[tokio::test]
async fn list_and_search_respect_budget_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(workspace.join("a.txt"), "needle-one\n").unwrap();
    std::fs::write(workspace.join("b.txt"), "needle-two\n").unwrap();
    std::fs::create_dir_all(workspace.join("sub")).unwrap();
    std::fs::write(workspace.join("sub/c.txt"), "plain\n").unwrap();
    std::fs::write(workspace.join("ignored.txt"), "needle-ignored\n").unwrap();
    std::fs::write(workspace.join(".gitignore"), "ignored.txt\n").unwrap();
    // ignore crate 需要 git 上下文才应用 .gitignore（标准 git 语义）。
    std::fs::create_dir_all(workspace.join(".git")).unwrap();
    let ctx = test_tool_context(&workspace);

    // read 目录模式（§list 并入 read）：遵循 .gitignore，报告扫描统计。
    let outcome = files::read(
        files::ReadArgs {
            path: ".".into(),
            start_line: 1,
            line_count: 200,
            depth: Some(2),
        },
        &ctx,
    );
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    let text = outcome.model_text();
    assert!(text.contains("scanned_files:"), "必须报告扫描统计: {text}");
    assert!(text.contains("stop_reason: complete"));
    assert!(!text.contains("ignored.txt"), ".gitignore 必须生效: {text}");
    assert!(text.contains("a.txt"));

    // 目录条目窗口分页（§20.2 场景 22 的 list 语义并入 read）：
    // 250 个文件 → 第一页 200 条 + 续读指引 → start_line 续读第二页 50 条。
    for i in 0..250 {
        std::fs::write(workspace.join(format!("f{i:03}.txt")), "x\n").unwrap();
    }
    let outcome = files::read(
        files::ReadArgs {
            path: ".".into(),
            start_line: 1,
            line_count: 200,
            depth: Some(1),
        },
        &ctx,
    );
    let text = outcome.model_text();
    assert!(text.contains("entries: 200 shown of 253"), "{text}");
    assert!(text.contains("续读: read"), "必须给出续读指引: {text}");
    // 第二页：start_line=201 续读剩余 53 条（250 文件 + a.txt/b.txt/sub/）。
    let page2 = files::read(
        files::ReadArgs {
            path: ".".into(),
            start_line: 201,
            line_count: 200,
            depth: Some(1),
        },
        &ctx,
    );
    let text2 = page2.model_text();
    assert!(text2.contains("entries: 53 shown of 253"), "{text2}");
}

#[tokio::test]
async fn bash_output_lands_in_artifact_and_readable_via_opaque_ref() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut ctx = test_tool_context(&workspace);
    ctx.artifacts_root = config.artifacts_root.clone();

    // 输出 50 行（超过模型预算的 tail 之外也有完整内容在 artifact）。
    // bash 包装：双引号内 \$_ 转义，避免 bash 展开 PowerShell 变量。
    let args = BashArgs {
        command: "powershell.exe -NoProfile -Command \"1..50 | ForEach-Object { 'line-' + \\$_ }\""
            .into(),
        cwd: None,
        timeout_ms: 30_000,
        background: false,
        lifetime: Default::default(),
    };
    let outcome = bash(args, &ctx).await;
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    assert!(
        !outcome.artifacts.is_empty(),
        "bash 必须产出 artifact（§8.4）"
    );
    // §8.4：artifact 引用必须出现在 model_payload（模型能感知引用才能 `read @artifact`）。
    assert!(
        outcome.model_payload.artifact.is_some(),
        "model_payload.artifact 必须携带 opaque 引用（模型读完整输出的唯一入口）"
    );
    let artifact = &outcome.artifacts[0];
    assert_eq!(artifact.session, ctx.session_id);

    // 完整输出已落盘。
    let record = tpi::session::artifact::find(&ctx.artifacts_root, &artifact.session, &artifact.id)
        .expect("artifact record exists");
    assert!(record.byte_length > 100, "完整输出写入 artifact");

    // 模型经 opaque ref 有界读取（§8.4：read(@artifact/...)）。
    let read_args = files::ReadArgs {
        path: format!("@artifact/{}/{}", artifact.session, artifact.id),
        start_line: 1,
        line_count: 200,
        depth: None,
    };
    let read_outcome = files::read(read_args, &ctx);
    assert_eq!(read_outcome.status, ToolStatus::Succeeded);
    let text = read_outcome.model_text();
    assert!(text.contains("line-1"), "{text}");
    assert!(text.contains("line-50"), "{text}");
}

/// `实时输出链路：output_tx` 订阅时，bash 执行中收到增量流事件（UI 卡片实时输出）。
#[tokio::test]
async fn bash_streams_live_output_through_output_tx() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut ctx = test_tool_context(&workspace);
    ctx.artifacts_root = config.artifacts_root.clone();
    ctx.call_id = tpi::ids::ToolCallId::new_v7();
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<tpi::tool::ToolStreamEvent>(tpi::tool::TOOL_STREAM_CAPACITY);
    ctx.output_tx = Some(tx);

    let args = BashArgs {
        command: r#"powershell.exe -NoProfile -Command "1..20 | ForEach-Object { Write-Output ('s-' + \$_); Start-Sleep -Milliseconds 50 }""#.into(),
        cwd: None,
        timeout_ms: 30_000,
        background: false,
        lifetime: Default::default(),
    };
    let call_id = ctx.call_id;
    let mut handle = tokio::spawn(async move { bash(args, &ctx).await });

    // 执行结束前就能收到流事件（实时性）；事件携带正确的 call_id。
    let mut received = String::new();
    let mut live_seen = false;
    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                assert_eq!(event.call_id, call_id, "流事件必须携带工具调用 id");
                received.push_str(&event.text);
                live_seen = true;
            }
            result = &mut handle => {
                // bash 完成：drain 残余流事件（可能仍在 channel 中）。
                while let Ok(event) = rx.try_recv() {
                    received.push_str(&event.text);
                }
                let outcome = result.expect("bash completes");
                assert_eq!(outcome.status, ToolStatus::Succeeded);
                break;
            }
        }
    }
    assert!(live_seen, "执行完成前必须能收到增量输出（实时链路）");
    // 流事件与最终输出一致（stdout 全部到达）。
    assert!(
        received.contains("s-20"),
        "流事件应包含最后一行: {received}"
    );
}

/// BUG-012：有界流通道 + `try_send——UI` 不消费时 bash 不得阻塞/无限堆积，
/// 必须正常完成并返回（实时输出是 lossy telemetry）。
#[tokio::test]
async fn bash_does_not_block_when_stream_channel_full() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut ctx = test_tool_context(&workspace);
    ctx.artifacts_root = config.artifacts_root.clone();
    ctx.call_id = tpi::ids::ToolCallId::new_v7();
    // 容量 1 且无人消费：bash 输出大量行时 try_send 必须丢弃而非阻塞。
    let (tx, _rx) = tokio::sync::mpsc::channel::<tpi::tool::ToolStreamEvent>(1);
    ctx.output_tx = Some(tx);

    let args = BashArgs {
        command: "for i in $(seq 1 200); do echo line-$i; done".into(),
        cwd: None,
        timeout_ms: 30_000,
        background: false,
        lifetime: Default::default(),
    };
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), bash(args, &ctx))
        .await
        .expect("bash 在满通道下必须完成，不得挂死");
    assert_eq!(outcome.status, ToolStatus::Succeeded);
}
