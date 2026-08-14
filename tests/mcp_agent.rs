//! Phase 5 端到端测试（README2 §15/§32）：MCP 工具在 **Agent Loop** 里
//! 被模型调用并执行——Discovery → Tool Call → Result → LLM 闭环；
//! 以及 ToolSelector 按上下文选择 MCP 工具（不一次塞给 LLM）。

mod fixtures;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::provider::ToolCall;
use tpi::session::{SessionLog, CompletionReason};

use fixtures::fake_provider::{FakeProvider, FakeResponse};

fn tool_call(name: &str, args: serde_json::Value) -> ToolCall {
    fixtures::fake_provider::tool_call(name, args)
}

fn test_server_config(name: &str) -> tpi::mcp::config::McpServerConfig {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/mcp_test_server.py");
    tpi::mcp::config::McpServerConfig {
        name: name.to_string(),
        command: "python".into(),
        args: vec![path.to_string_lossy().into_owned()],
        env: Default::default(),
        enabled: true,
        timeout: std::time::Duration::from_secs(5),
    }
}

/// §15/§32：MCP 工具在 agent loop 被调用并执行成功。
///
/// 流程：注册测试 MCP server → agent 请求（FakeProvider 调用
/// mcp::e2e-server::echo）→ adapter 执行 → 结果返回模型 → 完成。
#[tokio::test]
async fn mcp_tool_executes_inside_agent_loop() {
    // 注册 MCP server 到全局 registry（agent 的 ToolRuntime 读取同一目录）。
    let mut manager = tpi::mcp::manager::McpManager::new();
    let registry = tpi::tool::registry::global_registry();
    {
        manager
            .start_server(test_server_config("e2e-server"))
            .await
            .unwrap();
    }
    assert!(registry.lock().unwrap().get("mcp::e2e-server::echo").is_some());

    // agent 请求：FakeProvider 依次调用 MCP echo → 收到结果 → 完成。
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = fixtures::test_config(&workspace);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        tpi::ids::RunId::new_v7(),
    )
    .unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let mut provider = FakeProvider::scripted(vec![
        Box::new(|_req| {
            FakeResponse::with_tool_calls(vec![tool_call(
                "mcp::e2e-server::echo",
                serde_json::json!({"text": "hello from agent"}),
            )])
        }),
        Box::new(|req| {
            // 模型看到 echo 结果后完成。
            let saw_result = req.messages.iter().any(|m| match m {
                tpi::provider::ChatMessage::Tool { content, .. } => {
                    content.contains("echo: hello from agent")
                }
                _ => false,
            });
            FakeResponse::text(if saw_result {
                "MCP 工具执行成功"
            } else {
                "未看到工具结果"
            })
        }),
    ]);

    let outcome = tpi::agent::run(
        &mut provider,
        &mut session,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "调用 MCP echo 工具返回 hello from agent".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: None,
        },
    )
    .await
    .unwrap();
    drain.abort();
    let _ = rx;

    assert_eq!(outcome.reason, CompletionReason::Stop);
    // 模型看到了 MCP 工具的真实结果（闭环验证）。
    assert!(
        provider.requests.len() >= 2,
        "至少两轮请求（工具调用 + 完成）：{}",
        provider.requests.len()
    );

    // 清理：卸载测试 server 工具 + 关闭进程（避免污染其他测试/留孤儿）。
    {
        let mut guard = registry.lock().unwrap();
        for name in ["mcp::e2e-server::echo", "mcp::e2e-server::add", "mcp::e2e-server::fail", "mcp::e2e-server::sleep"] {
            guard.unregister(name);
        }
    }
    manager.shutdown_all().await;
}

/// §14：ToolSelector 生效——MCP 工具按上下文选择，无关 MCP 不发给模型。
#[tokio::test]
async fn tool_selector_filters_irrelevant_mcp_tools_from_model() {
    // 注册两个 server（工具名带明确语义）。
    let mut manager = tpi::mcp::manager::McpManager::new();
    let registry = tpi::tool::registry::global_registry();
    {
        manager
            .start_server(test_server_config("ctx-a"))
            .await
            .unwrap();
        manager
            .start_server(test_server_config("ctx-b"))
            .await
            .unwrap();
    }

    // agent 请求：上下文提到 "sleep"（ctx-a 的 sleep 工具相关；ctx-b 无关）。
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = fixtures::test_config(&workspace);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        tpi::ids::RunId::new_v7(),
    )
    .unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let mut provider = FakeProvider::new(vec![FakeResponse::text("完成")]);
    let _ = tpi::agent::run(
        &mut provider,
        &mut session,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "请调用 sleep 工具休眠 100ms 后继续".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: None,
        },
    )
    .await
    .unwrap();
    drain.abort();
    let _ = rx;

    // 第一条请求的工具列表：sleep 相关工具在；ctx-b 的无关工具（add）被过滤。
    // ctx-b::sleep 描述也含 sleep（两个 server 工具名相同），匹配属预期。
    let first = &provider.requests[0];
    let tool_names: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        tool_names.contains(&"mcp::ctx-a::sleep"),
        "相关 MCP 工具必须在：{tool_names:?}"
    );
    assert!(
        !tool_names.iter().any(|n| n == &"mcp::ctx-b::add"),
        "无关 MCP 工具被过滤：{tool_names:?}"
    );
    // builtin 始终保留。
    assert!(tool_names.contains(&"read") && tool_names.contains(&"bash"));

    // 清理。
    {
        let mut guard = registry.lock().unwrap();
        let names: Vec<String> = guard
            .list()
            .iter()
            .filter(|t| t.name().starts_with("mcp::ctx-"))
            .map(|t| t.name().to_string())
            .collect();
        for name in names {
            guard.unregister(&name);
        }
    }
    manager.shutdown_all().await;
}
