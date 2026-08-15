//! Session 持久层（P2-01 拆分后：protocol/codec/store 各归其位）。
//!
//! - [`protocol`]：durable domain event + envelope + wire 类型（领域 API + 稳定 wire）；
//! - [`store`]：SessionLog append/read/sync/lock + 投影；
//! - 派生视图（conversation/transcript/plan/catalog）都是 projection。
//!
//! 文件布局：`~/.tpi/sessions/<workspace-id>/<session-id>.jsonl`。

pub mod artifact;
pub mod conversation;
pub mod inbox;
pub mod projector;
pub mod protocol;
pub mod recovery;
pub mod repair;
pub mod store;
pub mod telemetry;

// P2-01：store 层 re-export（原 mod.rs 的 public API 保持不变；迁移完成后
// 调用方可改 `session::store::` 直引，re-export 留待 P10 清理）。
pub use inbox::{Inbox, InboxEntry, MAX_INBOX_CAPACITY};
pub use projector::{ConversationProjector, plan_from_events};
pub use protocol::{
    AssistantMessage, CompactSummary, CompletionReason, Envelope, EventBody, EventRange,
    InterruptCause, MAX_SESSION_EVENT_BYTES, ModelRef, RecoveryMetadata, RunLimits, SCHEMA_VERSION,
    SessionEvent, Usage,
};
pub use store::read_envelopes;
pub(crate) use store::{SessionProtocolState, open_and_lock_session};
pub use telemetry::{
    PROJECTOR_VERSION, SessionTelemetryProjector, TelemetryCounts, TelemetryGap, TelemetryRecord,
};

pub use store::{
    SessionLog, compacted_range, latest_plan, latest_plan_from_events, project_domain_messages,
    project_messages, project_messages_with_ranges, read_events, read_events_and_max_seq,
    read_events_with_seq, replay_domain_messages, replay_messages, workspace_id_for,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{EventId, RequestId, RunId, SessionId, ToolCallId};
    use crate::message::{ChatMessage, ToolCall};
    use crate::session::store::read_envelopes_state_with_limits;
    use camino::Utf8PathBuf;
    use std::path::PathBuf;

    /// 中间/已换行的损坏记录不能静默跳过；只有未换行的尾部残片可丢弃。
    #[test]
    fn middle_corruption_is_rejected_but_incomplete_tail_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let workspace_id = workspace_id_for(workspace.as_std_path());
        let session_id = SessionId::new_v7();
        let path = sessions_root
            .join(&workspace_id)
            .join(format!("{session_id}.jsonl"));

        // 先写两条完整事件。
        let mut log = SessionLog::create_with_id(
            &sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
            session_id,
        )
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "one".into(),
        })
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "two".into(),
        })
        .unwrap();
        drop(log);
        let valid = std::fs::read(&path).unwrap();
        let mut corrupt = valid.clone();
        corrupt.extend_from_slice(b"this-is-not-json\n");
        std::fs::write(&path, corrupt).unwrap();
        let error = SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .err()
            .expect("完整坏行必须拒绝");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        // 崩溃只可能留下未换行尾部残片；该残片丢弃后仍可恢复完整事件。
        let mut trailing = valid;
        trailing.extend_from_slice(b"{\"schema\":1");
        std::fs::write(&path, trailing).unwrap();
        let mut reopened = SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .expect("未完成尾部可恢复");
        assert_eq!(reopened.seq(), 2, "恢复游标等于最后完整 seq");
        let seq = reopened
            .append_event(&SessionEvent::UserSubmitted {
                content: "three".into(),
            })
            .unwrap();
        assert_eq!(seq, 3, "修复残片后必须连续续写");
        drop(reopened);
        assert_eq!(read_events(&path).unwrap().len(), 3);
    }

    #[test]
    fn session_reader_rejects_an_oversized_physical_event_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, format!("{}\n", "x".repeat(33))).unwrap();
        let error = read_envelopes_state_with_limits(&path, 32, 10)
            .err()
            .expect("oversized line must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("超过 32 字节上限"), "{error}");
    }

    #[test]
    fn open_repairs_missing_newline_before_append() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let mut log =
            SessionLog::create(&sessions_root, workspace.as_std_path(), RunId::new_v7()).unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "one".into(),
        })
        .unwrap();
        let session_id = log.session_id();
        let path = log.path().to_path_buf();
        drop(log);

        let mut raw = std::fs::read(&path).unwrap();
        assert_eq!(raw.pop(), Some(b'\n'));
        std::fs::write(&path, raw).unwrap();

        let mut reopened =
            SessionLog::open(&sessions_root, workspace.as_std_path(), session_id).unwrap();
        assert_eq!(reopened.seq(), 1);
        assert_eq!(
            reopened
                .append_event(&SessionEvent::UserSubmitted {
                    content: "two".into(),
                })
                .unwrap(),
            2
        );
        drop(reopened);
        assert_eq!(read_events(&path).unwrap().len(), 2);
    }

    #[test]
    fn session_has_a_single_exclusive_writer() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let first =
            SessionLog::create(&sessions_root, workspace.as_std_path(), RunId::new_v7()).unwrap();
        let session_id = first.session_id();

        let error = SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .err()
            .expect("第二个 writer 必须被拒绝");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

        drop(first);
        SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .expect("writer 退出后应能恢复 session");
    }

    #[test]
    fn append_rejects_invalid_tool_protocol_without_advancing_seq() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let mut log =
            SessionLog::create(&sessions_root, workspace.as_std_path(), RunId::new_v7()).unwrap();
        let call_id = ToolCallId::new_v7();
        let outcome = crate::outcome::ToolOutcome::succeeded("read", "ok".into()).into_stored();

        assert!(
            log.append_event(&SessionEvent::ToolCompleted { call_id, outcome })
                .is_err()
        );
        assert_eq!(log.seq(), 0, "被拒事件不得消耗 seq");

        log.append_event(&SessionEvent::ToolRequested {
            call: ToolCall {
                call_id,
                provider_id: "provider-call".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        })
        .unwrap();
        assert_eq!(log.seq(), 1);
    }

    #[test]
    fn compacted_range_rejects_invalid_ranges_and_prefers_later_equal_coverage() {
        let summary = |text: &str| SessionEvent::CompactionCommitted {
            covered: EventRange {
                start: EventId::from_u128(1),
                end: EventId::from_u128(3),
            },
            summary: CompactSummary { text: text.into() },
        };
        let events = vec![
            (
                2,
                SessionEvent::CompactionCommitted {
                    covered: EventRange {
                        start: EventId::from_u128(1),
                        end: EventId::from_u128(99),
                    },
                    summary: CompactSummary {
                        text: "invalid".into(),
                    },
                },
            ),
            (3, summary("first")),
            (5, summary("latest")),
        ];

        assert_eq!(
            compacted_range(&events),
            (Some(3), Some(5), Some("latest".into()))
        );
    }

    #[test]
    fn begin_run_rotates_envelope_run_id_without_changing_session() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let initial_run = RunId::new_v7();
        let mut log =
            SessionLog::create(&sessions_root, workspace.as_std_path(), initial_run).unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "first".into(),
        })
        .unwrap();
        let next_run = log.begin_run();
        assert_ne!(initial_run, next_run);
        log.append_event(&SessionEvent::UserSubmitted {
            content: "second".into(),
        })
        .unwrap();
        let session_id = log.session_id();
        let envelopes = read_envelopes(log.path()).unwrap();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].run_id, initial_run);
        assert_eq!(envelopes[1].run_id, next_run);
        assert!(envelopes.iter().all(|event| event.session_id == session_id));
    }

    /// §16：WallTimeExceeded 是新增完成原因，必须能持久化并读回（session 文件兼容）。
    #[test]
    fn wall_time_exceeded_round_trips_through_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = PathBuf::from(dir.path());
        let mut log = SessionLog::create(
            &dir.path().join("sessions"),
            workspace.as_path(),
            RunId::new_v7(),
        )
        .unwrap();
        log.append_event(&SessionEvent::RunCompleted {
            reason: CompletionReason::WallTimeExceeded,
            usage: Usage::default(),
        })
        .unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let events = read_events(&path).unwrap();
        assert!(
            matches!(
                events.first(),
                Some(SessionEvent::RunCompleted {
                    reason: CompletionReason::WallTimeExceeded,
                    ..
                })
            ),
            "WallTimeExceeded 必须可序列化/反序列化"
        );
    }

    /// §4.3：AssistantAttemptInterrupted 是记录型事件——partial content 持久化但
    /// 不进入对话投影（与 AssistantMessageCommitted 语义区分）。
    #[test]
    fn assistant_attempt_interrupted_round_trips_and_skips_projection() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = PathBuf::from(dir.path());
        let mut log = SessionLog::create(
            &dir.path().join("sessions"),
            workspace.as_path(),
            RunId::new_v7(),
        )
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "hello".into(),
        })
        .unwrap();
        log.append_event(&SessionEvent::AssistantAttemptInterrupted {
            request_id: RequestId::new_v7(),
            content: "部分输出已经".into(),
            cause: InterruptCause::Connection,
            saw_tool_calls: false,
        })
        .unwrap();
        log.append_event(&SessionEvent::RunCompleted {
            reason: CompletionReason::ProviderInterrupted,
            usage: Usage::default(),
        })
        .unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let events = read_events(&path).unwrap();
        assert!(
            matches!(
                events.get(1),
                Some(SessionEvent::AssistantAttemptInterrupted {
                    content,
                    cause: InterruptCause::Connection,
                    saw_tool_calls: false,
                    ..
                }) if content == "部分输出已经"
            ),
            "中断事件必须可序列化/反序列化"
        );
        // 投影：中断的 attempt 不产生 assistant 消息，也不中断后续投影。
        let messages = replay_messages(&path).unwrap();
        assert_eq!(messages.len(), 1, "只有 user 消息进入投影");
        assert!(matches!(messages[0], ChatMessage::User(_)));
    }
}
