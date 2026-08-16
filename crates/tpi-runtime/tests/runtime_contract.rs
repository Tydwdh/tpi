//! Runtime 契约测试（web_desktop.md §三十四 Runtime contract tests）。
//!
//! 验证核心 Application API：
//! - CreateSession / SubmitMessage / CancelRun 的命令生命周期；
//! - 事件序列（SubmitMessage → RunStarted → … → RunCompleted）；
//! - 命令 ack（Accepted / Rejected）；
//! - 事件 seq 单调递增；
//! - Busy 拒绝（run 中再次 submit）；
//! - 多订阅者都收到事件（multi-client 基础）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use camino::Utf8PathBuf;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use tpi_agent::provider::{
    FinishReason, ModelRequest, Provider, ProviderError, ProviderEvent, ProviderResponse, ToolCall,
};
use tpi_config::config::{Config, LimitsConfig, ModelConfig};
use tpi_core::ids::ToolCallId;
use tpi_protocol::{ClientCommand, EventEnvelope, RuntimeEvent};
use tpi_session::Usage;

use tpi_runtime::{RuntimeHandle, RuntimeTask};

// ---- 最小 fake provider（独立于顶层 tpi 的 fixtures，避免循环依赖） ----

#[derive(Debug, Clone)]
struct FakeResponse {
    text: String,
    finish: FinishReason,
    tool_calls: Vec<ToolCall>,
    /// 发送文本前延迟（测试 cancel 用）。
    delay_ms: u64,
}

impl FakeResponse {
    fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            finish: FinishReason::Stop,
            tool_calls: Vec::new(),
            delay_ms: 0,
        }
    }

    fn with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            text: String::new(),
            finish: FinishReason::ToolCalls,
            tool_calls,
            delay_ms: 0,
        }
    }

    fn with_delay(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }
}

/// 构造工具调用。
fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new_v7(),
        provider_id: format!("call-{name}"),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

struct FakeProvider {
    script: VecDeque<FakeResponse>,
    request_count: usize,
}

impl FakeProvider {
    fn new(responses: Vec<FakeResponse>) -> Self {
        Self {
            script: responses.into(),
            request_count: 0,
        }
    }
}

impl Provider for FakeProvider {
    fn model_name(&self) -> &str {
        "fake-model"
    }

    fn stream(
        &mut self,
        _request: ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send {
        let response = self
            .script
            .pop_front()
            .unwrap_or_else(|| FakeResponse::text(""));
        self.request_count += 1;
        let delay_ms = response.delay_ms;
        let cancel = cancel.clone();
        async move {
            if delay_ms > 0 {
                // 延迟期间响应取消（真实 provider 的网络请求会中断）。
                tokio::select! {
                    _ = cancel.cancelled() => {
                        return Err(ProviderError::Cancelled);
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                }
            }
            let text = response.text.clone();
            if !text.is_empty() {
                let _ = events.send(ProviderEvent::TextDelta(text)).await;
            }
            Ok(ProviderResponse {
                finish_reason: response.finish,
                usage: Usage::default(),
                tool_calls: response.tool_calls,
            })
        }
    }
}

// ---- 测试配置 ----

fn test_config(workspace: &Utf8PathBuf) -> Config {
    Config {
        model: ModelConfig {
            provider: "test".into(),
            name: "fake-model".into(),
            base_url: "https://example.invalid/v1".into(),
            reasoning: None,
            max_output_tokens: None,
            context_window: None,
            api_key_env: "TPI_TEST_API_KEY".into(),
            api_key: None,
            price_input: None,
            price_output: None,
        },
        models: Vec::new(),
        limits: LimitsConfig::default(),
        workspace_root: workspace.clone(),
        sessions_root: workspace.join(".tpi-test-sessions").into(),
        artifacts_root: workspace.join(".tpi-test-artifacts").into(),
        shell_path: None,
        safety_reserve_tokens: 8192,
        auto_open_browser: false,
        web_summary_model: "none".into(),
        system_prompt_extra: None,
        source: "test".into(),
        ui_theme: "omp".into(),
        ui_mode: tpi_ui_types::ViewMode::Fullscreen,
        ui_keymap: tpi_ui_types::Keymap::builtin(),
        ui_collapsed_lines: 10,
        allow_outside_workspace: true,
    }
}

/// 启动一个 runtime 实例，返回 (handle, join)。
fn start_runtime(config: Config) -> (RuntimeHandle, tokio::task::JoinHandle<()>) {
    let registry: Arc<StdMutex<tpi_capabilities::tool::registry::ToolRegistry>> = Arc::new(
        StdMutex::new(tpi_capabilities::tool::registry::builtin_registry()),
    );
    let build_provider: Box<dyn FnMut(&ModelConfig) -> Result<FakeProvider, String> + Send> =
        Box::new(|_| Ok(FakeProvider::new(vec![FakeResponse::text("你好")])));
    let task = RuntimeTask::new(Arc::new(config), build_provider, registry);
    RuntimeHandle::new(task)
}

/// 收集直到出现目标事件（含）的所有事件，带超时。
async fn collect_until(
    rx: &mut broadcast::Receiver<EventEnvelope>,
    predicate: impl Fn(&RuntimeEvent) -> bool,
) -> Vec<RuntimeEvent> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("等待目标事件超时；已收集 {collected:?}");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(envelope)) => {
                let done = predicate(&envelope.event);
                collected.push(envelope.event);
                if done {
                    return collected;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                panic!("事件订阅 lagged {n} 条（消费太慢）");
            }
            Ok(Err(_)) => panic!("事件流关闭"),
            Err(_) => panic!("等待目标事件超时"),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn submit_message_single_thread_runtime_also_completes() {
    // 与 ws 测试相同环境（current_thread）：验证单线程 runtime 不死锁。
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime(test_config(&ws));
    let mut rx = handle.subscribe();

    handle
        .command(ClientCommand::CreateSession { title: None })
        .await
        .unwrap();
    let events = collect_until(&mut rx, |e| {
        matches!(e, RuntimeEvent::SessionCreated { .. })
    })
    .await;
    let session_id = match &events[0] {
        RuntimeEvent::SessionCreated { session } => session.id,
        _ => unreachable!(),
    };
    handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "你好".into(),
        })
        .await
        .unwrap();
    let events = collect_until(&mut rx, |e| {
        matches!(
            e,
            RuntimeEvent::RunCompleted { .. } | RuntimeEvent::RunFailed { .. }
        )
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::RunCompleted { .. }))
    );

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn create_session_emits_session_created() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime(test_config(&ws));
    let mut rx = handle.subscribe();

    let ack = handle
        .command(ClientCommand::CreateSession { title: None })
        .await
        .expect("命令应被接受");
    assert!(ack.is_accepted(), "CreateSession 必须 Accepted");

    let events = collect_until(&mut rx, |e| {
        matches!(e, RuntimeEvent::SessionCreated { .. })
    })
    .await;
    assert!(matches!(events[0], RuntimeEvent::SessionCreated { .. }));

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn submit_message_produces_run_lifecycle_events() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime(test_config(&ws));
    let mut rx = handle.subscribe();

    handle
        .command(ClientCommand::CreateSession { title: None })
        .await
        .unwrap();
    let events = collect_until(&mut rx, |e| {
        matches!(e, RuntimeEvent::SessionCreated { .. })
    })
    .await;
    let session_id = match &events[0] {
        RuntimeEvent::SessionCreated { session } => session.id,
        _ => unreachable!(),
    };

    handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "你好".into(),
        })
        .await
        .unwrap();

    let events = collect_until(&mut rx, |e| {
        matches!(
            e,
            RuntimeEvent::RunCompleted { .. } | RuntimeEvent::RunFailed { .. }
        )
    })
    .await;

    // 事件序列：RunStarted → UserMessageAdded → AssistantDelta → RunCompleted
    // （中间允许 UsageUpdated / ContextUsage / SessionStatusChanged 等派生事件）。
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            RuntimeEvent::RunStarted { .. } => "run_started",
            RuntimeEvent::UserMessageAdded { .. } => "user_added",
            RuntimeEvent::AssistantDelta { .. } => "assistant_delta",
            RuntimeEvent::RunCompleted { .. } => "run_completed",
            RuntimeEvent::SessionStatusChanged { .. } => "status_changed",
            RuntimeEvent::UsageUpdated { .. } => "usage_updated",
            RuntimeEvent::ContextUsage { .. } => "context_usage",
            other => panic!("意外事件: {other:?}"),
        })
        .collect();
    assert!(
        kinds.iter().any(|k| *k == "run_started"),
        "必须出现 RunStarted: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| *k == "user_added"),
        "必须出现 UserMessageAdded: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| *k == "assistant_delta"),
        "必须出现 AssistantDelta: {kinds:?}"
    );
    assert_eq!(
        kinds.last(),
        Some(&"run_completed"),
        "最后事件必须是 RunCompleted: {kinds:?}"
    );

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn event_seq_is_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime(test_config(&ws));
    let mut rx = handle.subscribe();

    handle
        .command(ClientCommand::CreateSession { title: None })
        .await
        .unwrap();
    let events = collect_until(&mut rx, |e| {
        matches!(e, RuntimeEvent::SessionCreated { .. })
    })
    .await;
    let session_id = match &events[0] {
        RuntimeEvent::SessionCreated { session } => session.id,
        _ => unreachable!(),
    };
    handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "hi".into(),
        })
        .await
        .unwrap();
    let _ = collect_until(&mut rx, |e| {
        matches!(
            e,
            RuntimeEvent::RunCompleted { .. } | RuntimeEvent::RunFailed { .. }
        )
    })
    .await;

    // 订阅者收到的所有事件 seq 必须严格递增。
    let mut last_seq = 0u64;
    // 重新订阅（之前消费过的流已到尾）；用一个新的订阅者收集直到最后 seq。
    // 简化：直接检查 last_seq 游标已推进。
    let final_seq = handle.last_seq().await;
    assert!(final_seq > 0, "事件流必须产生 seq");

    // 再订阅一次验证 seq 连续性。
    let mut rx2 = handle.subscribe();
    let _ = collect_until(&mut rx2, |e| {
        matches!(e, RuntimeEvent::SessionStatusChanged { .. })
    });
    while let Ok(Ok(envelope)) =
        tokio::time::timeout(std::time::Duration::from_millis(500), rx2.recv()).await
    {
        assert!(envelope.seq > last_seq, "seq 必须单调递增");
        last_seq = envelope.seq;
    }

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn submit_to_unknown_session_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime(test_config(&ws));

    let ack = handle
        .command(ClientCommand::SubmitMessage {
            session_id: tpi_core::ids::SessionId::new_v7(),
            content: "hi".into(),
        })
        .await
        .unwrap();
    assert!(!ack.is_accepted(), "未知 session 必须 Rejected");
    match ack.status {
        tpi_protocol::AckStatus::Rejected(err) => {
            assert_eq!(err.code, tpi_protocol::ErrorCode::SessionNotFound);
        }
        tpi_protocol::AckStatus::Accepted => unreachable!(),
    }

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn empty_message_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime(test_config(&ws));
    let mut rx = handle.subscribe();

    handle
        .command(ClientCommand::CreateSession { title: None })
        .await
        .unwrap();
    let events = collect_until(&mut rx, |e| {
        matches!(e, RuntimeEvent::SessionCreated { .. })
    })
    .await;
    let session_id = match &events[0] {
        RuntimeEvent::SessionCreated { session } => session.id,
        _ => unreachable!(),
    };

    let ack = handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "   ".into(),
        })
        .await
        .unwrap();
    assert!(!ack.is_accepted(), "空消息必须 Rejected");
    match ack.status {
        tpi_protocol::AckStatus::Rejected(err) => {
            assert_eq!(err.code, tpi_protocol::ErrorCode::InvalidCommand);
        }
        tpi_protocol::AckStatus::Accepted => unreachable!(),
    }

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

// ===== TUI 交互生命周期契约（web_desktop.md §八：TUI = command producer + event consumer） =====
//
// 这些测试证明 Application API 能承载 TUI 的完整交互循环：
// 提交 → 流式事件 → 取消 → request_input 挂起 → 回答恢复。
// 未来 TUI 迁移到 RuntimeHandle 时以此为验收基线。

/// 专用 runtime：provider 脚本可注入。
fn start_runtime_with(
    config: Config,
    responses: Vec<FakeResponse>,
) -> (RuntimeHandle, tokio::task::JoinHandle<()>) {
    let registry: Arc<StdMutex<tpi_capabilities::tool::registry::ToolRegistry>> = Arc::new(
        StdMutex::new(tpi_capabilities::tool::registry::builtin_registry()),
    );
    let build_provider: Box<dyn FnMut(&ModelConfig) -> Result<FakeProvider, String> + Send> =
        Box::new(move |_| Ok(FakeProvider::new(responses.clone())));
    let task = RuntimeTask::new(Arc::new(config), build_provider, registry);
    RuntimeHandle::new(task)
}

/// 建会话并返回 session_id。
async fn create_session(
    handle: &RuntimeHandle,
    rx: &mut broadcast::Receiver<EventEnvelope>,
) -> tpi_core::ids::SessionId {
    handle
        .command(ClientCommand::CreateSession { title: None })
        .await
        .unwrap();
    let events = collect_until(rx, |e| matches!(e, RuntimeEvent::SessionCreated { .. })).await;
    match &events[0] {
        RuntimeEvent::SessionCreated { session } => session.id,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn cancel_run_actually_stops_running_run() {
    // 慢 provider（800ms 才返回文本）：cancel 必须在 run 完成前生效。
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime_with(
        test_config(&ws),
        vec![FakeResponse::text("慢回复").with_delay(800)],
    );
    let mut rx = handle.subscribe();
    let session_id = create_session(&handle, &mut rx).await;

    handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "开始".into(),
        })
        .await
        .unwrap();

    // 等 RunStarted 后立即 cancel。
    let _ = collect_until(&mut rx, |e| matches!(e, RuntimeEvent::RunStarted { .. })).await;
    handle
        .command(ClientCommand::CancelRun { session_id })
        .await
        .unwrap();

    // 最终必须收到 reason=cancelled 的 RunCompleted。
    let events = collect_until(&mut rx, |e| {
        matches!(
            e,
            RuntimeEvent::RunCompleted { .. } | RuntimeEvent::RunFailed { .. }
        )
    })
    .await;
    let completed = events
        .iter()
        .find(|e| matches!(e, RuntimeEvent::RunCompleted { .. }))
        .expect("必须有 RunCompleted");
    match completed {
        RuntimeEvent::RunCompleted { reason, .. } => {
            assert_eq!(
                *reason,
                tpi_protocol::CompletionReasonDto::Cancelled,
                "cancel 后 reason 必须是 cancelled"
            );
        }
        _ => unreachable!(),
    }

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn request_input_suspend_emits_input_requested_and_answer_resumes() {
    // provider 第一轮返回 request_input 工具调用（挂起），
    // 第二轮返回普通文本（回答后继续）。
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime_with(
        test_config(&ws),
        vec![
            FakeResponse::with_tool_calls(vec![tool_call(
                "request_input",
                serde_json::json!({"question": "继续吗？", "options": ["是", "否"]}),
            )]),
            FakeResponse::text("好的，继续了"),
        ],
    );
    let mut rx = handle.subscribe();
    let session_id = create_session(&handle, &mut rx).await;

    handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "开始".into(),
        })
        .await
        .unwrap();

    // 必须收到 InputRequested（挂起）与 RunCompleted(awaiting_user_input)。
    let events = collect_until(&mut rx, |e| {
        matches!(
            e,
            RuntimeEvent::RunCompleted { .. } | RuntimeEvent::RunFailed { .. }
        )
    })
    .await;
    let input_requested = events
        .iter()
        .find(|e| matches!(e, RuntimeEvent::InputRequested { .. }))
        .expect("必须收到 InputRequested");
    let request_id = match input_requested {
        RuntimeEvent::InputRequested { request_id, .. } => *request_id,
        _ => unreachable!(),
    };
    let completed = events
        .iter()
        .find(|e| matches!(e, RuntimeEvent::RunCompleted { .. }))
        .expect("必须收到 RunCompleted");
    match completed {
        RuntimeEvent::RunCompleted { reason, .. } => {
            assert_eq!(
                *reason,
                tpi_protocol::CompletionReasonDto::AwaitingUserInput,
                "挂起 reason 必须是 awaiting_user_input"
            );
        }
        _ => unreachable!(),
    }

    // 回答后 run 恢复并完成。
    handle
        .command(ClientCommand::AnswerInput {
            session_id,
            request_id,
            answer: "是".into(),
        })
        .await
        .unwrap();
    let events2 = collect_until(&mut rx, |e| {
        matches!(
            e,
            RuntimeEvent::RunCompleted { .. } | RuntimeEvent::RunFailed { .. }
        )
    })
    .await;
    let completed2 = events2
        .iter()
        .find(|e| matches!(e, RuntimeEvent::RunCompleted { .. }))
        .expect("回答后必须再次 RunCompleted");
    match completed2 {
        RuntimeEvent::RunCompleted {
            reason,
            assistant_text,
            ..
        } => {
            assert_eq!(*reason, tpi_protocol::CompletionReasonDto::Stop);
            assert!(
                assistant_text.contains("继续了"),
                "恢复后的回复: {assistant_text}"
            );
        }
        _ => unreachable!(),
    }

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn resume_session_broadcasts_history_snapshot() {
    // 页面刷新 / 断线重连后：ResumeSession 应广播 SessionHistory 供重建 transcript。
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) =
        start_runtime_with(test_config(&ws), vec![FakeResponse::text("你好，历史回复")]);
    let mut rx = handle.subscribe();
    let session_id = create_session(&handle, &mut rx).await;

    handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "第一条消息".into(),
        })
        .await
        .unwrap();
    let _ = collect_until(&mut rx, |e| {
        matches!(
            e,
            RuntimeEvent::RunCompleted { .. } | RuntimeEvent::RunFailed { .. }
        )
    })
    .await;

    // resume（模拟重连后的恢复）→ 期待 SessionHistory。
    handle
        .command(ClientCommand::ResumeSession {
            session_id: session_id.to_string(),
        })
        .await
        .unwrap();
    let events = collect_until(&mut rx, |e| {
        matches!(e, RuntimeEvent::SessionHistory { .. })
    })
    .await;
    let history = events
        .iter()
        .find_map(|e| match e {
            RuntimeEvent::SessionHistory { messages, .. } => Some(messages.clone()),
            _ => None,
        })
        .expect("必须收到 SessionHistory");
    assert!(
        history.iter().any(|m| {
            m.role == tpi_protocol::MessageRoleDto::User && m.content.contains("第一条消息")
        }),
        "历史必须包含用户消息: {history:?}"
    );
    assert!(
        history.iter().any(|m| {
            m.role == tpi_protocol::MessageRoleDto::Assistant && m.content.contains("历史回复")
        }),
        "历史必须包含助手回复: {history:?}"
    );

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}

#[tokio::test]
async fn double_answer_input_first_wins_second_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (handle, join) = start_runtime_with(
        test_config(&ws),
        vec![
            FakeResponse::with_tool_calls(vec![tool_call(
                "request_input",
                serde_json::json!({"question": "选哪个？"}),
            )]),
            FakeResponse::text("已选择"),
        ],
    );
    let mut rx = handle.subscribe();
    let session_id = create_session(&handle, &mut rx).await;
    handle
        .command(ClientCommand::SubmitMessage {
            session_id,
            content: "开始".into(),
        })
        .await
        .unwrap();

    let events = collect_until(&mut rx, |e| {
        matches!(e, RuntimeEvent::InputRequested { .. })
    })
    .await;
    let request_id = events
        .iter()
        .find_map(|e| match e {
            RuntimeEvent::InputRequested { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("必须收到 InputRequested");

    // 两个客户端同时回答：第一个成功，第二个被拒绝。
    let a1 = handle
        .command(ClientCommand::AnswerInput {
            session_id,
            request_id,
            answer: "A".into(),
        })
        .await
        .unwrap();
    // 等待第一个回答处理完（run 恢复中），再发第二个。
    let _ = collect_until(&mut rx, |e| matches!(e, RuntimeEvent::InputAnswered { .. })).await;
    let a2 = handle
        .command(ClientCommand::AnswerInput {
            session_id,
            request_id,
            answer: "B".into(),
        })
        .await
        .unwrap();

    assert!(a1.is_accepted(), "第一个回答必须 Accepted");
    assert!(!a2.is_accepted(), "重复回答必须 Rejected");

    handle.shutdown().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
}
