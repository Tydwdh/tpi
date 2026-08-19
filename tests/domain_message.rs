//! P1-02：domain message 双向 adapter 的 golden parity 测试。
//!
//! 验收：所有 golden model requests byte/semantic 等价；provider-specific field
//! 不进入 domain。具体断言：
//!
//! 1. 双向往返：合法 `ChatMessage` 经 `ChatMessage -> DomainMessage -> ChatMessage`
//!    语义等价（role / text / `tool_calls` / tool result 逐字段）。
//! 2. projection parity：`session::project_messages`（events -> domain -> provider）
//!    与直接 `events -> ChatMessage` 语义等价——重构内部先产出 domain message，
//!    对外 `ChatMessage` 契约不变。
//! 3. corpus parity：真实 `session（session_golden` corpus `001_tool_loop）经`
//!    `replay_domain_messages -> ChatMessage::from` 与 `replay_messages` 等价。

mod fixtures;

use tpi::message::{DomainContentBlock, DomainMessage, DomainRole};
use tpi::provider::{ChatMessage, ToolCall};
use tpi::session::{SessionEvent, project_domain_messages, project_messages, replay_messages};

fn tool_call(id: &str) -> ToolCall {
    ToolCall {
        call_id: tpi::ids::ToolCallId::new_v7(),
        provider_id: id.to_string(),
        name: "bash".into(),
        arguments: r#"{"command":"echo hi"}"#.into(),
    }
}

#[test]
fn roundtrip_preserves_semantics() {
    let cases = vec![
        ChatMessage::System("be terse".into()),
        ChatMessage::User("你好，帮我修复".into()),
        ChatMessage::Assistant {
            content: "先看文件".into(),
            tool_calls: vec![tool_call("tc1"), tool_call("tc2")],
        },
        ChatMessage::Assistant {
            content: String::new(),
            tool_calls: vec![tool_call("tc3")],
        },
        ChatMessage::Tool {
            tool_call_id: "tc1".into(),
            name: "bash".into(),
            content: "ok".into(),
        },
    ];
    for original in cases {
        let domain = DomainMessage::from(&original);
        let back = ChatMessage::from(&domain);
        assert_eq!(
            back, original,
            "ChatMessage -> DomainMessage -> ChatMessage 应语义等价"
        );
    }
}

#[test]
fn domain_has_no_provider_specific_fields() {
    // provider-specific 信息（base_url/finish reason 等）不在 domain 里——
    // domain 只表达 role + content（类型系统保证：DomainMessage 只有 role/content）。
    let call = tool_call("x");
    let assistant = DomainMessage::from(&ChatMessage::Assistant {
        content: "c".into(),
        tool_calls: vec![call.clone()],
    });
    assert_eq!(assistant.role, DomainRole::Assistant);
    assert_eq!(
        assistant.content,
        vec![
            DomainContentBlock::Text("c".into()),
            DomainContentBlock::ToolCall(call),
        ]
    );
}

/// 构造包含 User/Assistant(+tool calls)/Tool result 的事件序列，验证
/// `project_messages（domain` 中转）与直接投影语义一致。
#[test]
fn projection_parity_domain_vs_direct() {
    let c1 = tool_call("c1");
    let events = vec![
        (
            1u64,
            SessionEvent::UserSubmitted {
                content: "u1".into(),
            },
        ),
        (
            2,
            SessionEvent::AssistantMessageCommitted {
                message: tpi::session::AssistantMessage {
                    content: "a1".into(),
                    tool_calls: vec![c1.clone()],
                },
            },
        ),
        (3, SessionEvent::ToolRequested { call: c1.clone() }),
        (
            4,
            SessionEvent::ToolCompleted {
                call_id: c1.call_id,
                outcome: tool_outcome("done"),
            },
        ),
        (
            5,
            SessionEvent::UserSubmitted {
                content: "u2".into(),
            },
        ),
        (
            6,
            SessionEvent::AssistantMessageCommitted {
                message: tpi::session::AssistantMessage {
                    content: "a2".into(),
                    tool_calls: Vec::new(),
                },
            },
        ),
    ];
    // 直接投影 ChatMessage（旧路径：events -> ChatMessage）。
    let direct = project_messages(&events);
    assert_eq!(direct.len(), 5, "User/Assistant/Tool/User/Assistant");
    // domain 中转投影（新路径：events -> domain -> ChatMessage）必须等价。
    let domains = project_domain_messages(&events);
    let via_domain: Vec<ChatMessage> = domains.iter().map(ChatMessage::from).collect();
    assert_eq!(
        via_domain, direct,
        "events->domain->ChatMessage 与 events->ChatMessage 等价"
    );
}

#[test]
fn corpus_replay_parity() {
    // 真实 corpus（session_golden 001_tool_loop）：domain 中转与直接 replay 等价。
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/session_corpus/001_tool_loop.jsonl");
    let direct = replay_messages(&path).expect("replay_messages");
    let domains = tpi::session::replay_domain_messages(&path).expect("replay_domain_messages");
    let via_domain: Vec<ChatMessage> = domains.iter().map(ChatMessage::from).collect();
    assert_eq!(
        via_domain, direct,
        "真实 session 的 domain 中转与直接 replay 必须语义等价（golden parity）"
    );
    assert!(!direct.is_empty());
}

fn tool_outcome(text: &str) -> tpi::outcome::StoredToolOutcome {
    tpi::outcome::StoredToolOutcome {
        status: tpi::outcome::ToolStatus::Succeeded,
        session_metadata: tpi::outcome::ToolMetadata {
            tool: "bash".into(),
            target: None,
            program: None,
            timeout_ms: None,
            diff: None,
        },
        model_payload: tpi::outcome::ModelPayload {
            status: tpi::outcome::ToolStatus::Succeeded,
            program: Some("bash".into()),
            exit_code: Some(0),
            duration_ms: 0,
            output: text.into(),
            effect: None,
            artifact: None,
        },
    }
}
