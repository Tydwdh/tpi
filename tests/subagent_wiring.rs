//! ADR-007 接线回归：spawn_agent worker 的 report 必须进入**与 parent run
//! drain 同一个共享 AgentManager**（修复前 composition root 创建的 manager 与
//! run 现场新建的 manager 是两个实例，inbox 永远为空，"报告自动注入"失效）。
//!
//! child provider 指向本地 mock SSE server（真实 HTTP 路径，无外网依赖）。
//! 同时覆盖 wait 唤醒契约：worker settle 后 `agent action=wait` 在有陘时间内
//! 返回终态。

mod fixtures;

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use fixtures::fake_provider::{FakeProvider, FakeResponse, tool_call};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::ids::RunId;
use tpi::provider::openai_compat::OpenAiCompatClient;
use tpi::session::SessionLog;
use tpi_agent::agent::manager::{AgentManager, AgentState};
use tpi_agent::subagent::async_tool::register_async_subagent_tools;

/// 本地 mock SSE server：返回一段固定文本响应（child 调查"成功"）。
async fn mock_sse_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                // 读请求头直到 \r\n\r\n。
                loop {
                    let Ok(n) = socket.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"调查完成\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n",
                );
                let response =
                    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{body}");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn test_manager() -> Arc<Mutex<AgentManager>> {
    Arc::new(Mutex::new(AgentManager::new()))
}

/// spawn_agent 工具执行 → 后台 worker 完成 → 共享 manager 收到 report 且
/// agent 进入终态。（接线语义：工具与 drain 侧共用同一实例——本测试直接断言
/// 该实例收到数据；修复前 worker 写入的是另一个实例，这里恒为空。）
#[tokio::test]
async fn spawn_worker_report_lands_in_shared_manager_inbox() {
    // child provider 直连本地 mock：绕过系统代理（reqwest system-proxy feature
    // 会把 127.0.0.1 交给代理，连接失败后重试预算长达数分钟）。
    // SAFETY: edition 2024 中 env::set_var 是 unsafe；测试早期调用、
    // tokio 测试 runtime 尚未创建其他读环境变量的线程。
    unsafe { std::env::set_var("NO_PROXY", "*") };
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = fixtures::test_config(&workspace);
    config.limits.max_model_turns = 2;
    let config = Arc::new(config);

    let manager = test_manager();
    let registry = Arc::new(Mutex::new(tpi::tool::registry::builtin_registry()));
    // 与生产路径一致：工厂闭包每次调用创建独立 provider 实例（共享 client 缓存）。
    let url_for_factory = mock_sse_server().await;
    register_async_subagent_tools::<OpenAiCompatClient, _>(
        &registry,
        config.clone(),
        move || {
            OpenAiCompatClient::new(
                url_for_factory.clone(),
                "fake".into(),
                "unused-key".into(),
                None,
                None,
                None,
            )
        },
        manager.clone(),
        None,
    );

    // 通过 agent loop 执行一次 spawn_agent 调用（真实工具路径）。
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    // parent provider：第一轮返回 spawn_agent 调用；第二轮 Stop 结束。
    let mut provider = FakeProvider::new(vec![
        FakeResponse::with_tool_calls(vec![tool_call(
            "spawn_agent",
            serde_json::json!({ "instruction": "调查 src 目录结构" }),
        )]),
        FakeResponse::text("done"),
    ]);

    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel(256);
    let drain = tokio::spawn(async move { while ui_rx.recv().await.is_some() {} });

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "发起后台调查".into(),
            ui: ui_tx,
            cancel: CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: None,
            registry: registry.clone(),
            processes: Arc::new(Mutex::new(tpi::process::managed::ProcessRegistry::new())),
            terminals: Arc::new(Mutex::new(tpi::terminal::TerminalRegistry::default())),
            // 关键：与 register_async_subagent_tools 注入的是同一实例。
            agents: manager.clone(),
        },
    )
    .await
    .expect("run 成功");
    assert_eq!(
        outcome.reason,
        tpi::session::CompletionReason::Stop,
        "两轮脚本应正常结束"
    );
    drain.abort();

    // spawn 已注册至少一个 agent。
    let views = manager.lock().unwrap().list();
    assert!(!views.is_empty(), "spawn_agent 应注册 agent");

    // 等 worker 完成：agent 到达终态且 last_summary 非空（有陘等待，防 CI 慢机器）。
    let mut last_summary: Option<String> = None;
    let mut reached_stopped = false;
    for i in 0..200 {
        let views = manager.lock().unwrap().list();
        if let Some(v) = views.iter().find(|v| v.state == AgentState::Stopped) {
            reached_stopped = true;
            last_summary = v.last_summary.clone();
            break;
        }
        if i == 199 {
            panic!("worker 10s 内未到达 Stopped 终态: {views:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(reached_stopped, "worker 应到达 Stopped 终态");
    let summary = last_summary.expect("终态 agent 应携带 report summary");
    assert!(
        summary.contains("调查完成"),
        "child 报告应包含调查结论: {summary}"
    );

    // 端到端核心断言（Fix 1 回归）：parent run 的 deterministic boundary 必须
    // 从**同一个共享 manager** drain 出该 report 并写入 durable 事件。
    // 修复前 worker 写入的是另一个实例，parent session 中永远没有此事件。
    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, tpi::session::SessionEvent::SubagentReported { .. })),
        "parent session 必须包含 durable SubagentReported（自动注入接线生效）"
    );
}

/// wait 唤醒契约：对已 Running 的 agent 调 wait，settle 后必须立即返回，
/// 不能挂到超时（per-agent notify 回归）。
#[tokio::test]
async fn agent_control_wait_returns_after_settle_on_shared_manager() {
    let manager = test_manager();
    let agent_id = {
        let mut m = manager.lock().unwrap();
        m.register(
            tpi::ids::SessionId::new_v7(),
            "wait 契约".into(),
            CancellationToken::new(),
        )
        .unwrap()
    };
    let delegation_id = tpi::ids::DelegationId::new_v7();
    {
        let mut m = manager.lock().unwrap();
        m.mark_running(agent_id);
        m.add_delegation(tpi_agent::agent::manager::Delegation {
            id: delegation_id,
            child_agent: agent_id,
            child_session: tpi::ids::SessionId::new_v7(),
            state: tpi_agent::agent::manager::DelegationState::Running,
        });
    }
    let m2 = manager.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        m2.lock().unwrap().settle(
            delegation_id,
            agent_id,
            tpi_agent::agent::manager::AgentState::Stopped,
            None,
        );
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        AgentManager::wait(&manager, agent_id, CancellationToken::new()),
    )
    .await
    .expect("wait 必须在 3s 内返回（不能挂到默认 timeout）");
    assert!(result.is_some());
}
