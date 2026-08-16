//! 事件信封（web_desktop.md §六：Event 必须带稳定 Identity）。
//!
//! 每个事件携带：
//!
//! ```text
//! protocol_version
//! seq                —— runtime 全局单调递增（前端据此断线续传）
//! timestamp          —— unix epoch 毫秒
//! session_id         —— 事件适用的会话（部分 runtime 级事件为 None）
//! run_id             —— 事件适用的 run（部分事件为 None）
//! event              —— 事件负载
//! ```
//!
//! 前端不依赖"上一条事件应该是谁"的隐式顺序推断：每个信封自带完整上下文。

use serde::{Deserialize, Serialize};

use tpi_core::ids::{RunId, SessionId};

use crate::event::RuntimeEvent;
use crate::version::PROTOCOL_VERSION;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub protocol_version: u32,
    /// runtime 全局单调递增序列（断线重连 after_seq 的游标）。
    pub seq: u64,
    /// unix epoch 毫秒。
    pub timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// 事件负载（嵌套字段，避免 serde flatten 与 skip 交互的坑；
    /// wire 形如 `{"type":"run_started",...}`）。
    pub event: RuntimeEvent,
}

impl EventEnvelope {
    pub fn new(seq: u64, event: RuntimeEvent) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (session_id, run_id) = match &event {
            RuntimeEvent::SessionCreated { session } => (Some(session.id), None),
            RuntimeEvent::SessionList { .. } => (None, None),
            RuntimeEvent::SessionResumed { session } => (Some(session.id), None),
            RuntimeEvent::SessionHistory { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::SessionStatusChanged { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::RunStarted { session_id, run_id } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::RunCompleted {
                session_id, run_id, ..
            } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::RunFailed {
                session_id, run_id, ..
            } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::UserMessageAdded { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::AssistantDelta {
                session_id, run_id, ..
            } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::ToolStarted {
                session_id, run_id, ..
            } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::ToolCompleted {
                session_id, run_id, ..
            } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::ToolOutputDelta {
                session_id, run_id, ..
            } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::InputRequested {
                session_id, run_id, ..
            } => (Some(*session_id), Some(*run_id)),
            RuntimeEvent::InputAnswered { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::InputRejected { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::PlanUpdated { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::ContextUsage { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::UsageUpdated { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::BudgetWarning { session_id } => (Some(*session_id), None),
            RuntimeEvent::StreamRecovering { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::TurnRestarting { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::CompactionNotice { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::SubagentReported { child_session, .. } => (Some(*child_session), None),
            RuntimeEvent::MutationRecorded { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::UndoCompleted { session_id, .. } => (Some(*session_id), None),
            RuntimeEvent::RedoCompleted { session_id, .. } => (Some(*session_id), None),
        };
        Self {
            protocol_version: PROTOCOL_VERSION,
            seq,
            timestamp_ms,
            session_id,
            run_id,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RuntimeEvent;

    #[test]
    fn envelope_round_trips_and_carries_identity() {
        let session_id = SessionId::new_v7();
        let run_id = RunId::new_v7();
        let ev = EventEnvelope::new(42, RuntimeEvent::RunStarted { session_id, run_id });
        assert_eq!(ev.session_id, Some(session_id));
        assert_eq!(ev.run_id, Some(run_id));
        assert_eq!(ev.seq, 42);
        assert_eq!(ev.protocol_version, PROTOCOL_VERSION);

        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"]["type"], "run_started");
        assert_eq!(json["seq"], 42);
        assert_eq!(json["session_id"], session_id.to_string());
        let back: EventEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn session_created_envelope_propagates_id() {
        let ev = EventEnvelope::new(
            1,
            RuntimeEvent::SessionCreated {
                session: crate::view::SessionView {
                    id: SessionId::new_v7(),
                    title: String::new(),
                    workspace: "w".into(),
                    status: crate::view::SessionStatus::Idle,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                },
            },
        );
        assert!(ev.session_id.is_some());
    }
}
