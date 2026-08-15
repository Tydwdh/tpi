//! P2-02：SessionStore port 的 in-memory fake + adapter 契约测试。
//!
//! 验收：
//! - in-memory fake 跑 agent_flow（agent 通过 `S: SessionStore` 泛型访问，
//!   不依赖 JSONL/文件）；
//! - 单写者 / seq / recovery 契约对 adapter（SessionLog）运行。

mod fixtures;

use std::path::Path;

use tpi::ids::{RunId, SessionId, ToolCallId};
use tpi::provider::ToolCall;
use tpi::session::protocol::{RecoveryMetadata, SessionEvent};
use tpi::session::store::{SessionLog, SessionStore};
use tpi::tool::outcome::StoredToolOutcome;

/// in-memory SessionStore：事件存 Vec，seq 单调；无文件/锁。
/// 单写者由 `&mut self` 借用保证。
struct InMemoryStore {
    events: Vec<SessionEvent>,
    seq: u64,
    session_id: SessionId,
    run_id: RunId,
}

impl InMemoryStore {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            seq: 0,
            session_id: SessionId::new_v7(),
            run_id: RunId::new_v7(),
        }
    }
    fn events(&self) -> &[SessionEvent] {
        &self.events
    }
}

impl SessionStore for InMemoryStore {
    fn begin_run(&mut self) -> RunId {
        self.run_id = RunId::new_v7();
        self.run_id
    }
    fn session_id(&self) -> SessionId {
        self.session_id
    }
    fn seq(&self) -> u64 {
        self.seq
    }
    fn path(&self) -> &Path {
        // 无文件：占位路径（fake 不持久化）。
        Path::new(":memory:")
    }
    fn append_event(&mut self, event: &SessionEvent) -> std::io::Result<u64> {
        self.events.push(event.clone());
        self.seq = self.seq.saturating_add(1);
        Ok(self.seq)
    }
    fn sync_data(&mut self) -> std::io::Result<()> {
        Ok(()) // in-memory 无落盘
    }
    fn write_ahead_tool(
        &mut self,
        call_id: ToolCallId,
        recovery: Option<RecoveryMetadata>,
    ) -> std::io::Result<()> {
        self.append_event(&SessionEvent::ToolStarted { call_id, recovery })?;
        self.sync_data()
    }
    fn complete_tool(
        &mut self,
        call_id: ToolCallId,
        outcome: &StoredToolOutcome,
    ) -> std::io::Result<()> {
        self.append_event(&SessionEvent::ToolCompleted {
            call_id,
            outcome: outcome.clone(),
        })?;
        self.sync_data()
    }
    fn events_with_seq(&self) -> std::io::Result<Vec<(u64, SessionEvent)>> {
        Ok(self
            .events
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, e)| (i as u64 + 1, e))
            .collect())
    }
}

fn tool_call(id: u128) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::from_u128(id),
        provider_id: format!("p{id}"),
        name: "bash".into(),
        arguments: r#"{"command":"echo hi"}"#.into(),
    }
}

fn fake_outcome() -> StoredToolOutcome {
    StoredToolOutcome {
        status: tpi::tool::outcome::ToolStatus::Succeeded,
        session_metadata: tpi::tool::outcome::ToolMetadata {
            tool: "bash".into(),
            target: None,
            program: None,
            timeout_ms: None,
            diff: None,
        },
        model_payload: tpi::tool::outcome::ModelPayload {
            status: tpi::tool::outcome::ToolStatus::Succeeded,
            program: Some("bash".into()),
            exit_code: Some(0),
            duration_ms: 5,
            output: "ok".into(),
            effect: None,
            artifact: None,
        },
    }
}

// ---- 契约测试 1：seq 单调（对 adapter 运行）----

#[test]
fn adapter_seq_is_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut log = SessionLog::create(
        &dir.path().join("sessions"),
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    let mut prev = 0u64;
    for i in 0..5 {
        let seq = log
            .append_event(&SessionEvent::UserSubmitted {
                content: format!("m{i}"),
            })
            .unwrap();
        assert!(seq > prev, "seq 必须严格递增");
        prev = seq;
    }
    assert_eq!(log.seq(), 5);
    // 持久化后可读回（adapter 真实写盘）。
    let events = tpi::session::read_events(log.path()).unwrap();
    assert_eq!(events.len(), 5);
}

// ---- 契约测试 2：write-ahead（adapter：ToolStarted 先于副作用持久化）----

#[test]
fn adapter_write_ahead_then_complete() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut log = SessionLog::create(
        &dir.path().join("sessions"),
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    let call = tool_call(1);
    // 协议顺序：ToolRequested → ToolStarted（write-ahead）→ ToolCompleted。
    log.append_event(&SessionEvent::ToolRequested { call: call.clone() })
        .unwrap();
    log.write_ahead_tool(
        call.call_id,
        Some(RecoveryMetadata {
            tool: "bash".into(),
            target_path: "/tmp/x".into(),
            expected_revision: "abc".into(),
            candidate_revision: None,
            temp_path: "/tmp/t".into(),
            backup_path: None,
        }),
    )
    .unwrap();
    // 副作用后
    log.complete_tool(call.call_id, &fake_outcome()).unwrap();

    let events = tpi::session::read_events(log.path()).unwrap();
    assert!(matches!(events[0], SessionEvent::ToolRequested { .. }));
    assert!(matches!(events[1], SessionEvent::ToolStarted { .. }));
    assert!(matches!(events[2], SessionEvent::ToolCompleted { .. }));
    assert_eq!(log.seq(), 3);
}

// ---- 契约测试 3：in-memory fake 跑 agent_flow ----

#[tokio::test]
async fn fake_store_runs_agent_flow() {
    use tpi::provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderResponse};

    struct EchoProvider;
    impl Provider for EchoProvider {
        fn model_name(&self) -> &str {
            "echo"
        }
        async fn stream(
            &mut self,
            _request: tpi::provider::ModelRequest,
            events: tokio::sync::mpsc::Sender<ProviderEvent>,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<ProviderResponse, ProviderError> {
            events
                .send(ProviderEvent::TextDelta("fake ok".into()))
                .await
                .map_err(|_| ProviderError::Protocol("closed".into()))?;
            Ok(ProviderResponse {
                finish_reason: FinishReason::Stop,
                usage: Default::default(),
                tool_calls: Vec::new(),
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = fixtures::test_config(&workspace);
    let mut provider = EchoProvider;
    let mut store = InMemoryStore::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let outcome = tpi::agent::run(
        &mut provider,
        &mut store,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "hello".into(),
            ui: tx,
            cancel: tokio_util::sync::CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: None,
        },
    )
    .await
    .expect("agent run on in-memory store");
    drain.abort();

    assert_eq!(outcome.assistant_text, "fake ok");
    assert_eq!(outcome.reason, tpi::session::CompletionReason::Stop);
    // in-memory store 记录了完整事件序列（UserSubmitted/RunStarted/Assistant/RunCompleted）。
    let types: Vec<&str> = store.events().iter().map(SessionEvent::type_name).collect();
    assert_eq!(
        types,
        vec![
            "user_submitted",
            "run_started",
            "assistant_message_committed",
            "run_completed",
        ],
        "in-memory store 事件序列"
    );
    assert_eq!(store.seq(), 4);
}

// ---- 契约测试 4：单写者（&mut 借用编译期保证 + 两 store 隔离）----

#[test]
fn two_stores_are_isolated() {
    let mut a = InMemoryStore::new();
    let mut b = InMemoryStore::new();
    a.append_event(&SessionEvent::UserSubmitted {
        content: "a".into(),
    })
    .unwrap();
    b.append_event(&SessionEvent::UserSubmitted {
        content: "b".into(),
    })
    .unwrap();
    assert_eq!(a.seq(), 1);
    assert_eq!(b.seq(), 1);
    assert_eq!(a.events().len(), 1);
    assert_eq!(b.events().len(), 1);
    assert_ne!(a.session_id(), b.session_id(), "隔离 store 各自身份");
}
