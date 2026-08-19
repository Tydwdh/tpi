//! 命令执行契约测试（对应 §4.2 `tests/process_contract.rs`）。
//!
//! §2.2/§3.2 不变量 5：退出码等判断下一步所需的状态必须进入 `model_payload`，
//! 不能只存在于 UI/session metadata。
//! P2+：Managed Background Process 的集成契约（任务书 §56-§58）。

mod fixtures;

use serial_test::serial;
use tpi::outcome::{ToolOutcome, ToolStatus};
use tpi::process::managed::{ManagedProcessState, ProcessId, wait_process};
use tpi::tool::command::{BashArgs, bash};

#[test]
fn exit_status_is_visible_in_model_payload() {
    let outcome = ToolOutcome::command_failed("cargo", 101);

    // 状态必须对模型可见（§2.2：不能出现"UI 显示 failed，模型只看到一段 stderr"的分叉）。
    assert_eq!(outcome.model_payload.status, ToolStatus::Failed);

    // 退出码必须进入 model payload（§2.2：结构化退出状态不能只在 UI details）。
    assert_eq!(outcome.model_payload.exit_code, Some(101));
    assert!(outcome.model_payload.output.contains("exit_code: 101"));
}

#[test]
fn every_tool_call_has_exactly_one_terminal_status() {
    // §3.2 不变量 3：每个 tool call 恰好产生一个终态结果。
    let outcome = ToolOutcome::command_failed("cargo", 1);
    assert!(matches!(
        outcome.status,
        ToolStatus::Succeeded
            | ToolStatus::Failed
            | ToolStatus::TimedOut
            | ToolStatus::Cancelled
            | ToolStatus::Interrupted
            | ToolStatus::Rejected
    ));
}

/// P2（任务书 §56）：`bash("sleep 5", background=true)` 必须明显少于 5 秒返回
/// `status: running` + `process_id: pN`；进程在后台继续运行（跨工具调用存活）。
#[tokio::test]
#[serial]
async fn background_bash_returns_immediately_and_process_keeps_running() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }

    let start = std::time::Instant::now();
    let outcome = bash(
        BashArgs {
            command: "sleep 5".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "background 启动必须明显快于 sleep 5（§56）：{elapsed:?}"
    );
    // §49/§50：工具调用成功（start succeeded），但模型可见文本是 status: running，
    // 进程本身仍在后台——两个状态分离。
    assert_eq!(
        outcome.status,
        ToolStatus::Succeeded,
        "{} (启动工具调用成功)",
        outcome.model_text()
    );
    let text = outcome.model_text();
    assert!(text.contains("status: running"), "{text}");
    let pid_text = text
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("必须返回逻辑 ProcessId")
        .to_string();
    let process_id: ProcessId = pid_text.parse().expect("p{{n}} 格式");

    // 1 秒后（远小于 5 秒）进程仍在运行，且 registry 记录正确（跨调用存活）。
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let (state, command) = {
        let reg = ctx.processes.lock().unwrap();
        let process = reg.get(process_id).expect("进程已注册到 registry");
        (process.state, process.command.clone())
    };
    assert_eq!(state, ManagedProcessState::Running, "sleep 5 应仍在运行");
    assert_eq!(command, "sleep 5");

    // 等待完成（§20：wait 最多等 timeout；完成返回终态）。
    let terminal = wait_process(
        &ctx.processes,
        process_id,
        std::time::Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        terminal,
        Some(ManagedProcessState::Exited { exit_code: 0 }),
        "sleep 5 正常退出 0"
    );
    // 完整输出进入 artifact（§16 第二层）。artifact finalize 在 drain task
    // 收尾阶段（wait 返回时可能尚未完成），短暂等待其出现。
    let mut artifact = None;
    for _ in 0..50 {
        artifact = ctx
            .processes
            .lock()
            .unwrap()
            .get(process_id)
            .unwrap()
            .artifact
            .clone();
        if artifact.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(artifact.is_some(), "进程完成后必须生成 artifact 引用");
}

/// P2：后台命令非零退出 → Exited{code}（控制状态与程序退出状态分离，§6）。
#[tokio::test]
#[serial]
async fn background_bash_reports_exit_code() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    let outcome = bash(
        BashArgs {
            command: "exit 7".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    let pid_text = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    let process_id: ProcessId = pid_text.parse().unwrap();
    let terminal = wait_process(
        &ctx.processes,
        process_id,
        std::time::Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        terminal,
        Some(ManagedProcessState::Exited { exit_code: 7 }),
        "exit 7 必须如实记录为非零退出，不是 Failed"
    );
}

/// P2：shell `&` 逃逸在 `ManagedProcess` 语义下不被允许——命令内 `&` 只是
/// shell 语法，Job Object 仍拥有整棵进程树；进程结束（或 cancel）时整树被杀。
/// 本测试只验证 background=true 下命令正常完成且状态正确。
#[tokio::test]
#[serial]
async fn background_bash_completes_pipeline() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    let outcome = bash(
        BashArgs {
            command: "echo background-ok".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    let pid_text = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    let process_id: ProcessId = pid_text.parse().unwrap();
    let terminal = wait_process(
        &ctx.processes,
        process_id,
        std::time::Duration::from_secs(10),
    )
    .await;
    assert_eq!(terminal, Some(ManagedProcessState::Exited { exit_code: 0 }));
    // 输出被持续 drain 到 live tail（§15/§16）。
    let tail = {
        let reg = ctx.processes.lock().unwrap();
        String::from_utf8_lossy(&reg.get(process_id).unwrap().tail).into_owned()
    };
    assert!(
        tail.contains("background-ok"),
        "live tail 必须包含输出: {tail}"
    );
}

/// 工作区事务边界不能只覆盖 foreground：后台 Job 结束后也必须把实际
/// 文件 delta 写入 session journal，供 undo/审计使用。
#[tokio::test]
#[serial]
async fn background_bash_commits_workspace_delta_after_exit() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        return;
    }
    let outcome = bash(
        BashArgs {
            command: "printf tracked > generated-by-job.txt".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    let id: ProcessId = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        wait_process(&ctx.processes, id, std::time::Duration::from_secs(10)).await,
        Some(ManagedProcessState::Exited { exit_code: 0 })
    );
    let journal = tpi::session::journal::load_journal(&tpi::session::journal::journal_path(
        &ctx.artifacts_root,
        &ctx.session_id,
    ))
    .unwrap();
    assert!(journal.mutations.iter().any(|mutation| {
        mutation.files.iter().any(|file| {
            file.path.ends_with("generated-by-job.txt")
                && !file.before_exists
                && file.after_exists
                && file.after_content == b"tracked"
        })
    }));
}

/// P4（任务书 §58）：process 工具 status/output 可查看后台进程状态与输出。
#[tokio::test]
#[serial]
async fn process_tool_reports_status_and_output() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    let outcome = bash(
        BashArgs {
            command: "echo ready-line; sleep 4; echo done-line".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    let pid_text = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    let process_id: ProcessId = pid_text.parse().unwrap();

    // 运行中：status 报告 running + 已有输出（live tail，§15 持续 drain）。
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let status = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Status,
            id: Some(pid_text.clone()),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(status.status, ToolStatus::Succeeded);
    let status_text = status.model_text();
    assert!(status_text.contains("status: running"), "{status_text}");
    assert!(
        status_text.contains(&format!("process_id: {process_id}")),
        "{status_text}"
    );

    let output = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Output,
            id: Some(pid_text.clone()),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    let output_text = output.model_text();
    assert!(
        output_text.contains("ready-line"),
        "output 必须包含已产生输出: {output_text}"
    );
    let next_cursor: u64 = output_text
        .lines()
        .find_map(|line| line.strip_prefix("next_cursor: "))
        .expect("output must return a cursor")
        .parse()
        .expect("cursor must be numeric");
    let continued = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Output,
            id: Some(pid_text.clone()),
            timeout_ms: 1000,
            after: Some(next_cursor),
        },
        &ctx,
    )
    .await;
    assert!(
        continued.model_text().contains("after: ")
            && continued.model_text().contains("next_cursor: "),
        "cursor response must remain pageable: {}",
        continued.model_text()
    );

    // wait：等 4 秒 sleep 结束 → 返回 exited 0（任务书 §20：不是错误）。
    let waited = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Wait,
            id: Some(pid_text.clone()),
            timeout_ms: 10_000,
            after: None,
        },
        &ctx,
    )
    .await;
    let waited_text = waited.model_text();
    assert!(waited_text.contains("status: exited"), "{waited_text}");
    assert!(waited_text.contains("exit_code: 0"), "{waited_text}");
}

/// P4：process cancel 终止整棵进程树（§22），状态迁移到 cancelled。
#[tokio::test]
#[serial]
async fn process_cancel_terminates_tree_and_marks_cancelled() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    let outcome = bash(
        BashArgs {
            command: "sleep 60".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    let pid_text = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    let process_id: ProcessId = pid_text.parse().unwrap();
    // 确认已在运行。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        ctx.processes.lock().unwrap().get(process_id).unwrap().state,
        ManagedProcessState::Running
    );

    let cancelled = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Cancel,
            id: Some(pid_text.clone()),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    let cancel_text = cancelled.model_text();
    assert!(
        cancel_text.contains("status: cancelled"),
        "cancel 必须返回 cancelled（§22）：{cancel_text}"
    );
    assert!(
        cancel_text.contains("process_tree_terminated"),
        "{cancel_text}"
    );
    // 状态已迁移到 Cancelled。
    let state = ctx.processes.lock().unwrap().get(process_id).unwrap().state;
    assert_eq!(
        state,
        ManagedProcessState::Cancelled,
        "drain task 必须迁移到 Cancelled"
    );

    // 已结束的进程再次 cancel → rejected（not_running），不伪造成功。
    let again = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Cancel,
            id: Some(pid_text),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(again.status, ToolStatus::Rejected);
    assert!(
        again.model_text().contains("not_running"),
        "{}",
        again.model_text()
    );
}

/// P4：process list 展示全部（含已结束）；未知 id 的 status → rejected。
#[tokio::test]
#[serial]
async fn process_list_and_unknown_id() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    let outcome = bash(
        BashArgs {
            command: "echo listed".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    let pid_text = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    let process_id: ProcessId = pid_text.parse().unwrap();
    // 等它结束。
    let _ = wait_process(
        &ctx.processes,
        process_id,
        std::time::Duration::from_secs(10),
    )
    .await;

    let list = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::List,
            id: None,
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(list.status, ToolStatus::Succeeded);
    assert!(
        list.model_text().contains("processes:"),
        "{}",
        list.model_text()
    );
    assert!(
        list.model_text().contains(&pid_text),
        "list 必须包含刚启动的进程"
    );

    let unknown = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Status,
            id: Some("p99999".into()),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(unknown.status, ToolStatus::Rejected);
    assert!(
        unknown.model_text().contains("not_found"),
        "{}",
        unknown.model_text()
    );
}

/// P4：wait 的 timeout 语义——进程未完成时返回 running（不是错误）。
#[tokio::test]
#[serial]
async fn process_wait_timeout_returns_running_not_error() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    let outcome = bash(
        BashArgs {
            command: "sleep 5".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    let pid_text = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    // 只等 300ms（sleep 5 未结束）→ running，不是错误（§20）。
    let waited = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Wait,
            id: Some(pid_text.clone()),
            timeout_ms: 300,
            after: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(waited.status, ToolStatus::Succeeded);
    assert!(
        waited.model_text().contains("status: running"),
        "{}",
        waited.model_text()
    );

    // 清理：取消，避免测试进程残留。
    let _ = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Cancel,
            id: Some(pid_text),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
}

/// P5（任务书 §59/§9/§10/§44）：background 只继承启动时的 cwd/env 快照，
/// 绝不反向修改 ShellSessionState（否则多个后台进程会竞争写 shell state）。
#[tokio::test]
#[serial]
async fn background_inherits_snapshot_and_never_commits_shell_state() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    std::fs::create_dir_all(workspace.join("sub")).unwrap();

    // 前台设置 session 状态：cwd=sub、export TPI_BG_TEST=1。
    let set = bash(
        BashArgs {
            command: "cd sub && export TPI_BG_TEST=1 && pwd".into(),
            cwd: None,
            timeout_ms: 60_000,
            background: false,
        },
        &ctx,
    )
    .await;
    assert_eq!(set.status, ToolStatus::Succeeded);
    assert!(
        ctx.shell.lock().unwrap().cwd.as_str().ends_with("sub"),
        "前台 cd 必须提交到 session"
    );

    // 后台启动：命令不 cd/export（验证继承），但故意写文件验证 cwd/env 快照。
    let bg = bash(
        BashArgs {
            command: "printf '%s' \"$TPI_BG_TEST\" > inherited.txt; cygpath -w \"$PWD\" > bg-cwd.txt; sleep 1".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    assert_eq!(bg.status, ToolStatus::Succeeded);
    let pid_text = bg
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    let process_id: ProcessId = pid_text.parse().unwrap();
    let _ = wait_process(
        &ctx.processes,
        process_id,
        std::time::Duration::from_secs(10),
    )
    .await;

    // 1) 后台进程继承了启动时的 env 快照（TPI_BG_TEST=1）。文件写在继承的
    //    cwd（workspace/sub）下——这本身就是 cwd 继承的证明。
    let inherited = std::fs::read_to_string(workspace.join("sub/inherited.txt")).unwrap();
    assert_eq!(inherited, "1", "后台必须继承 env snapshot（§9）");
    // 2) 后台进程继承了启动时的 cwd（sub 目录）。
    let bg_cwd = std::fs::read_to_string(workspace.join("sub/bg-cwd.txt")).unwrap();
    let norm = |s: &str| s.trim().to_lowercase().replace('\\', "/");
    assert!(
        norm(&bg_cwd).ends_with("/sub"),
        "后台必须继承 cwd snapshot: {bg_cwd}"
    );

    // 3) 后台运行期间/结束后，session cwd/env 均未被反向修改（§10/§44）。
    let (session_cwd, session_env) = {
        let state = ctx.shell.lock().unwrap();
        (
            state.cwd.to_string(),
            state.env_overlay.set.get("TPI_BG_TEST").cloned(),
        )
    };
    assert!(
        session_cwd.ends_with("sub"),
        "background 不得反向修改 session cwd: {session_cwd}"
    );
    assert_eq!(
        session_env.as_deref(),
        Some("1"),
        "background 不得反向修改 session env（TPI_BG_TEST 应仍为 1）"
    );
}

/// P5：background 命令内部的 cd/export 只影响后台进程自身，不影响 session。
#[tokio::test]
#[serial]
async fn background_inner_cd_export_are_isolated() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    let before = {
        let state = ctx.shell.lock().unwrap();
        (state.cwd.to_string(), state.version)
    };
    let bg = bash(
        BashArgs {
            command: "cd .. && export TPI_ISOLATED=1 && sleep 0.5".into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        &ctx,
    )
    .await;
    assert_eq!(bg.status, ToolStatus::Succeeded);
    let pid_text = bg
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    let process_id: ProcessId = pid_text.parse().unwrap();
    let _ = wait_process(
        &ctx.processes,
        process_id,
        std::time::Duration::from_secs(10),
    )
    .await;

    let after = {
        let state = ctx.shell.lock().unwrap();
        (state.cwd.to_string(), state.version)
    };
    assert_eq!(before.0, after.0, "session cwd 不得被后台 cd 修改");
    assert_eq!(
        before.1, after.1,
        "session version 不得因后台命令递增（无 commit）"
    );
}

/// 检查本机是否有 python（web server 场景依赖；无则跳过）。
fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("-c")
        .arg("print('ok')")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// 启动后台命令并解析 `process_id`。
async fn start_background(ctx: &tpi::tool::ToolContext, command: &str) -> ProcessId {
    let outcome = bash(
        BashArgs {
            command: command.into(),
            cwd: None,
            timeout_ms: 120_000,
            background: true,
        },
        ctx,
    )
    .await;
    assert_eq!(
        outcome.status,
        ToolStatus::Succeeded,
        "{}",
        outcome.model_text()
    );
    let pid_text = outcome
        .model_text()
        .lines()
        .find_map(|line| line.strip_prefix("process_id: "))
        .expect("process_id")
        .to_string();
    pid_text.parse().expect("p{{n}}")
}

/// §65 真实场景（任务书）：后台启动 web server，前台 curl 访问；
/// cancel 后 server 停止；重新启动后再访问成功。Agent 不应等待 server 退出。
#[tokio::test]
#[serial]
async fn web_server_background_lifecycle() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() || !python_available() {
        eprintln!("缺少 Git Bash 或 python，跳过（§65 环境依赖）");
        return;
    }
    let port = 18765;

    // 启动 server（后台，立即返回 process id——不等待退出）。
    let server_id = start_background(&ctx, &format!("python -m http.server {port}")).await;

    // 前台 curl 访问（重试直到 server 就绪）。
    let mut reachable = false;
    for _ in 0..40 {
        let r = bash(
            BashArgs {
                command: format!(
                    "curl -s -o /dev/null -w '%{{http_code}}' http://127.0.0.1:{port}/"
                ),
                cwd: None,
                timeout_ms: 10_000,
                background: false,
            },
            &ctx,
        )
        .await;
        if r.model_text().contains("200") {
            reachable = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(reachable, "后台 server 必须可访问（§65）");

    // cancel server → 整棵进程树终止。
    let cancelled = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Cancel,
            id: Some(server_id.to_string()),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    assert!(
        cancelled.model_text().contains("status: cancelled"),
        "{}",
        cancelled.model_text()
    );

    // server 已停止：curl 应失败（连接拒绝）。
    let mut stopped = false;
    for _ in 0..20 {
        let r = bash(
            BashArgs {
                command: format!("curl -s -m 1 http://127.0.0.1:{port}/ > /dev/null 2>&1"),
                cwd: None,
                timeout_ms: 10_000,
                background: false,
            },
            &ctx,
        )
        .await;
        if r.model_payload.exit_code != Some(0) {
            stopped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(stopped, "cancel 后 server 必须停止（§22 Job tree 终止）");

    // 重新启动 → 再次可访问（Agent 流程：cancel p17 → 启动 p18 → curl 成功）。
    let server2 = start_background(&ctx, &format!("python -m http.server {port}")).await;
    let mut reachable2 = false;
    for _ in 0..40 {
        let r = bash(
            BashArgs {
                command: format!(
                    "curl -s -o /dev/null -w '%{{http_code}}' http://127.0.0.1:{port}/"
                ),
                cwd: None,
                timeout_ms: 10_000,
                background: false,
            },
            &ctx,
        )
        .await;
        if r.model_text().contains("200") {
            reachable2 = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(reachable2, "重启后的 server 必须可访问");
    let _ = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Cancel,
            id: Some(server2.to_string()),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
}

/// §67：多进程并存——registry 正确、输出互不混淆、cancel 一个不影响其他。
#[tokio::test]
#[serial]
async fn multiple_processes_are_isolated() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() || !python_available() {
        eprintln!("缺少 Git Bash 或 python，跳过（§67 环境依赖）");
        return;
    }
    let port = 18766;

    // p1 server、p2 长 sleep、p3 输出标记行。
    // p3 标记行输出完后必须继续运行：后面的检查点（700ms 状态断言、
    // cancel p2 后的隔离断言）都要求 p3 仍 running--脚本自身在 ~0.6s 退出
    // 会让 Exited{0} 与 Running 的断言变成竞态（本机实测必败）。
    let p1 = start_background(&ctx, &format!("python -m http.server {port}")).await;
    let p2 = start_background(&ctx, "sleep 3").await;
    let p3 = start_background(
        &ctx,
        "python -u -c \"import time\nfor i in range(3): print('p3-line-', i); time.sleep(0.2)\ntime.sleep(30)\"",
    )
    .await;

    // 三个进程都在 registry（§67：ProcessRegistry 正确）。
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let states = {
        let reg = ctx.processes.lock().unwrap();
        [p1, p2, p3].map(|id| (id, reg.get(id).map(|p| p.state)))
    };
    for (id, state) in states {
        assert_eq!(state, Some(ManagedProcessState::Running), "{id} 应 running");
    }

    // 输出互不混淆：p3 的标记行只出现在 p3 的 tail。
    let (p1_tail, p3_tail) = {
        let reg = ctx.processes.lock().unwrap();
        (
            String::from_utf8_lossy(&reg.get(p1).unwrap().tail).into_owned(),
            String::from_utf8_lossy(&reg.get(p3).unwrap().tail).into_owned(),
        )
    };
    assert!(
        p3_tail.contains("p3-line-"),
        "p3 输出必须被 drain: {p3_tail}"
    );
    assert!(
        !p1_tail.contains("p3-line-"),
        "p1 输出不得混入 p3: {p1_tail}"
    );

    // cancel p2 不影响 p1/p3。
    let c = tpi::tool::process::process(
        tpi::tool::process::ProcessArgs {
            action: tpi::tool::process::ProcessAction::Cancel,
            id: Some(p2.to_string()),
            timeout_ms: 1000,
            after: None,
        },
        &ctx,
    )
    .await;
    assert!(
        c.model_text().contains("status: cancelled"),
        "{}",
        c.model_text()
    );
    let after = {
        let reg = ctx.processes.lock().unwrap();
        (
            reg.get(p1).map(|p| p.state),
            reg.get(p2).map(|p| p.state),
            reg.get(p3).map(|p| p.state),
        )
    };
    assert_eq!(after.0, Some(ManagedProcessState::Running), "p1 不受影响");
    assert_eq!(after.1, Some(ManagedProcessState::Cancelled), "p2 已取消");
    assert_eq!(after.2, Some(ManagedProcessState::Running), "p3 不受影响");

    // 清理 p1/p3。
    for id in [p1, p3] {
        let _ = tpi::tool::process::process(
            tpi::tool::process::ProcessArgs {
                action: tpi::tool::process::ProcessAction::Cancel,
                id: Some(id.to_string()),
                timeout_ms: 1000,
                after: None,
            },
            &ctx,
        )
        .await;
    }
}

/// P5-06：foreground/background cancel terminal 对齐——都产生 Cancelled 终态。
#[tokio::test]
async fn foreground_and_background_cancel_both_cancelled() {
    use tpi::tool::command::{BashArgs, bash};
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }

    // foreground：先取消再执行 → 立即 Cancelled（不启动进程）。
    ctx.cancel = tokio_util::sync::CancellationToken::new();
    ctx.cancel.cancel();
    let fg = bash(
        BashArgs {
            command: "echo hi".into(),
            cwd: None,
            timeout_ms: 10_000,
            background: false,
        },
        &ctx,
    )
    .await;
    assert_eq!(
        fg.status,
        tpi::outcome::ToolStatus::Cancelled,
        "foreground cancel 对齐"
    );

    // background：取消后启动 → 也是 Cancelled（cancel terminal 对齐）。
    let bg = bash(
        BashArgs {
            command: "sleep 5".into(),
            cwd: None,
            timeout_ms: 10_000,
            background: true,
        },
        &ctx,
    )
    .await;
    assert_eq!(
        bg.status,
        tpi::outcome::ToolStatus::Cancelled,
        "background cancel 对齐"
    );
    // 无残留进程（registry 空）。
    assert_eq!(ctx.processes.lock().unwrap().active_count(), 0);
}

/// P8 gate：100 次 spawn/cancel 无泄漏——registry 每次 cancel 后无残留，
/// 100 次后 `active_count` 归零（进程/任务不泄漏）。
#[tokio::test]
async fn hundred_spawn_cancel_cycles_leave_no_leak() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);
    if tpi::tool::command::locate_git_bash(&ctx).is_none() {
        eprintln!("本机未安装 Git Bash，跳过（§11.2 环境依赖）");
        return;
    }
    for i in 0..100 {
        let outcome = bash(
            BashArgs {
                command: "sleep 60".into(),
                cwd: None,
                timeout_ms: 120_000,
                background: true,
            },
            &ctx,
        )
        .await;
        let pid_text = outcome
            .model_text()
            .lines()
            .find_map(|line| line.strip_prefix("process_id: "))
            .expect("process_id")
            .to_string();
        let process_id: ProcessId = pid_text.parse().unwrap();
        // cancel（不等待自然退出；立即取消，验证 cancel 路径无残留）。
        let cancelled = tpi::tool::process::process(
            tpi::tool::process::ProcessArgs {
                action: tpi::tool::process::ProcessAction::Cancel,
                id: Some(pid_text.clone()),
                timeout_ms: 1000,
                after: None,
            },
            &ctx,
        )
        .await;
        assert_eq!(cancelled.status, tpi::outcome::ToolStatus::Succeeded);
        // 每轮 cancel 后 registry 无残留（状态迁移到 Cancelled 并移除）。
        if let Some(proc) = ctx.processes.lock().unwrap().get(process_id) {
            assert_eq!(
                proc.state,
                tpi::process::managed::ManagedProcessState::Cancelled,
                "第 {i} 轮 cancel 后状态为 Cancelled"
            );
        }
    }
    // 全部 cancel 后：等待 drain task 移除所有已取消进程（最多 5s）。
    for _ in 0..50 {
        if ctx.processes.lock().unwrap().active_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        ctx.processes.lock().unwrap().active_count(),
        0,
        "100 次 spawn/cancel 后 registry 无残留（无泄漏）"
    );
}
