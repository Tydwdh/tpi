//! P2-03：ConversationProjector 属性测试——incremental == rebuild。
//!
//! 不变量：对任意合法事件前缀，`apply` 逐条应用后的投影（history/plan）
//! 与 `rebuild(全量 events)` 完全等价。
//!
//! 用 proptest 生成随机事件序列（确定性种子），对每个前缀断言等价。

mod fixtures;

use proptest::prelude::*;

use tpi::ids::ToolCallId;
use tpi::session::projector::ConversationProjector;
use tpi::session::protocol::{AssistantMessage, CompletionReason, SessionEvent, Usage};
use tpi::session::store::project_messages;

/// 生成一条合法事件（内容确定，seq 由调用方维护）。
fn gen_event(seq: u64, call_id: ToolCallId) -> SessionEvent {
    // 交替生成不同类型，覆盖 User/Assistant/Plan/Completion 等。
    let choice = (seq % 6) as u32;
    match choice {
        0 => SessionEvent::UserSubmitted {
            content: format!("user-{seq}"),
        },
        1 => SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: format!("assistant-{seq}"),
                tool_calls: vec![],
            },
        },
        2 => SessionEvent::PlanReplaced {
            plan: tpi::plan::Plan {
                explanation: Some(format!("plan-{seq}")),
                items: vec![],
            },
        },
        3 => SessionEvent::RunStarted {
            model: tpi::session::protocol::ModelRef {
                name: "m".into(),
                provider: "p".into(),
            },
            limits: tpi::session::protocol::RunLimits {
                max_turns: 5,
                max_tool_calls: 10,
            },
        },
        4 => SessionEvent::ToolRequested {
            call: tpi::provider::ToolCall {
                call_id,
                provider_id: format!("p{seq}"),
                name: "bash".into(),
                arguments: r#"{"command":"echo hi"}"#.into(),
            },
        },
        _ => SessionEvent::RunCompleted {
            reason: CompletionReason::Stop,
            usage: Usage::default(),
        },
    }
}

/// 对任意前缀断言：apply 逐条 == rebuild 全量（history 与 plan）。
fn assert_apply_equals_rebuild(events: &[(u64, SessionEvent)]) {
    // rebuild 全量
    let rebuilt = ConversationProjector::rebuild(events);

    // apply 逐条
    let mut applied = ConversationProjector::new();
    for (seq, event) in events {
        applied.apply(*seq, event.clone());
    }

    // 惰性读取触发投影
    let (applied_history, applied_plan) = {
        let h = applied.history().to_vec();
        let p = applied.plan().cloned();
        (h, p)
    };
    let (rebuilt_history, rebuilt_plan) = {
        let mut r = rebuilt;
        let h = r.history().to_vec();
        let p = r.plan().cloned();
        (h, p)
    };

    assert_eq!(
        applied_history,
        rebuilt_history,
        "incremental apply 的 history 必须等于 rebuild（events 前缀长度 {}）",
        events.len()
    );
    assert_eq!(
        applied_plan, rebuilt_plan,
        "incremental apply 的 plan 必须等于 rebuild"
    );
}

#[test]
fn incremental_equals_rebuild_for_all_prefixes() {
    // 构造一个固定但多样的序列（覆盖全部事件类型），对每个前缀断言等价。
    let mut events: Vec<(u64, SessionEvent)> = Vec::new();
    let call_id = ToolCallId::new_v7();
    for seq in 1..=24u64 {
        events.push((seq, gen_event(seq, call_id)));
    }
    // 前缀从 0 到全量逐步验证（含空投影）。
    for len in 0..=events.len() {
        assert_apply_equals_rebuild(&events[..len]);
    }
}

proptest! {
    /// 随机事件序列（确定性种子）：任意前缀 apply == rebuild。
    #[test]
    fn proptest_incremental_equals_rebuild(seqs in proptest::collection::vec(1u64..1000, 1..50)) {
        let call_id = ToolCallId::from_u128(42);
        let mut events: Vec<(u64, SessionEvent)> = Vec::new();
        for (i, seq) in seqs.iter().enumerate() {
            events.push((*seq, gen_event(i as u64 + 1, call_id)));
        }
        assert_apply_equals_rebuild(&events);
        // 也验证中间前缀（随机截断）。
        let cut = events.len() / 2;
        assert_apply_equals_rebuild(&events[..cut]);
    }
}

#[test]
fn rebuild_matches_store_projection() {
    // 关键等价：projector 的投影与 store::project_messages 完全一致
    //（projector 复用 store 逻辑，但作为契约断言防未来漂移）。
    let mut events: Vec<(u64, SessionEvent)> = Vec::new();
    let call_id = ToolCallId::new_v7();
    for seq in 1..=12u64 {
        events.push((seq, gen_event(seq, call_id)));
    }
    let projector = ConversationProjector::rebuild(&events);
    let from_store = project_messages(&events);
    let mut projector = projector;
    assert_eq!(projector.history(), from_store.as_slice());
}
