//! R4 端到端 Agent 测试（任务书 §63/§64）：Agent 在 **Remote Workspace**
//! 上完整跑一个真实任务——写脚本、运行、读结果，全程不出现 ssh/scp。
//!
//! 用 loopback russh server 充当"远端机器"，其临时目录即远端 FS；
//! FakeProvider 脚本驱动 Agent 按合理顺序调用工具（list/read/write/bash/read）。

mod fixtures;

use camino::Utf8PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::provider::ToolCall;
use tpi::remote::RemoteWorkspace;
use tpi::remote::ssh::{HostKeyDecision, RemoteHost};
use tpi::session::SessionLog;
use tpi::workspace::ActiveWorkspace;

use fixtures::fake_provider::{FakeProvider, FakeResponse};
use fixtures::test_config;

fn tool_call(name: &str, args: serde_json::Value) -> ToolCall {
    fixtures::fake_provider::tool_call(name, args)
}

/// 启动 server + 确认 host key + 构造 remote ActiveWorkspace。
/// 返回 (tempdir_remote_fs, workspace)。
async fn setup_remote_workspace() -> (tempfile::TempDir, tpi::workspace::ActiveWorkspace) {
    let (port, root, known_hosts) = fixtures::remote_server::start_test_server().await;
    let mut probe = fixtures::remote_server::test_client(port, &known_hosts).await;
    assert_eq!(
        probe.connect().await.unwrap(),
        HostKeyDecision::UnknownPending
    );
    probe.confirm_host_key().unwrap();
    probe.disconnect().await;

    let mut host = RemoteHost::direct("127.0.0.1", port, "test");
    host.known_hosts_path = known_hosts;
    host.password = Some(fixtures::remote_server::TEST_PASSWORD.into());
    // 远端 root：POSIX（远端 Linux；测试 server exec 在本地 Git Bash 执行）；
    // 纯 Rust 转换，与 fixture server 用同一实现，不依赖 cygpath。
    let root_posix = fixtures::remote_server::win_to_posix(root.path());
    let remote = RemoteWorkspace::new(host, Utf8PathBuf::from(root_posix));
    (root, ActiveWorkspace::remote(remote))
}

/// §63：Agent 在远端写脚本分析日志并运行验证。
///
/// 用户诉求（模拟远端机器）：
/// "project/logs/input.log 里有多种错误，写一个脚本统计每种错误数量，
/// 输出 result.csv，并运行验证。"
///
/// FakeProvider 脚本：list logs → read input.log → write analyze.sh →
/// bash bash analyze.sh → read result.csv → 完成。
#[tokio::test]
async fn agent_solves_task_on_remote_workspace() {
    let (remote_fs, remote_workspace) = setup_remote_workspace().await;
    // 远端已有 logs/input.log。
    let logs = remote_fs.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("input.log"),
        b"error: network_timeout\nok\nerror: auth_failed\nerror: network_timeout\nwarning: x\n",
    )
    .unwrap();

    let config = test_config(&remote_fs.path().to_path_buf().try_into().unwrap());
    let mut session = SessionLog::create(
        &config.sessions_root,
        remote_fs.path(),
        tpi::ids::RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let mut provider = FakeProvider::scripted(vec![
        // 1. 列出 logs 目录。
        Box::new(move |_req| {
            FakeResponse::with_tool_calls(vec![tool_call(
                "list",
                serde_json::json!({"path": "logs", "depth": 2}),
            )])
            .with_text("先看看远端 logs 目录")
        }),
        // 2. 读取 input.log。
        Box::new(|_req| {
            FakeResponse::with_tool_calls(vec![tool_call(
                "read",
                serde_json::json!({"path": "logs/input.log"}),
            )])
        }),
        // 3. 写 analyze.sh（统计错误并输出 result.csv）。
        Box::new(|_req| {
            FakeResponse::with_tool_calls(vec![tool_call(
                "write",
                serde_json::json!({
                    "path": "analyze.sh",
                    "content": "grep -o 'error: [a-z_]*' logs/input.log | sort | uniq -c > result.csv\n"
                }),
            )])
        }),
        // 4. 运行脚本。
        Box::new(|_req| {
            FakeResponse::with_tool_calls(vec![tool_call(
                "bash",
                serde_json::json!({"command": "bash analyze.sh", "timeout_ms": 60000}),
            )])
        }),
        // 5. 读取结果。
        Box::new(|_req| {
            FakeResponse::with_tool_calls(vec![tool_call(
                "read",
                serde_json::json!({"path": "result.csv"}),
            )])
        }),
        // 6. 汇报完成。
        Box::new(|_req| FakeResponse::text("统计完成，结果已写入 result.csv")),
    ]);

    let outcome = tpi::agent::run(
        &mut provider,
        &mut session,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "在远程机器上统计 logs/input.log 中每种错误的数量，写出脚本并运行验证。"
                .into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: Some(remote_workspace),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::process::managed::ProcessRegistry::new(),
            )),
            terminals: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::terminal::TerminalRegistry::default(),
            )),

            agents: std::sync::Arc::new(std::sync::Mutex::new(

                tpi_agent::agent::manager::AgentManager::new(),

            )),
        },
    )
    .await
    .expect("agent run 成功");

    drain.abort();
    let _ = rx;

    // 1. 任务完成（stop）。
    assert_eq!(outcome.reason, tpi::session::CompletionReason::Stop);
    assert!(
        outcome.assistant_text.contains("result.csv"),
        "汇报含结果：{}",
        outcome.assistant_text
    );

    // 2. result.csv 在**远端 FS**（server tempdir）真实生成——证明 bash 走了
    //    remote executor（而非本地）。
    let result_csv = remote_fs.path().join("result.csv");
    assert!(result_csv.is_file(), "result.csv 必须生成在远端 FS");
    let content = std::fs::read_to_string(&result_csv).unwrap();
    assert!(content.contains("network_timeout"), "统计内容：{content}");
    assert!(content.contains("auth_failed"), "统计内容：{content}");

    // 3. 工具调用序列正确（list/read/write/bash/read），无 ssh/scp。
    //    历史消息会重放旧 tool calls，因此每条请求只取**最后一个**
    //    Assistant 消息（即本轮新增的工具调用）。
    let mut tools_seen = Vec::new();
    for request in &provider.requests {
        let last_assistant = request.messages.iter().rev().find_map(|m| match m {
            tpi::provider::ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                Some(tool_calls.clone())
            }
            _ => None,
        });
        if let Some(calls) = last_assistant {
            for call in calls {
                tools_seen.push(call.name);
            }
        }
    }
    assert!(
        tools_seen.iter().any(|t| t == "bash"),
        "必须经过 bash 工具：{tools_seen:?}"
    );
    assert!(
        !tools_seen
            .iter()
            .any(|t| t.contains("ssh") || t.contains("scp")),
        "trajectory 不得出现 ssh/scp（§63）：{tools_seen:?}"
    );
    // 顺序：list → read → write → bash → read。
    assert_eq!(
        tools_seen,
        vec!["list", "read", "write", "bash", "read"],
        "工具顺序应符合远端任务流程：{tools_seen:?}"
    );
}

/// §53：模型请求中必须包含 workspace identity（Agent Context 注入）。
#[tokio::test]
async fn agent_request_includes_workspace_identity() {
    let (_remote_fs, remote_workspace) = setup_remote_workspace().await;
    let config = test_config(
        &tempfile::tempdir()
            .unwrap()
            .path()
            .to_path_buf()
            .try_into()
            .unwrap(),
    );
    let mut session = SessionLog::create(
        &config.sessions_root,
        tempfile::tempdir().unwrap().path(),
        tpi::ids::RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let mut provider = FakeProvider::new(vec![
        FakeResponse::with_tool_calls(vec![tool_call(
            "bash",
            serde_json::json!({"command": "echo hi", "timeout_ms": 30000}),
        )]),
        FakeResponse::text("完成"),
    ]);

    let _ = tpi::agent::run(
        &mut provider,
        &mut session,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "hi".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: Some(remote_workspace),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::process::managed::ProcessRegistry::new(),
            )),
            terminals: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::terminal::TerminalRegistry::default(),
            )),

            agents: std::sync::Arc::new(std::sync::Mutex::new(

                tpi_agent::agent::manager::AgentManager::new(),

            )),
        },
    )
    .await
    .expect("run 成功");
    drain.abort();
    let _ = rx;

    // 第一条请求的 system 消息含 workspace identity（§53）。
    let first = &provider.requests[0];
    let has_workspace = first.messages.iter().any(|m| match m {
        tpi::provider::ChatMessage::System(s) => {
            s.contains("[当前 workspace]") && s.contains("ssh:127.0.0.1:")
        }
        _ => false,
    });
    assert!(has_workspace, "请求必须带 workspace identity（§53）");
}
