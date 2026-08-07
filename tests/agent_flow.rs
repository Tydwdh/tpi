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
        &[],
        "hi".into(),
        tx,
        CancellationToken::new(),
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
        &[],
        "read a file".into(),
        tx,
        CancellationToken::new(),
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
