//! Agent 状态机契约测试（对应 §4.2 tests/agent_flow.rs）。
//!
//! §3.2 不变量 2：一个 provider response 对应一次明确的状态转换；
//! `finish=stop` 且无 tool call 时 run 立即结束，不自动补一次模型请求。
//! §20.2 场景 11：provider `finish=stop` 后不发生幽灵般的额外模型调用。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::fake_provider::{FakeProvider, FakeResponse};
use fixtures::test_config;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::ids::RunId;
use tpi::provider::ChatMessage;
use tpi::session::{CompletionReason, SessionEvent, SessionLog, read_events};

#[tokio::test]
async fn finish_stop_without_tool_calls_completes_run_without_second_request() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![FakeResponse::text("done")]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "hi".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
        },
    )
    .await
    .expect("run succeeds");

    // 恰好一次请求（§3.2 不变量 2）。
    assert_eq!(provider.request_count, 1);
    assert_eq!(outcome.reason, CompletionReason::Stop);
    assert_eq!(outcome.assistant_text, "done");

    // 每个真实请求先通知 UI 更新运行状态，随后流文本送达。
    let started = rx.recv().await.expect("turn started");
    assert!(matches!(
        started,
        agent::RuntimeEvent::TurnStarted { turn: 1 }
    ));
    let event = rx.recv().await.expect("one text delta");
    assert!(matches!(
        event,
        agent::RuntimeEvent::AssistantDelta { text, .. } if text == "done"
    ));

    // session 事实：UserSubmitted → RunStarted → AssistantMessageCommitted → RunCompleted。
    let events = read_events(session.path()).expect("read session");
    let types: Vec<&str> = events.iter().map(SessionEvent::type_name).collect();
    assert_eq!(
        types,
        vec![
            "user_submitted",
            "run_started",
            "assistant_message_committed",
            "run_completed",
        ]
    );
}

#[tokio::test]
async fn tool_call_loop_terminates_and_reports_completion() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "missing.txt"}),
        )]),
        FakeResponse::text("finally"),
    ]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(16);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "read a file".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
        },
    )
    .await
    .expect("run succeeds");

    // 两次请求：一次工具调用 + 一次最终回答。
    assert_eq!(provider.request_count, 2);
    assert_eq!(outcome.reason, CompletionReason::Stop);

    // 工具失败是模型可处理的 ToolOutcome（§19.1），不是 RunFailure。
    let events = read_events(session.path()).expect("read session");
    let completed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCompleted { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].status, tpi::tool::outcome::ToolStatus::Failed);
    assert!(completed[0].model_payload.output.contains("not_found"));
}

#[tokio::test]
async fn todo_state_is_not_reinjected_as_a_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "update_plan",
            serde_json::json!({
                "explanation": "审查顺序",
                "items": ["检查 gcodes", "检查组件层"]
            }),
        )]),
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "missing.txt"}),
        )]),
        FakeResponse::text("审查完成"),
    ]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);

    agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "继续审查".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
        },
    )
    .await
    .expect("run succeeds");

    assert_eq!(provider.requests.len(), 3);
    for request in &provider.requests {
        let users: Vec<_> = request
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::User(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["继续审查"], "Todo 不得伪装成额外 User 消息");
    }
    assert!(matches!(
        provider.requests[1].messages.last(),
        Some(ChatMessage::Tool { name, .. }) if name == "update_plan"
    ));
    assert!(matches!(
        provider.requests[2].messages.last(),
        Some(ChatMessage::Tool { name, .. }) if name == "read"
    ));
}

/// §2.2/§12.4：多轮 tool-call 循环的 usage 必须跨请求累加（RunCompleted 记录总用量）。
#[tokio::test]
async fn usage_accumulates_across_turns() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![
        // 第一轮：工具调用（usage 100+20）。
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "missing.txt"}),
        )])
        .with_usage(100, 20),
        // 第二轮：最终回答（usage 80+10）。
        FakeResponse::text("done").with_usage(80, 10),
    ]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(16);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "hi".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
        },
    )
    .await
    .expect("run succeeds");

    assert_eq!(outcome.reason, CompletionReason::Stop);
    // 总用量 = 100+20 + 80+10 = 180 input / 30 output（覆盖式赋值时只剩第二轮 80/10）。
    assert_eq!(outcome.usage.input_tokens, 180, "input 必须跨轮累加");
    assert_eq!(outcome.usage.output_tokens, 30, "output 必须跨轮累加");

    let events = read_events(session.path()).expect("read session");
    let completed = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::RunCompleted { usage, .. } => Some(*usage),
            _ => None,
        })
        .expect("run completed event");
    assert_eq!(completed.input_tokens, 180, "session 记录的 usage 必须累加");
    assert_eq!(completed.output_tokens, 30);
}

/// 上下文用量事件：配置 context_window 时每次请求前发送 ContextUsage（TUI 用量条）。
#[tokio::test]
async fn context_usage_event_sent_before_requests() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = test_config(&workspace);
    // 窗口足够大，不触发 compaction（只验证 ContextUsage 事件）。
    config.model.context_window = Some(100_000);
    let mut provider = FakeProvider::new(vec![FakeResponse::text("done")]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);

    let _outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "hi".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
        },
    )
    .await
    .expect("run succeeds");

    let mut saw_usage = false;
    while let Ok(event) = rx.try_recv() {
        if let agent::RuntimeEvent::ContextUsage { projected, usable } = event {
            saw_usage = true;
            // usable = window - output(未配置) - reserve = 100000 - 8192。
            assert_eq!(usable, 100_000 - 8192);
            assert!(projected > 0, "投影必须非零");
        }
    }
    assert!(saw_usage, "必须发送 ContextUsage 事件");
}
