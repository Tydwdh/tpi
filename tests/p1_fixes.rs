//! P1 修复回归测试（fix.md 外部审查报告，第二批）。
//!
//! - P1-1：取消后 session 已提交的 assistant 内容必须同步进 outcome.messages；
//! - P1-2：max_tool_calls 超限必须是独立的 MaxToolCalls reason，不是 Error。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::fake_provider::{FakeProvider, FakeResponse};
use fixtures::test_config;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::ids::RunId;
use tpi::provider::{
    ChatMessage, ModelRequest, Provider, ProviderError, ProviderEvent, ProviderResponse,
};
use tpi::session::{CompletionReason, SessionEvent, SessionLog};

/// P1-1：发送部分文本后以 Cancelled 结束的 provider。
struct CancelAfterDeltaProvider;

impl Provider for CancelAfterDeltaProvider {
    fn model_name(&self) -> &str {
        "cancel-after-delta"
    }

    async fn stream(
        &mut self,
        _request: ModelRequest,
        events: mpsc::Sender<ProviderEvent>,
        _cancel: CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        events
            .send(ProviderEvent::TextDelta("partial ".into()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        events
            .send(ProviderEvent::TextDelta("answer".into()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        Err(ProviderError::Cancelled)
    }
}

/// P1-1：run 被取消且已有 assistant 内容提交到 session 时，
/// outcome.messages 必须包含该 assistant 消息（否则继续对话时
/// 模型上下文与 session 事实不一致——架构层"session 是事实源"的分叉）。
#[tokio::test]
async fn p1_1_cancel_keeps_history_consistent_with_session() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = CancelAfterDeltaProvider;
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        &[],
        "hello".into(),
        tx,
        CancellationToken::new(),
        false,
        false,
    )
    .await
    .expect("run 以 Cancelled 正常结束");

    drain.abort();
    assert_eq!(outcome.reason, CompletionReason::Cancelled);
    assert!(!outcome.assistant_text.is_empty(), "已到达的内容必须保留");

    // session 事实：已提交 assistant 内容。
    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::AssistantMessageCommitted { .. })),
        "session 必须包含已提交的 assistant 消息"
    );

    // P1-1：outcome.messages 必须与 session 一致（含该 assistant 消息）。
    assert!(
        outcome.messages.iter().any(
            |m| matches!(m, ChatMessage::Assistant { content, .. } if content == "partial answer")
        ),
        "outcome.messages 必须包含已提交的 assistant 内容（与 session 一致）: {:?}",
        outcome.messages
    );
}

/// P1-2：工具调用预算超限必须是独立的 MaxToolCalls reason。
/// 此前归为 CompletionReason::Error，用户/模型会误以为是协议错误。
#[tokio::test]
async fn p1_2_max_tool_calls_has_own_reason() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = test_config(&workspace);
    config.limits.max_tool_calls = 2;

    // 每次请求都要求调用工具（永不停止）→ 触发 MaxToolCalls。
    let mut provider = FakeProvider::scripted_loop(Box::new(|_request| {
        FakeResponse::with_tool_calls(vec![fixtures::fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "sample.txt"}),
        )])
    }));
    std::fs::write(workspace.join("sample.txt"), "x\n").unwrap();
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        &[],
        "go".into(),
        tx,
        CancellationToken::new(),
        false,
        false,
    )
    .await
    .expect("run 正常结束");

    drain.abort();
    assert_eq!(
        outcome.reason,
        CompletionReason::MaxToolCalls,
        "工具预算超限必须是 MaxToolCalls（此前误归为 Error）"
    );

    // session 事实同样记录独立 reason。
    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::RunCompleted {
                reason: CompletionReason::MaxToolCalls,
                ..
            }
        )),
        "session 必须记录 MaxToolCalls reason: {events:?}"
    );
}

/// P1-4：压缩与 prune 后上下文仍超窗口时，run 必须明确结束（ContextOverflow），
/// 而不是继续发起必然 length error 的请求。
#[tokio::test]
async fn p1_4_context_overflow_stops_run_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = test_config(&workspace);
    // 极小窗口：system prompt(919) + 工具 schema(5482) 已远超。
    config.model.context_window = Some(1200);
    config.model.max_output_tokens = Some(100);
    config.safety_reserve_tokens = 0;

    let mut provider = FakeProvider::scripted_loop(Box::new(|_request| FakeResponse::text("done")));
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        &[],
        "x".repeat(9000),
        tx,
        CancellationToken::new(),
        false,
        false,
    )
    .await
    .expect("run 正常结束");

    drain.abort();
    assert_eq!(
        outcome.reason,
        CompletionReason::ContextOverflow,
        "窗口无法容纳时必须明确结束，而不是发必然失败的请求"
    );
    // session 事实同样记录。
    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::RunCompleted {
                reason: CompletionReason::ContextOverflow,
                ..
            }
        )),
        "session 必须记录 ContextOverflow: {events:?}"
    );
}

/// P1-3：watchdog 在 deadline 前触发 on_warn（TUI BudgetWarning 的发送源头）。
#[tokio::test]
async fn p1_3_watchdog_fires_warn_before_deadline() {
    let cancel = CancellationToken::new();
    let (warn_tx, mut warn_rx) = tokio::sync::mpsc::unbounded_channel();
    let (handle, _) = tpi::agent::limits::spawn_watchdog_with_wall(
        std::time::Duration::from_secs(2),
        cancel.clone(),
        || {},
        move || {
            let _ = warn_tx.send(());
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while warn_rx.recv().await.is_none() {}
    })
    .await
    .expect("接近预算时必须触发 warn（P1-3）");
    cancel.cancel();
    let _ = handle.await;
}

/// P1-10：手动 /compact（force_compaction）在第一个完整边界无条件压缩，
/// 即使投影未超过窗口（此前只有自动触发，/compact 只是说明文字）。
#[tokio::test]
async fn p1_10_manual_compaction_runs_at_next_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace); // context_window None → 自动压缩永不触发

    let mut provider = FakeProvider::scripted_loop(Box::new(|request| {
        if request.tools.is_empty() {
            FakeResponse::text(
                "Goal: g\nConstraints: c\nDecisions: d\nCompleted: e\nIn progress: f\nNext exact action: g\nRelevant files and revisions: h\nVerification status: i\nFailed attempts and why: j",
            )
        } else {
            FakeResponse::text("done")
        }
    }));
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    // 历史足够大（压缩显著缩小），force=true 无条件压缩。
    let history = vec![
        ChatMessage::User("u".repeat(3000)),
        ChatMessage::Assistant {
            content: "a".repeat(3000),
            tool_calls: vec![],
        },
        ChatMessage::User("q".repeat(3000)),
    ];
    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        &history,
        "go".into(),
        tx,
        CancellationToken::new(),
        false,
        true, // P1-10：手动压缩
    )
    .await
    .expect("run 成功");

    drain.abort();
    assert_eq!(outcome.reason, CompletionReason::Stop);
    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::CompactionCommitted { .. })),
        "force 压缩必须提交 CompactionCommitted（context_window=None 时自动压缩不会触发）: {events:?}"
    );
}

/// §4.3：已发出部分文本后连接断开的 provider。
struct InterruptAfterDeltaProvider;

impl Provider for InterruptAfterDeltaProvider {
    fn model_name(&self) -> &str {
        "interrupt-after-delta"
    }

    async fn stream(
        &mut self,
        _request: ModelRequest,
        events: mpsc::Sender<ProviderEvent>,
        _cancel: CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        events
            .send(ProviderEvent::TextDelta("问题在 src/tool/files.rs 的 write...".into()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        events
            .send(ProviderEvent::TextDelta("因为 target_exists 分支".into()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        Err(ProviderError::Connection("connection reset by peer".into()))
    }
}

/// §4.3：已收到部分内容后断联——run 以 ProviderInterrupted 正常结束，
/// partial content 写入 AssistantAttemptInterrupted（record 事件），
/// 不丢、不进入对话投影（不是完整 turn）。
#[tokio::test]
async fn interrupted_attempt_records_partial_and_keeps_session_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = InterruptAfterDeltaProvider;
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        &[],
        "hello".into(),
        tx,
        CancellationToken::new(),
        false,
        false,
    )
    .await
    .expect("interrupted run 以正常结果结束（非 Err）");

    drain.abort();
    assert_eq!(outcome.reason, CompletionReason::ProviderInterrupted);
    // 用户看到的 partial 必须保留在 outcome（UI 展示用）。
    assert!(
        outcome.assistant_text.contains("target_exists"),
        "partial output 必须保留: {}",
        outcome.assistant_text
    );

    // session 事实：AssistantAttemptInterrupted（不是 AssistantMessageCommitted）。
    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::AssistantAttemptInterrupted {
                content,
                cause: tpi::session::InterruptCause::Connection,
                saw_tool_calls: false,
                ..
            } if content.contains("target_exists")
        )),
        "session 必须记录中断 attempt（connection cause, partial content）: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::AssistantMessageCommitted { .. })),
        "中断的 attempt 不得伪装成已提交 assistant 消息"
    );

    // 不进入对话投影：messages 只有 user 消息。
    assert!(
        !outcome
            .messages
            .iter()
            .any(|m| matches!(m, ChatMessage::Assistant { .. })),
        "partial 不是完整 turn，不得进入上下文"
    );
}

/// §4.3：未收到任何语义事件就连接失败——run 以 Err(RunFailure::Provider) 结束，
/// reason 记为 ProviderUnavailable，且没有任何 AssistantAttemptInterrupted。
struct UnavailableProvider;

impl Provider for UnavailableProvider {
    fn model_name(&self) -> &str {
        "unavailable-provider"
    }

    async fn stream(
        &mut self,
        _request: ModelRequest,
        _events: mpsc::Sender<ProviderEvent>,
        _cancel: CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::Connection("connect timeout".into()))
    }
}

#[tokio::test]
async fn unavailable_connect_fails_without_recorded_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = UnavailableProvider;
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let result = agent::run(
        &mut provider,
        &mut session,
        &config,
        &[],
        "hello".into(),
        tx,
        CancellationToken::new(),
        false,
        false,
    )
    .await;
    drain.abort();
    let error = match result {
        Ok(_) => panic!("连接失败必须返回 Err"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("provider failure"),
        "必须是 provider failure: {error}"
    );

    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::AssistantAttemptInterrupted { .. })),
        "未收到内容不得记录中断 attempt"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::RunCompleted {
                reason: CompletionReason::ProviderUnavailable,
                ..
            }
        )),
        "连接失败 reason 必须是 ProviderUnavailable: {events:?}"
    );
}
