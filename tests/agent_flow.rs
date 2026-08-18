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
            workspace: None,
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
    .expect("run succeeds");

    // 恰好一次请求（§3.2 不变量 2）。
    assert_eq!(provider.request_count, 1);
    assert_eq!(outcome.reason, CompletionReason::Stop);
    assert_eq!(outcome.assistant_text, "done");

    // 每个真实请求先通知 UI 更新运行状态，随后流文本送达。
    let started = rx.recv().await.expect("turn started");
    assert!(matches!(started, agent::LiveEvent::StepStarted { step: 1 }));
    let event = rx.recv().await.expect("one text delta");
    assert!(matches!(
        event,
        agent::LiveEvent::AssistantDelta { text, .. } if text == "done"
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
            workspace: None,
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
    assert_eq!(completed[0].status, tpi::outcome::ToolStatus::Failed);
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
                "items": [
                    {"text": "检查 gcodes", "status": "in_progress"},
                    {"text": "检查组件层", "status": "pending"}
                ]
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
            workspace: None,
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
    .expect("run succeeds");

    assert_eq!(provider.requests.len(), 3);
    // 首次请求：plan 尚未建立（update_plan 是第一个 tool call），不注入当前计划。
    let first_users: Vec<_> = provider.requests[0]
        .messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        first_users,
        vec!["继续审查"],
        "Todo 不得伪装成额外 User 消息"
    );
    assert!(
        !provider.requests[0].messages.iter().any(|m| matches!(
            m,
            ChatMessage::System(text) if text.contains("[当前计划")
        )),
        "首次请求（无 plan）不注入当前计划"
    );

    // 后续请求（plan 已建立）：
    // 1. 不伪装成额外 User 消息；
    // 2. plan 仍以合法 Tool 协议存在于历史；
    // 3. 尾部注入 [当前计划·唯一权威] System 快照供模型每轮可见（§用户诉求）。
    for request in &provider.requests[1..] {
        let users: Vec<_> = request
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::User(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["继续审查"], "Todo 不得伪装成额外 User 消息");
        assert!(
            request.messages.iter().any(|m| matches!(
                m,
                ChatMessage::Tool { name, .. } if name == "update_plan"
            )),
            "update_plan 必须以 Tool 消息存在于历史"
        );
        assert!(
            request.messages.iter().any(|m| matches!(
                m,
                ChatMessage::System(text) if text.contains("[当前计划·唯一权威")
            )),
            "plan 建立后每轮请求尾部必须注入 [当前计划·唯一权威] System 快照"
        );
    }
    assert!(
        provider.requests[2]
            .messages
            .iter()
            .any(|m| matches!(m, ChatMessage::Tool { name, .. } if name == "read")),
        "read Tool 消息仍在历史"
    );
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
            workspace: None,
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
            workspace: None,
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
    .expect("run succeeds");

    let mut saw_usage = false;
    while let Ok(event) = rx.try_recv() {
        if let agent::LiveEvent::ContextUsage { projected, usable } = event {
            saw_usage = true;
            // usable = window - output(未配置) - reserve = 100000 - 8192。
            assert_eq!(usable, 100_000 - 8192);
            assert!(projected > 0, "投影必须非零");
        }
    }
    assert!(saw_usage, "必须发送 ContextUsage 事件");
}

/// §用户诉求：max_model_turns=0（默认）不限制——多轮工具循环正常完成，
/// 不因回合数被截断。
#[tokio::test]
async fn zero_max_turns_is_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    assert_eq!(config.limits.max_model_turns, 0, "默认不限制");
    let mut provider = FakeProvider::new(vec![
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "a.txt"}),
        )]),
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "b.txt"}),
        )]),
        FakeResponse::text("完成"),
    ]);
    std::fs::write(workspace.join("a.txt"), "a").unwrap();
    std::fs::write(workspace.join("b.txt"), "b").unwrap();
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);
    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "多轮".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");
    assert_eq!(
        outcome.reason,
        CompletionReason::Stop,
        "0 = 不限制，应正常走到 Stop（而非 MaxTurns）"
    );
    assert_eq!(provider.requests.len(), 3, "三轮请求都应执行");
}

/// §用户诉求（软着陆）：max_model_turns 已配置时，最后一轮请求注入收尾
/// 指令（harness control metadata），而非硬断。
#[tokio::test]
async fn final_turn_injects_wrapup_instruction() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = test_config(&workspace);
    config.limits.max_model_turns = 2;
    let mut provider = FakeProvider::new(vec![
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "a.txt"}),
        )]),
        FakeResponse::text("收尾"),
    ]);
    std::fs::write(workspace.join("a.txt"), "a").unwrap();
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);
    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "干活".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");
    // 第 2 轮（max=2，turn 2 == max）= 最后一轮 → 注入收尾指令。
    // 第 1 轮（turn 1）是普通工作轮，不注入。
    let second = &provider.requests[1];
    let has_wrapup = second
        .messages
        .iter()
        .any(|m| matches!(m, ChatMessage::System(text) if text.contains("最后一个回合")));
    assert!(
        has_wrapup,
        "最后一轮必须注入收尾指令: {:?}",
        second.messages
    );
    let first = &provider.requests[0];
    assert!(
        !first.messages.iter().any(|m| {
            matches!(m, ChatMessage::System(text) if text.contains("最后一个回合"))
        }),
        "普通轮不得注入收尾指令"
    );
    // 收尾轮后若再调工具，下一轮触发 MaxTurns；本测试第 2 轮直接 stop。
    assert_eq!(outcome.reason, CompletionReason::Stop);
}

/// AGENTS.md §13：`request_input` 使 run 真正挂起（不是完成）——
/// session 记录 `user_input_requested` + `run_completed(AwaitingUserInput)`，
/// outcome 携带模型的问题文本。
#[tokio::test]
async fn request_input_suspends_run_with_durable_event() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![FakeResponse::with_tool_calls(vec![
        fixtures::fake_provider::tool_call(
            "request_input",
            serde_json::json!({"question": "要运行完整测试套件吗？"}),
        ),
    ])]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "需要你确认一下".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");

    // 挂起：reason + 问题文本（渲染格式：编号前缀）。
    assert_eq!(outcome.reason, CompletionReason::AwaitingUserInput);
    assert_eq!(
        outcome.awaiting_input.as_ref().map(|a| a.text.as_str()),
        Some("1. 要运行完整测试套件吗？")
    );

    // session 事实：request_input 是工具调用（ToolRequested/ToolCompleted），
    // 之后 user_input_requested + run_completed(AwaitingUserInput)。
    let events = read_events(session.path()).expect("read session");
    let types: Vec<&str> = events.iter().map(SessionEvent::type_name).collect();
    assert!(
        types.contains(&"user_input_requested"),
        "缺少 user_input_requested: {types:?}"
    );
    assert!(types.contains(&"tool_requested"), "{types:?}");
    assert!(types.contains(&"tool_completed"), "{types:?}");
    let last = events.last().expect("run_completed");
    assert!(matches!(
        last,
        SessionEvent::RunCompleted {
            reason: CompletionReason::AwaitingUserInput,
            ..
        }
    ));
}

/// AGENTS.md §13：挂起后用户回答 → `user_input_received` 记录，
/// 后续 run 带着完整历史继续（reason 正常完成）。
#[tokio::test]
async fn resume_after_suspend_records_input_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);

    // 第一次 run：模型请求输入 → 挂起。
    let mut provider = FakeProvider::new(vec![FakeResponse::with_tool_calls(vec![
        fixtures::fake_provider::tool_call(
            "request_input",
            serde_json::json!({"question": "要跑测试吗？"}),
        ),
    ])]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);
    let outcome1 = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "确认一下".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");
    assert_eq!(outcome1.reason, CompletionReason::AwaitingUserInput);

    // 模拟 app 层：用户回答 → 先记录 UserInputReceived（durable 事实）。
    session
        .append_event(&SessionEvent::UserInputReceived {
            content: "跑吧".into(),
        })
        .and_then(|_| session.sync_data())
        .expect("append user input received");

    // 第二次 run：resume（history 从 session 重建；user_message = 回答）。
    let history = tpi::session::replay_messages(session.path()).expect("replay");
    let mut provider2 = FakeProvider::new(vec![FakeResponse::text("好的，开始跑测试")]);
    let (tx2, _rx2) = mpsc::channel(32);
    let outcome2 = agent::run(
        &mut provider2,
        &mut session,
        &config,
        agent::RunInput {
            history: &history,
            user_message: "跑吧".into(),
            ui: tx2,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("resume run succeeds");
    assert_eq!(outcome2.reason, CompletionReason::Stop);
    assert!(outcome2.awaiting_input.is_none());

    // session 完整保留请求/回答事件对；resume 的模型请求能看到
    // request_input 的工具结果与用户的回答（上下文连续）。
    let events = read_events(session.path()).expect("read session");
    let types: Vec<&str> = events.iter().map(SessionEvent::type_name).collect();
    assert!(
        types.contains(&"user_input_requested") && types.contains(&"user_input_received"),
        "请求/回答事件对必须都保留: {types:?}"
    );
    let resume_request = provider2.requests.last().expect("resume request");
    let joined: Vec<String> = resume_request
        .messages
        .iter()
        .map(|m| match m {
            ChatMessage::User(text) | ChatMessage::System(text) => text.clone(),
            ChatMessage::Assistant { content, .. } => content.clone(),
            ChatMessage::Tool { content, .. } => content.clone(),
        })
        .collect();
    let joined = joined.join("\n");
    assert!(
        joined.contains("要跑测试吗"),
        "resume 上下文应含问题: {joined}"
    );
    assert!(
        joined.contains("跑吧"),
        "resume 上下文应含用户回答: {joined}"
    );
}

/// AGENTS.md §13（对标 AskUserQuestion）：`request_input` 一次请求多个问题
/// （各带 header/options）——渲染后的挂起问题文本包含全部问题、标题与选项，
/// session 记录 `user_input_requested`；旧单问题格式仍兼容。
#[tokio::test]
async fn request_input_multi_question_suspends_with_rendered_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![FakeResponse::with_tool_calls(vec![
        fixtures::fake_provider::tool_call(
            "request_input",
            serde_json::json!({
                "questions": [
                    {
                        "question": "要运行完整测试套件吗？",
                        "options": ["是，运行全部", "只跑单元测试", "跳过"]
                    },
                    {
                        "question": "发布到哪个环境？",
                        "header": "部署",
                        "options": ["生产", "staging"]
                    }
                ]
            }),
        ),
    ])]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "需要你确认几件事".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");

    assert_eq!(outcome.reason, CompletionReason::AwaitingUserInput);
    let awaiting = outcome.awaiting_input.expect("挂起信息");
    let prompt = &awaiting.text;
    // 全部问题 + header + 选项进入渲染文本（编号、分组、选项行）。
    assert!(prompt.contains("要运行完整测试套件吗"), "prompt: {prompt}");
    assert!(prompt.contains("只跑单元测试"), "prompt: {prompt}");
    assert!(prompt.contains("部署"), "header 应渲染: {prompt}");
    assert!(prompt.contains("发布到哪个环境"), "prompt: {prompt}");
    assert!(prompt.contains("staging"), "prompt: {prompt}");
    // 两个问题都编号。
    assert!(
        prompt.contains("1. ") && prompt.contains("2. "),
        "prompt: {prompt}"
    );
    // 结构化 questions 供 TUI 选择器使用：全部问题 + 选项保留。
    assert_eq!(awaiting.questions.len(), 2, "结构化问题数");
    assert_eq!(
        awaiting.questions[0]
            .options
            .iter()
            .map(|o| o.label())
            .collect::<Vec<_>>(),
        vec!["是，运行全部", "只跑单元测试", "跳过"]
    );
    assert_eq!(awaiting.questions[1].header.as_deref(), Some("部署"));
    assert_eq!(
        awaiting.questions[1]
            .options
            .iter()
            .map(|o| o.label())
            .collect::<Vec<_>>(),
        vec!["生产", "staging"]
    );

    // session 事实：user_input_requested 记录渲染后的完整问题文本。
    let events = read_events(session.path()).expect("read session");
    let requested = events.iter().find_map(|event| match event {
        SessionEvent::UserInputRequested { prompt } => Some(prompt.as_str()),
        _ => None,
    });
    assert!(requested.is_some(), "缺少 user_input_requested");
    assert!(
        requested.unwrap().contains("发布到哪个环境"),
        "durable prompt 应含全部问题: {:?}",
        requested
    );
}

/// AGENTS.md §13：旧单问题格式（question + options）仍可用，
/// 渲染文本包含问题与建议选项（兼容回归）。
#[tokio::test]
async fn request_input_legacy_single_question_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![FakeResponse::with_tool_calls(vec![
        fixtures::fake_provider::tool_call(
            "request_input",
            serde_json::json!({
                "question": "要运行完整测试套件吗？",
                "options": ["是", "否"]
            }),
        ),
    ])]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "确认一下".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");

    assert_eq!(outcome.reason, CompletionReason::AwaitingUserInput);
    let awaiting = outcome.awaiting_input.expect("挂起信息");
    let prompt = &awaiting.text;
    assert!(prompt.contains("要运行完整测试套件吗"), "prompt: {prompt}");
    assert!(
        prompt.contains("是") && prompt.contains("否"),
        "prompt: {prompt}"
    );
    // 兼容格式同样产出结构化问题（供选择器）。
    assert_eq!(awaiting.questions.len(), 1);
    assert_eq!(
        awaiting.questions[0]
            .options
            .iter()
            .map(|o| o.label())
            .collect::<Vec<_>>(),
        vec!["是", "否"]
    );
}

/// §13 回归：非交互 run（interactive=false，`-p`/web）中 `request_input`
/// 被拒绝而不是挂起——否则 run 挂起后无人可答，永远等不到输入。
#[tokio::test]
async fn request_input_rejected_in_non_interactive_run() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    // 第一轮：模型尝试 request_input（会被拒绝）；第二轮：正常收尾。
    let mut provider = FakeProvider::new(vec![
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "request_input",
            serde_json::json!({"question": "要跑测试吗？"}),
        )]),
        FakeResponse::text("没有用户可答，我基于现有信息继续"),
    ]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "跑一下".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");

    // 不挂起：正常完成（模型拿到拒绝结果后继续）。
    assert_eq!(outcome.reason, CompletionReason::Stop);
    assert!(outcome.awaiting_input.is_none(), "非交互不得挂起");
    // 拒绝信息已送达模型（第二轮请求能看到）。
    let joined: Vec<String> = provider.requests[1]
        .messages
        .iter()
        .map(|m| match m {
            ChatMessage::User(text) | ChatMessage::System(text) => text.clone(),
            ChatMessage::Assistant { content, .. } => content.clone(),
            ChatMessage::Tool { content, .. } => content.clone(),
        })
        .collect();
    let joined = joined.join("\n");
    assert!(
        joined.contains("unavailable_in_non_interactive_run"),
        "模型应看到拒绝原因: {joined}"
    );
    // session 事实：request_input 是 Rejected 工具结果，且没有挂起事件。
    let events = read_events(session.path()).expect("read session");
    let types: Vec<&str> = events.iter().map(SessionEvent::type_name).collect();
    assert!(!types.contains(&"user_input_requested"), "{types:?}");
    let rejected = events.iter().any(|event| {
        matches!(
            event,
            SessionEvent::ToolCompleted { outcome, .. }
                if outcome.status == tpi::outcome::ToolStatus::Rejected
                    && outcome.session_metadata.tool == "request_input"
        )
    });
    assert!(rejected, "request_input 必须是 Rejected 结果: {types:?}");
}

/// §13 回归：挂起时同一批中**后续 wave** 的工具调用不再执行，但必须有
/// 对应的 ToolRequested/ToolCompleted（Rejected）——否则 resume 重建的
/// 消息序列里 assistant.tool_calls 缺少 tool result，provider 拒绝。
/// 构造：request_input（Pure，wave 1）+ bash（WorkspaceUnknown，wave 2）。
#[tokio::test]
async fn suspend_persists_rejected_outcomes_for_later_waves() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![FakeResponse::with_tool_calls(vec![
        fixtures::fake_provider::tool_call(
            "request_input",
            serde_json::json!({"question": "继续吗？"}),
        ),
        fixtures::fake_provider::tool_call(
            "bash",
            serde_json::json!({"command": "echo never runs"}),
        ),
    ])]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(32);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "确认".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
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
    .expect("run succeeds");

    assert_eq!(outcome.reason, CompletionReason::AwaitingUserInput);

    // session 事实：两个工具都有完整生命周期（bash 是 Rejected）。
    let events = read_events(session.path()).expect("read session");
    let requested: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolRequested { call } => Some(call.name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        requested.contains(&"request_input") && requested.contains(&"bash"),
        "两个调用都必须有 ToolRequested: {requested:?}"
    );
    let bash_completed = events.iter().any(|event| {
        matches!(
            event,
            SessionEvent::ToolCompleted { outcome, .. }
                if outcome.session_metadata.tool == "bash"
                    && outcome.status == tpi::outcome::ToolStatus::Rejected
                    && outcome.model_payload.output.contains("run_suspended_before_execution")
        )
    });
    assert!(bash_completed, "bash 必须持久化为 Rejected（未执行）");

    // resume 重放：每条 assistant tool_call 都有对应 tool result（协议合法）。
    let history = tpi::session::replay_messages(session.path()).expect("replay");
    let assistant_calls: usize = history
        .iter()
        .map(|m| match m {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.len(),
            _ => 0,
        })
        .sum();
    let tool_results: usize = history
        .iter()
        .filter(|m| matches!(m, ChatMessage::Tool { .. }))
        .count();
    assert_eq!(
        assistant_calls, tool_results,
        "assistant tool_calls 与 tool result 必须一一对应（否则 resume 序列非法）: {history:?}"
    );
}
