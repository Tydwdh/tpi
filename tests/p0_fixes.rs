//! P0 修复回归测试（fix.md 外部审查报告，逐条红→绿）。
//!
//! - P0-1：`-p` 模式 UI channel 无人消费时大量 delta 不得挂死；
//! - P0-2：compaction 请求大量 delta 不得挂死（事件消费与 stream 并发）；
//! - P0-8：compaction covered 范围必须包含压缩前最后一条事件（off-by-one）。

mod fixtures;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use fixtures::fake_provider::{FakeProvider, FakeResponse};
use fixtures::test_config;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::app;
use tpi::ids::{EventId, RunId};
use tpi::provider::{
    ChatMessage, FinishReason, ModelRequest, Provider, ProviderError, ProviderEvent,
    ProviderResponse,
};
use tpi::session::{
    AssistantMessage, CompactSummary, CompletionReason, EventRange, SessionEvent, SessionLog, Usage,
};

/// 洪泛 provider：一次性发送 N 个 TextDelta（超过任何 channel 容量）。
struct FloodProvider {
    deltas: usize,
}

impl Provider for FloodProvider {
    fn model_name(&self) -> &str {
        "flood"
    }

    async fn stream(
        &mut self,
        _request: ModelRequest,
        events: mpsc::Sender<ProviderEvent>,
        _cancel: CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        for i in 0..self.deltas {
            events
                .send(ProviderEvent::TextDelta(format!("t{i} ")))
                .await
                .map_err(|_| ProviderError::Protocol("channel closed".into()))?;
        }
        events
            .send(ProviderEvent::TextDelta("end".into()))
            .await
            .map_err(|_| ProviderError::Protocol("channel closed".into()))?;
        Ok(ProviderResponse {
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            tool_calls: Vec::new(),
        })
    }
}

/// P0-1：`-p` 模式 `(ui_tx, _ui_rx)` 丢弃 rx 后，agent 的 `ui.send().await`
/// 在 channel 满（128）时永久等待 → 整个进程挂死。
/// 修复：run_prompt_once 启动 drain task 消费 UI 事件。
#[tokio::test]
async fn p0_1_prompt_mode_survives_delta_flood() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FloodProvider { deltas: 400 };
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let cancel = CancellationToken::new();
    let current_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(Some(cancel)));

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        app::run_prompt_once(
            &mut provider,
            &mut session,
            &config,
            &[],
            "hello".to_string(),
            current_cancel,
        ),
    )
    .await
    .expect("run_prompt_once 必须在 10s 内完成（P0-1 死锁）")
    .expect("run 成功");

    assert!(
        outcome.assistant_text.contains("end"),
        "最终答案必须包含全部 delta（助手文本前缀: {}）",
        &outcome.assistant_text[..outcome.assistant_text.len().min(200)]
    );
}

/// P0-2：compaction 请求（无工具 schema）返回大量 delta 时，`compact_turn`
/// 先 `stream().await` 再收 event_rx 会死锁（provider 同一 task 内 send().await，
/// 事件数超过 EVENT_CHANNEL_CAPACITY=256）。
/// 修复：stream 与事件消费并发（select!），stream 返回后 drain 残余事件。
#[tokio::test]
async fn p0_2_compaction_survives_delta_flood() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = test_config(&workspace);
    config.model.context_window = Some(3000);
    config.model.max_output_tokens = Some(100);
    config.safety_reserve_tokens = 0; // usable = 2900

    // 第一条消息 9000 字符（≈3000 tokens）→ 第一轮就触发 compaction。
    let mut provider = FakeProvider::scripted_loop(Box::new(|request| {
        if request.tools.is_empty() {
            // compaction 请求：400 个短 delta（总 token 小，满足显著缩小校验）。
            let deltas: Vec<String> = (0..400).map(|i| format!("goal{i} ")).collect();
            FakeResponse::text_deltas(deltas)
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

    // 模拟 TUI：正常消费 UI 事件（-p 的 drain 场景由 P0-1 覆盖）。
    let (ui_tx, mut ui_rx) = mpsc::channel(128);
    let drain = tokio::spawn(async move { while ui_rx.recv().await.is_some() {} });

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        agent::run(
            &mut provider,
            &mut session,
            &config,
            &[],
            "x".repeat(9000),
            ui_tx,
            CancellationToken::new(),
            false,
            false,
        ),
    )
    .await
    .expect("agent::run 必须在 10s 内完成（P0-2 死锁）");

    drain.abort();
    // P1-4 后：窗口过小（user 消息 + system + 工具 schema 远超 usable）时，
    // compaction 失败后明确 ContextOverflow 结束——测试核心是“大量 delta
    // 不死锁”，compaction 请求已真实发出并消费。
    let outcome = outcome.expect("agent::run 成功");
    assert!(
        matches!(
            outcome.reason,
            CompletionReason::ContextOverflow | CompletionReason::Stop
        ),
        "unexpected reason: {:?}",
        outcome.reason
    );
}

/// P0-8：compaction 的 covered.end 是 exclusive，必须等于"compaction 时下一条
/// 事件的 seq"（= 最后一条事件 seq + 1）。此前写 `session.seq()` 少覆盖最后一条，
/// 短会话恢复时该 raw 事件会与 summary 重复注入。
#[test]
fn p0_8_compaction_covered_includes_last_pre_compact_event() {
    // 真实时序：seq 1-3 是压缩前事件，compaction 在 seq=3 时执行，
    // 覆盖 1..=3（end=4 exclusive）。seq 5 是压缩后的新事件。
    let events = vec![
        SessionEvent::UserSubmitted {
            content: "hello".into(),
        },
        SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: "hi".into(),
                tool_calls: Vec::new(),
            },
        },
        SessionEvent::UserSubmitted {
            content: "again".into(),
        },
        SessionEvent::CompactionCommitted {
            covered: EventRange {
                start: EventId::from_u128(1),
                end: EventId::from_u128(4),
            },
            summary: CompactSummary {
                text: "（此前会话的压缩摘要）".into(),
            },
        },
        SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: "reply".into(),
                tool_calls: Vec::new(),
            },
        },
    ];
    let messages = app::session_to_messages(&events);

    // 摘要以 user 前缀注入。
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, ChatMessage::User(t) if t.contains("压缩摘要"))),
        "摘要必须注入: {messages:?}"
    );
    // "again"（seq 3）已被压缩进摘要，不得以 raw 形式重复注入。
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, ChatMessage::User(t) if t == "again")),
        "被压缩覆盖的 raw 事件不得重复注入: {messages:?}"
    );
    // 压缩后的新事件保留。
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, ChatMessage::Assistant { content, .. } if content == "reply")),
        "压缩后的新事件必须保留: {messages:?}"
    );
}
