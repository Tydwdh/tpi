//! Phase F（任务书 §36）：crash 恢复矩阵——agent 级恢复测试。
//!
//! 覆盖：crash after ToolRequested / after ToolStarted（副作用前）/
//! after 副作用 before ToolCompleted。验收：恢复后不自动重放未知副作用、
//! assistant/tool protocol 仍合法、effect 明确、模型看到重新 inspect 提示。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_config;
use tpi::ids::{RunId, ToolCallId};
use tpi::provider::{ChatMessage, ToolCall};
use tpi::session::recovery::recover;
use tpi::session::{AssistantMessage, RecoveryMetadata, SessionEvent, SessionLog};
use tpi::tool::edit::Effect;

fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, workspace)
}

fn read_call(provider_id: &str) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new_v7(),
        provider_id: provider_id.into(),
        name: "read".into(),
        arguments: r#"{"path":"probe.txt"}"#.into(),
    }
}

/// 构造 session 并写入给定事件，返回 (config, workspace)。
fn write_events(
    workspace: &Utf8PathBuf,
    events: &[SessionEvent],
) -> (tpi::config::Config, std::path::PathBuf) {
    let config = test_config(workspace);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    for event in events {
        session.append_event(event).unwrap();
    }
    session.sync_data().unwrap();
    let path = session.path().to_path_buf();
    drop(session);
    (config, path)
}

/// §36 场景 A：crash after ToolRequested（无 ToolStarted/ToolCompleted）。
/// 恢复后：合成 Interrupted outcome（effect=not_applied），
/// resume history 序列合法（User → Assistant(tool_calls) → Tool(interrupted)）。
#[test]
fn crash_after_tool_requested_recovers_with_clear_effect() {
    let (_dir, workspace) = workspace();
    std::fs::write(workspace.join("probe.txt"), "P").unwrap();
    let call = read_call("call_read_1");
    let events = vec![
        SessionEvent::UserSubmitted {
            content: "读 probe.txt".into(),
        },
        SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: String::new(),
                tool_calls: vec![call.clone()],
            },
        },
        SessionEvent::ToolRequested { call },
    ];
    let (_config, path) = write_events(&workspace, &events);

    let recovery = recover(&path).unwrap();
    assert_eq!(recovery.interrupted.len(), 1, "必须合成 1 条 Interrupted");
    let (provider_id, outcome) = &recovery.interrupted[0];
    assert_eq!(provider_id, "call_read_1");
    assert_eq!(outcome.status, tpi::tool::outcome::ToolStatus::Interrupted);
    assert_eq!(outcome.model_payload.effect, Some(Effect::NotApplied));
    assert!(
        outcome.model_payload.output.contains("未自动重跑"),
        "必须提示未自动重跑: {}",
        outcome.model_payload.output
    );

    // resume history：replay（原事件投影）+ interrupted 注入 → 协议合法。
    let mut history = tpi::session::replay_messages(&path).unwrap();
    history.extend(tpi::agent::interrupted_as_messages(&recovery.interrupted));
    assert_eq!(history.len(), 3);
    assert_eq!(history[0], ChatMessage::User("读 probe.txt".into()));
    let ChatMessage::Assistant { tool_calls, .. } = &history[1] else {
        panic!("history[1] 必须是 Assistant");
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].provider_id, "call_read_1");
    let ChatMessage::Tool {
        tool_call_id, name, ..
    } = &history[2]
    else {
        panic!("history[2] 必须是 Tool");
    };
    assert_eq!(
        tool_call_id, "call_read_1",
        "tool_call_id 必须匹配 assistant 载体"
    );
    assert_eq!(name, "read");
}

/// §36 场景 B：crash after ToolStarted（write-ahead）before 副作用——
/// 文件未变（target 仍是 expected revision）→ effect=not_applied。
#[test]
fn crash_after_write_ahead_before_effect_is_not_applied() {
    let (_dir, workspace) = workspace();
    let target = workspace.join("a.txt");
    std::fs::write(&target, "old\n").unwrap();
    let expected = tpi::tool::edit::revision_of(&std::fs::read(&target).unwrap());

    let call_id = ToolCallId::new_v7();
    let events = vec![
        SessionEvent::UserSubmitted {
            content: "改 a.txt".into(),
        },
        SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    call_id,
                    provider_id: "call_edit_1".into(),
                    name: "edit".into(),
                    arguments: r#"{"path":"a.txt","revision":"x","replacements":[]}"#.into(),
                }],
            },
        },
        SessionEvent::ToolRequested {
            call: ToolCall {
                call_id,
                provider_id: "call_edit_1".into(),
                name: "edit".into(),
                arguments: r#"{"path":"a.txt","revision":"x","replacements":[]}"#.into(),
            },
        },
        SessionEvent::ToolStarted {
            call_id,
            recovery: Some(RecoveryMetadata {
                tool: "edit".into(),
                target_path: target.to_string(),
                expected_revision: expected.clone(),
                temp_path: workspace.join(".tpi-edit-xxx.tmp").to_string(),
                backup_path: None,
            }),
        },
    ];
    let (_config, path) = write_events(&workspace, &events);

    // 文件未被修改（crash 发生在副作用前）。
    assert_eq!(
        tpi::tool::edit::revision_of(&std::fs::read(&target).unwrap()),
        expected,
        "文件必须保持原样"
    );
    let recovery = recover(&path).unwrap();
    assert_eq!(recovery.interrupted.len(), 1);
    let (_, outcome) = &recovery.interrupted[0];
    assert_eq!(outcome.model_payload.effect, Some(Effect::NotApplied));
    assert!(
        outcome.model_payload.output.contains("未自动重跑"),
        "{}",
        outcome.model_payload.output
    );
}

/// §36 场景 C：crash after 副作用 before ToolCompleted——
/// 文件已变（revision != expected）→ effect=committed，模型能看到已提交事实。
#[test]
fn crash_after_effect_before_completed_is_committed() {
    let (_dir, workspace) = workspace();
    let target = workspace.join("a.txt");
    std::fs::write(&target, "old\n").unwrap();
    let expected = tpi::tool::edit::revision_of(&std::fs::read(&target).unwrap());

    let backup_path = workspace.join(".tpi-edit-yyy.bak");
    let call_id = ToolCallId::new_v7();
    let events = vec![
        SessionEvent::UserSubmitted {
            content: "改 a.txt".into(),
        },
        SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    call_id,
                    provider_id: "call_edit_2".into(),
                    name: "edit".into(),
                    arguments: r#"{"path":"a.txt","revision":"x","replacements":[]}"#.into(),
                }],
            },
        },
        SessionEvent::ToolRequested {
            call: ToolCall {
                call_id,
                provider_id: "call_edit_2".into(),
                name: "edit".into(),
                arguments: r#"{"path":"a.txt","revision":"x","replacements":[]}"#.into(),
            },
        },
        SessionEvent::ToolStarted {
            call_id,
            recovery: Some(RecoveryMetadata {
                tool: "edit".into(),
                target_path: target.to_string(),
                expected_revision: expected,
                temp_path: workspace.join(".tpi-edit-yyy.tmp").to_string(),
                backup_path: Some(backup_path.to_string()),
            }),
        },
    ];
    let (_config, path) = write_events(&workspace, &events);

    // 模拟：副作用已发生（文件已改、temp 已清理、backup 保留原内容）
    // 但 ToolCompleted 未落盘。
    std::fs::write(&target, "new\n").unwrap();
    std::fs::write(&backup_path, "old\n").unwrap();

    let recovery = recover(&path).unwrap();
    assert_eq!(recovery.interrupted.len(), 1);
    let (_, outcome) = &recovery.interrupted[0];
    assert_eq!(outcome.model_payload.effect, Some(Effect::Committed));
    assert!(
        outcome.model_payload.output.contains("未自动重跑"),
        "{}",
        outcome.model_payload.output
    );
}

/// §36 场景 D：crash 后恢复并继续对话——SessionLog 重新 open 后
/// 追加事件 seq 不冲突（单调递增），resume 后的完整序列协议合法。
#[test]
fn crash_then_resume_appends_with_monotonic_seq() {
    let (_dir, workspace) = workspace();
    std::fs::write(workspace.join("probe.txt"), "P").unwrap();
    let call = read_call("call_read_2");
    let events = vec![
        SessionEvent::UserSubmitted {
            content: "读 probe.txt".into(),
        },
        SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: String::new(),
                tool_calls: vec![call.clone()],
            },
        },
        SessionEvent::ToolRequested { call },
    ];
    let (config, path) = write_events(&workspace, &events);

    // 恢复：合成 Interrupted 并重新打开 session 继续写。
    let recovery = recover(&path).unwrap();
    let mut history = tpi::session::replay_messages(&path).unwrap();
    history.extend(tpi::agent::interrupted_as_messages(&recovery.interrupted));

    let mut session = SessionLog::open(&config.sessions_root, workspace.as_std_path(), {
        // 从文件名提取 session id。
        let name = path.file_name().unwrap().to_str().unwrap();
        let id = name.trim_end_matches(".jsonl");
        tpi::ids::SessionId(uuid::Uuid::parse_str(id).unwrap())
    })
    .unwrap();
    let seq = session
        .append_event(&SessionEvent::UserSubmitted {
            content: "继续".into(),
        })
        .unwrap();
    assert_eq!(seq, 5, "追加事件 seq 必须接续（4 条已有 + 1）");

    // 完整 resume 序列：User, Assistant(tool_calls), Tool(interrupted), User（追加后）。
    assert_eq!(history.len(), 3);
    let ChatMessage::Tool { content, .. } = &history[2] else {
        unreachable!()
    };
    // 模型看到 interrupted 消息后应重新 inspect：内容含提示。
    assert!(content.contains("重新读取"), "{content}");
}
