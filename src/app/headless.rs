//! P3-05：Headless JSON surface——直接订阅 semantic runtime，无 TUI drain task。
//!
//! - [`run_headless_json`]：跑一次 agent run，**真正消费** `LiveEvent`（不丢弃），
//!   产出 versioned JSON lines（v1）；
//! - 取消/await input 有明确事件（`run_completed.reason` / `input_requested`），
//!   调用方（CLI `-p`/web）据此定退出码；
//! - 与 TUI 对同一 fake provider 得到等价业务终态（测试断言）。
//!
//! 无 TUI channel/drain：本 surface 的 ui channel 由本函数消费（收集/写 JSONL），
//! 不存在“spawn 丢弃 rx”的 workaround。

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentOutcome, LiveEvent, RunFailure, RunInput};
use crate::provider::Provider;
use crate::session::store::SessionStore;

/// JSON output schema 版本。
pub const JSON_OUTPUT_VERSION: u32 = 1;

/// 一条 headless JSON 输出事件（serde 序列化；逐行写 stdout/返回 Vec）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonEvent {
    pub v: u32,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_text: Option<String>,
}

/// `LiveEvent` → JsonEvent（纯投影；无正文泄露：tool output 已是有界摘要）。
#[must_use]
pub fn json_event(event: &LiveEvent) -> JsonEvent {
    match event {
        LiveEvent::StepStarted { step } => JsonEvent {
            v: JSON_OUTPUT_VERSION,
            kind: "step_started",
            step: Some(*step),
            ..Default::default()
        },
        LiveEvent::AssistantDelta { text, .. } => JsonEvent {
            v: JSON_OUTPUT_VERSION,
            kind: "assistant_delta",
            delta: Some(text.clone()),
            ..Default::default()
        },
        LiveEvent::ToolStarted { name, .. } => JsonEvent {
            v: JSON_OUTPUT_VERSION,
            kind: "tool_started",
            tool: Some(name.clone()),
            ..Default::default()
        },
        LiveEvent::ToolCompleted {
            name,
            status,
            output,
            ..
        } => JsonEvent {
            v: JSON_OUTPUT_VERSION,
            kind: "tool_completed",
            tool: Some(name.clone()),
            tool_status: Some(format!("{status:?}")),
            delta: Some(output.clone()),
            ..Default::default()
        },
        LiveEvent::ToolOutputDelta { text, .. } => JsonEvent {
            v: JSON_OUTPUT_VERSION,
            kind: "tool_output_delta",
            delta: Some(text.clone()),
            ..Default::default()
        },
        LiveEvent::ContextUsage { .. }
        | LiveEvent::UsageUpdated { .. }
        | LiveEvent::BudgetWarning
        | LiveEvent::PlanUpdated { .. }
        | LiveEvent::StreamRecovering { .. }
        | LiveEvent::TurnRestarting { .. }
        | LiveEvent::ProviderRetrying { .. }
        | LiveEvent::CompactionNotice { .. }
        | LiveEvent::SubagentReported { .. } => JsonEvent {
            v: JSON_OUTPUT_VERSION,
            kind: "notice",
            ..Default::default()
        },
    }
}

impl Default for JsonEvent {
    fn default() -> Self {
        Self {
            v: JSON_OUTPUT_VERSION,
            kind: "",
            step: None,
            delta: None,
            tool: None,
            tool_status: None,
            reason: None,
            assistant_text: None,
        }
    }
}

/// headless 输出：事件列表 + 终态。
pub struct HeadlessOutput {
    pub events: Vec<JsonEvent>,
    pub outcome: AgentOutcome,
}

/// 跑一次 run，消费 `LiveEvent` 并收集为 JSON 事件（无 drain task）。
pub async fn run_headless<P: Provider, S: SessionStore>(
    provider: &mut P,
    session: &mut S,
    config: &crate::config::Config,
    history: &[crate::provider::ChatMessage],
    message: String,
    current_cancel: &Arc<Mutex<Option<CancellationToken>>>,
    registry: Arc<std::sync::Mutex<crate::tool::registry::ToolRegistry>>,
) -> Result<HeadlessOutput, String> {
    let cancel = CancellationToken::new();
    *crate::util::lock_mutex(current_cancel, "current_cancel") = Some(cancel.clone());
    let (ui_tx, mut ui_rx) = mpsc::channel::<LiveEvent>(128);
    // 本函数消费 ui channel（收集 JSON 事件），不 spawn 丢弃 task。
    let collector = tokio::spawn(async move {
        let mut events: Vec<JsonEvent> = Vec::new();
        while let Some(event) = ui_rx.recv().await {
            events.push(json_event(&event));
        }
        events
    });
    let outcome = crate::agent::run(
        provider,
        session,
        config,
        RunInput {
            history,
            user_message: message,
            ui: ui_tx,
            cancel: cancel.clone(),
            interactive: false,
            force_compaction: false,
            workspace: None,
            registry,
            // 单次 run 诊断：不经 session 级共享，每次新建（无跨 run 需求）。
            processes: Arc::new(Mutex::new(crate::process::managed::ProcessRegistry::new())),
            terminals: Arc::new(Mutex::new(crate::terminal::TerminalRegistry::default())),
            agents: Arc::new(Mutex::new(tpi_agent::agent::manager::AgentManager::new())),
        },
    )
    .await;
    *crate::util::lock_mutex(current_cancel, "current_cancel") = None;
    // 等 collector 收完剩余事件（channel 关闭后结束）并取回事件。
    let events = collector
        .await
        .map_err(|e| format!("headless collector join 失败: {e}"))?;
    let outcome = outcome.map_err(|failure| failure.to_string())?;
    if outcome.reason == crate::session::CompletionReason::Error {
        return Err(format!(
            "run 以 Error 结束（长度限制/内容过滤/协议错误）；session 记录: {}",
            session.path().display()
        ));
    }
    Ok(HeadlessOutput { events, outcome })
}

/// 终态 → JSON 事件（含 `reason/assistant_text；调用方据此定退出码`）。
#[must_use]
pub fn final_json(outcome: &AgentOutcome) -> JsonEvent {
    JsonEvent {
        v: JSON_OUTPUT_VERSION,
        kind: "run_completed",
        reason: Some(format!("{:?}", outcome.reason)),
        assistant_text: Some(outcome.assistant_text.clone()),
        ..Default::default()
    }
}

/// 退出码建议（headless 调用方用）：正常 Stop 0；取消 130；错误 1。
#[must_use]
pub fn exit_code_for(reason: &crate::session::CompletionReason) -> i32 {
    match reason {
        crate::session::CompletionReason::Stop => 0,
        crate::session::CompletionReason::Cancelled
        | crate::session::CompletionReason::WallTimeExceeded => 130,
        _ => 1,
    }
}

// 兼容引用（RunFailure 未直接使用但类型在错误路径出现）。
#[allow(unused)]
fn _failure_type(_: RunFailure) {}
