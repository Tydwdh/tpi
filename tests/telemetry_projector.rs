//! P2-09：O3 session telemetry projector 验收测试。
//!
//! 覆盖 roadmap 验收：
//! - 任意合法 prefix：incremental == replay（逐条 project 与 rebuild 等价）；
//! - 去重：`(session_id,event_seq,projector_version)`——handoff 重复幂等忽略；
//! - 无声明缺口：seq 跳变 → `TelemetryGap` 记录；
//! - sink drop 不影响 append（projector 是纯状态，不依赖 sink）。

use tpi::ids::SessionId;
use tpi::session::protocol::{CompletionReason, SessionEvent, Usage};
use tpi::session::telemetry::{PROJECTOR_VERSION, SessionTelemetryProjector, TelemetryGap};

fn sid() -> SessionId {
    SessionId::from_u128(7)
}

fn user_event(seq: u64) -> (u64, SessionEvent) {
    (
        seq,
        SessionEvent::UserSubmitted {
            content: format!("msg-{seq}"),
        },
    )
}

fn run_completed(seq: u64) -> (u64, SessionEvent) {
    (
        seq,
        SessionEvent::RunCompleted {
            reason: CompletionReason::Stop,
            usage: Usage::default(),
        },
    )
}

/// 构造一个事件序列（覆盖 User/RunStarted/Assistant/Tool/RunCompleted）。
fn make_events() -> Vec<(u64, SessionEvent)> {
    let mut events = vec![user_event(1)];
    events.push((
        2,
        SessionEvent::RunStarted {
            model: tpi::session::protocol::ModelRef {
                name: "m".into(),
                provider: "p".into(),
            },
            limits: tpi::session::protocol::RunLimits {
                max_turns: 5,
                max_tool_calls: 10,
            },
        },
    ));
    events.push((
        3,
        SessionEvent::AssistantMessageCommitted {
            message: tpi::session::protocol::AssistantMessage {
                content: "a".into(),
                tool_calls: vec![],
            },
        },
    ));
    events.push((
        4,
        SessionEvent::ToolRequested {
            call: tpi::provider::ToolCall {
                call_id: tpi::ids::ToolCallId::from_u128(1),
                provider_id: "p1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"echo hi"}"#.into(),
            },
        },
    ));
    events.push(run_completed(5));
    events
}

/// 任意前缀：incremental（逐条 project）== replay（rebuild）。
#[test]
fn incremental_equals_replay_for_all_prefixes() {
    let events = make_events();
    for len in 0..=events.len() {
        let mut incremental = SessionTelemetryProjector::new(sid());
        for (seq, event) in &events[..len] {
            incremental.project(*seq, event);
        }
        let replay = SessionTelemetryProjector::rebuild(sid(), &events[..len]);
        assert_eq!(
            incremental.records(),
            replay.records(),
            "prefix 长度 {len}：incremental != replay"
        );
        assert_eq!(incremental.gaps(), replay.gaps());
        assert_eq!(incremental.len(), len);
    }
}

/// 去重：handoff 重复（同 seq 重放）幂等，不产生额外记录。
#[test]
fn duplicate_handoff_is_idempotent() {
    let mut p = SessionTelemetryProjector::new(sid());
    let (seq, event) = user_event(1);
    p.project(seq, &event);
    p.project(seq, &event); // 重复（handoff 重放）
    p.project(seq, &event); // 再重复
    assert_eq!(p.len(), 1, "重复 seq 必须幂等忽略");
    assert_eq!(p.last_seq(), 1);
    assert!(p.gaps().is_empty());
}

/// 无声明缺口：seq 跳变（1 → 3）必须记录 `TelemetryGap`。
#[test]
fn seq_jump_declares_gap() {
    let mut p = SessionTelemetryProjector::new(sid());
    let (_, e1) = user_event(1);
    let (_, e3) = run_completed(3);
    p.project(1, &e1);
    p.project(3, &e3); // 跳过 2：无声明缺口
    assert_eq!(p.gaps().len(), 1, "seq 跳变必须声明 gap");
    assert_eq!(
        p.gaps()[0],
        TelemetryGap {
            session_id: sid(),
            from_seq: 1,
            to_seq: 3,
            reason: "unexpected seq jump",
        }
    );
    assert_eq!(p.len(), 2, "跳变后仍投影两条记录");
}

/// 去重三元组：record 携带 `session_id/event_seq/projector_version`。
#[test]
fn dedup_triple_is_present() {
    let mut p = SessionTelemetryProjector::new(sid());
    let (seq, event) = user_event(1);
    p.project(seq, &event);
    let r = &p.records()[0];
    assert_eq!(r.session_id, sid());
    assert_eq!(r.event_seq, 1);
    assert_eq!(r.projector_version, PROJECTOR_VERSION);
    assert_eq!(r.event_type, "user_submitted");
    // Standard 不含正文：sidecar_seq 为 None，counts 为 0。
    assert_eq!(r.sidecar_seq, None);
}

/// 计数：ToolRequested → `tool_calls=1；Interrupted` → interrupted=1。
#[test]
fn counts_are_metadata_only() {
    let mut p = SessionTelemetryProjector::new(sid());
    p.project(
        1,
        &SessionEvent::ToolRequested {
            call: tpi::provider::ToolCall {
                call_id: tpi::ids::ToolCallId::from_u128(2),
                provider_id: "p".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
        },
    );
    p.project(
        2,
        &SessionEvent::AssistantAttemptInterrupted {
            request_id: tpi::ids::RequestId::from_u128(3),
            content: "partial".into(),
            cause: tpi::session::protocol::InterruptCause::Connection,
            saw_tool_calls: false,
        },
    );
    assert_eq!(p.records()[0].counts.tool_calls, 1);
    assert_eq!(p.records()[1].counts.interrupted, 1);
    // 正文不在 record（content 未存）。
    assert_eq!(p.records()[1].counts.tool_calls, 0);
}

/// sink drop 不影响 append：projector 是纯状态，不依赖任何 sink。
#[test]
fn sink_drop_does_not_affect_append() {
    let mut p = SessionTelemetryProjector::new(sid());
    // 模拟：record 被消费（drop）后，继续 append 仍正常。
    let (seq, event) = user_event(1);
    p.project(seq, &event);
    let consumed = p.records().to_vec();
    drop(consumed);
    p.project(2, &run_completed(2).1);
    assert_eq!(p.len(), 2, "sink drop 后 append 不受影响");
}
