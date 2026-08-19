//! P3-05：Headless JSON surface 验收。
//!
//! - headless 直接订阅 semantic runtime（收集 LiveEvent），无 TUI drain task；
//! - versioned JSON output（v1）；
//! - 与 `TUI（agent_flow` 路径）对同一 fake provider 得到**等价业务终态**
//!   （`assistant_text` + reason）；
//! - `取消/完成有明确事件（run_completed.reason），退出码明确`。

mod fixtures;

use std::sync::{Arc, Mutex};

use tpi::app::headless::{exit_code_for, json_event, run_headless};
use tpi::ids::RunId;
use tpi::provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderResponse};
use tpi::session::store::{SessionLog, SessionStore};

/// 与 TUI `测试（agent_flow）同一` fake：单文本 delta + Stop。
struct EchoProvider;
impl Provider for EchoProvider {
    fn model_name(&self) -> &'static str {
        "echo"
    }
    async fn stream(
        &mut self,
        _request: tpi::provider::ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        events
            .send(ProviderEvent::TextDelta("headless ok".into()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        Ok(ProviderResponse {
            finish_reason: FinishReason::Stop,
            usage: Default::default(),
            tool_calls: Vec::new(),
        })
    }
}

fn setup() -> (tempfile::TempDir, tpi::config::Config) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = fixtures::test_config(&workspace);
    (dir, config)
}

/// headless 收集事件 + 终态：与 TUI `等价业务终态（assistant_text/reason`）。
#[tokio::test]
async fn headless_reaches_equivalent_terminal_state() {
    let (dir, config) = setup();
    let mut provider = EchoProvider;
    let mut log =
        SessionLog::create(&dir.path().join("sessions"), dir.path(), RunId::new_v7()).unwrap();
    let cancel_slot: Arc<Mutex<Option<tokio_util::sync::CancellationToken>>> =
        Arc::new(Mutex::new(None));

    let output = run_headless(
        &mut provider,
        &mut log,
        &config,
        &[],
        "hello".into(),
        &cancel_slot,
        std::sync::Arc::new(std::sync::Mutex::new(
            tpi::tool::registry::builtin_registry(),
        )),
    )
    .await
    .expect("headless run 成功");

    // 业务终态：assistant_text + Stop（与 TUI agent_flow 断言一致）。
    assert_eq!(output.outcome.assistant_text, "headless ok");
    assert_eq!(output.outcome.reason, tpi::session::CompletionReason::Stop);
    assert_eq!(exit_code_for(&output.outcome.reason), 0);

    // 事件序列包含 step_started + assistant_delta + run_completed 投影。
    let kinds: Vec<&str> = output.events.iter().map(|e| e.kind).collect();
    assert!(
        kinds.contains(&"step_started"),
        "headless 必须收到 step 边界事件: {kinds:?}"
    );
    assert!(
        kinds.contains(&"assistant_delta"),
        "headless 必须收到 assistant delta"
    );
    // 终态 JSON 由调用方追加（reason/assistant_text）。
    let final_event = tpi::app::headless::final_json(&output.outcome);
    assert_eq!(final_event.kind, "run_completed");
    assert_eq!(final_event.reason.as_deref(), Some("Stop"));
    assert_eq!(final_event.assistant_text.as_deref(), Some("headless ok"));
}

/// 无 drain task：ui channel 由 headless 消费（session 事件完整落盘）。
#[tokio::test]
async fn headless_persists_session_events() {
    let (dir, config) = setup();
    let mut provider = EchoProvider;
    let mut log =
        SessionLog::create(&dir.path().join("sessions"), dir.path(), RunId::new_v7()).unwrap();
    let cancel_slot: Arc<Mutex<Option<tokio_util::sync::CancellationToken>>> =
        Arc::new(Mutex::new(None));

    let _ = run_headless(
        &mut provider,
        &mut log,
        &config,
        &[],
        "hi".into(),
        &cancel_slot,
        std::sync::Arc::new(std::sync::Mutex::new(
            tpi::tool::registry::builtin_registry(),
        )),
    )
    .await
    .expect("headless run");

    // 事件已 durable 落盘（UserSubmitted/RunStarted/Assistant/RunCompleted）。
    let events = log.events_with_seq().expect("读事件");
    let types: Vec<&str> = events.iter().map(|(_, e)| e.type_name()).collect();
    assert!(types.contains(&"user_submitted"));
    assert!(types.contains(&"run_completed"));
    // 幂等：cancel_slot 已清空（run 结束后）。
    assert!(
        cancel_slot.lock().unwrap().is_none(),
        "run 结束后 current_cancel 必须清空"
    );
}

/// JSON 投影：versioned + 无正文泄露（tool output 是有界摘要，非原始正文）。
#[test]
fn json_events_are_versioned_and_bounded() {
    let event = tpi::agent::LiveEvent::StepStarted { step: 3 };
    let json = json_event(&event);
    assert_eq!(json.v, 1);
    assert_eq!(json.kind, "step_started");
    assert_eq!(json.step, Some(3));

    let tool = tpi::agent::LiveEvent::ToolCompleted {
        call_id: tpi::ids::ToolCallId::from_u128(1),
        name: "bash".into(),
        status: tpi::outcome::ToolStatus::Succeeded,
        duration_ms: 5,
        exit_code: Some(0),
        output: "bounded-summary".into(),
        diff: None,
    };
    let json = json_event(&tool);
    assert_eq!(json.kind, "tool_completed");
    assert_eq!(json.tool.as_deref(), Some("bash"));
    assert_eq!(json.delta.as_deref(), Some("bounded-summary"));
}

/// 退出码：取消 130、错误 1、正常 0。
#[test]
fn exit_codes_are_explicit() {
    assert_eq!(exit_code_for(&tpi::session::CompletionReason::Stop), 0);
    assert_eq!(
        exit_code_for(&tpi::session::CompletionReason::Cancelled),
        130
    );
    assert_eq!(
        exit_code_for(&tpi::session::CompletionReason::WallTimeExceeded),
        130
    );
    assert_eq!(exit_code_for(&tpi::session::CompletionReason::Error), 1);
}
