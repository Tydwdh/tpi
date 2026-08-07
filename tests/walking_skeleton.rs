//! M1 Walking Skeleton 端到端验收（§21 M1）。
//!
//! fake provider 驱动 TPI 读取 fixture、修改一处代码、运行一次失败检查、
//! 再次修正并通过；验证 finish=stop 只有一次请求、session 持久化完整、
//! 模型可见真实 exit_code。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::fake_provider::{FakeProvider, FakeResponse, tool_call};
use fixtures::test_config;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::ids::RunId;
use tpi::session::{CompletionReason, SessionEvent, SessionLog, read_events};
use tpi::tool::edit::revision_of;
use tpi::tool::outcome::ToolStatus;

fn current_revision(workspace: &Utf8PathBuf, relative: &str) -> String {
    let path = workspace.join(relative);
    let raw = std::fs::read(&path).expect("fixture exists");
    revision_of(&raw)
}

/// bash 检查：文件中不含 "BUG" 时 exit 0，含时 exit 1。
fn bug_check_tool_call() -> tpi::provider::ToolCall {
    tool_call(
        "bash",
        serde_json::json!({
            "command": "powershell.exe -NoProfile -Command \"\\$c = Get-Content -Raw 'sample.rs'; if (\\$c -match 'BUG') { exit 1 } else { exit 0 }\"",
            "timeout_ms": 60000,
        }),
    )
}

#[tokio::test]
async fn fake_provider_drives_full_read_edit_verify_loop() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let fixture = workspace.join("sample.rs");
    std::fs::write(
        &fixture,
        "fn main() {\n    let a = 1; // BUG-A\n    let b = 2; // BUG-B\n    println!(\"{}\", a + b);\n}\n",
    )
    .unwrap();

    let workspace_read = workspace.clone();
    let workspace_edit_a = workspace.clone();
    let mut provider = FakeProvider::scripted(vec![
        // 1. read fixture（模型先读取）
        Box::new(move |_request| {
            FakeResponse::with_tool_calls(vec![tool_call(
                "read",
                serde_json::json!({"path": "sample.rs"}),
            )])
        }),
        // 2. 第一次修改：修复 BUG-A（revision 实时计算）
        Box::new(move |_request| {
            let revision = current_revision(&workspace_read, "sample.rs");
            FakeResponse::with_tool_calls(vec![tool_call(
                "edit",
                serde_json::json!({
                    "path": "sample.rs",
                    "revision": revision,
                    "replacements": [
                        {"old_text": "let a = 1; // BUG-A", "new_text": "let a = 1;"}
                    ],
                }),
            )])
        }),
        // 3. 运行失败检查（BUG-B 仍在 → 失败）
        Box::new(move |_request| FakeResponse::with_tool_calls(vec![bug_check_tool_call()])),
        // 4. 再次修正：修复 BUG-B
        Box::new(move |_request| {
            let revision = current_revision(&workspace_edit_a, "sample.rs");
            FakeResponse::with_tool_calls(vec![tool_call(
                "edit",
                serde_json::json!({
                    "path": "sample.rs",
                    "revision": revision,
                    "replacements": [
                        {"old_text": "let b = 2; // BUG-B", "new_text": "let b = 2;"}
                    ],
                }),
            )])
        }),
        // 5. 再次运行检查（应通过）
        Box::new(move |_request| FakeResponse::with_tool_calls(vec![bug_check_tool_call()])),
        // 6. 完成
        Box::new(move |_request| FakeResponse::text("两个 bug 已修复，检查通过。")),
    ]);

    let config = test_config(&workspace);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = mpsc::channel(128);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        &[],
        "修复 sample.rs 中的两个 bug".into(),
        tx,
        CancellationToken::new(),
        true,
    )
    .await
    .expect("end-to-end run succeeds");

    // 6 次请求：read → edit → run → edit → run → stop（无幽灵请求，§20.2 场景 11）。
    assert_eq!(provider.request_count, 6);
    assert_eq!(outcome.reason, CompletionReason::Stop);

    // 文件最终无任何 BUG 标记。
    let final_content = std::fs::read_to_string(&fixture).unwrap();
    assert!(
        !final_content.contains("BUG"),
        "fixture 应被完整修复: {final_content}"
    );

    // session 中记录的 run 工具 outcomes：第一次 failed(exit 1)，第二次 succeeded(exit 0)。
    let events = read_events(session.path()).expect("read session");
    let bash_outcomes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolCompleted {
                outcome,
                call_id: _,
            } if outcome.session_metadata.tool == "bash" => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(bash_outcomes.len(), 2);
    assert_eq!(bash_outcomes[0].status, ToolStatus::Failed);
    assert_eq!(bash_outcomes[0].model_payload.exit_code, Some(1));
    assert_eq!(bash_outcomes[1].status, ToolStatus::Succeeded);
    assert_eq!(bash_outcomes[1].model_payload.exit_code, Some(0));

    // write-ahead 顺序（§14.2）：每次 edit 前都有 ToolStarted + recovery metadata。
    // （bash 也属于 WorkspaceUnknown，同样有 recovery metadata，因此按 tool == "edit" 过滤。）
    let edit_started: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolStarted { call_id, recovery }
                if recovery.as_ref().map(|r| r.tool == "edit").unwrap_or(false) =>
            {
                Some((call_id, recovery.clone().unwrap()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(edit_started.len(), 2);
    assert_eq!(edit_started[0].1.tool, "edit");
    assert!(edit_started[0].1.expected_revision.starts_with("b3:"));
}

/// 崩溃后 session 可读且写工具不自动重放（§4.3/§14.2/§20.2 场景 13）。
#[tokio::test]
async fn recovery_never_replays_write_tools() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let fixture = workspace.join("sample.rs");
    std::fs::write(&fixture, "let x = 1;\n").unwrap();
    let config = test_config(&workspace);

    // 构造一个"崩溃"的 session：ToolRequested 后只有 ToolStarted（edit），无 ToolCompleted，
    // 尾部还有一行不完整的 JSON（崩溃残行，§14.2）。
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let call = tool_call(
        "edit",
        serde_json::json!({
            "path": "sample.rs",
            "revision": current_revision(&workspace, "sample.rs"),
            "replacements": [{"old_text": "let x = 1;", "new_text": "let x = 2;"}],
        }),
    );
    session
        .append_event(&SessionEvent::UserSubmitted {
            content: "fix it".into(),
        })
        .unwrap();
    session
        .append_event(&SessionEvent::ToolRequested { call: call.clone() })
        .unwrap();
    session
        .append_event(&SessionEvent::ToolStarted {
            call_id: call.call_id,
            recovery: Some(tpi::session::RecoveryMetadata {
                tool: "edit".into(),
                target_path: "sample.rs".into(),
                expected_revision: current_revision(&workspace, "sample.rs"),
                temp_path: String::new(),
                backup_path: None,
            }),
        })
        .unwrap();
    session.sync_data().unwrap();
    // 崩溃残行：写入不完整的 JSON。
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(session.path())
        .unwrap();
    writeln!(file, "{{\"schema\":1,\"seq\":99,\"event_id\":\"broken").unwrap();
    drop(file);

    // 恢复：残行丢弃；未完成 edit 合成 Interrupted(unknown)；绝不重放工具。
    let recovery = tpi::session::recovery::recover(session.path()).unwrap();
    let types: Vec<&str> = recovery
        .events
        .iter()
        .map(SessionEvent::type_name)
        .collect();
    assert_eq!(
        types,
        vec!["user_submitted", "tool_requested", "tool_started"]
    );
    assert_eq!(recovery.interrupted.len(), 1);
    let (provider_id, outcome) = &recovery.interrupted[0];
    assert_eq!(provider_id, &call.provider_id);
    assert_eq!(outcome.status, ToolStatus::Interrupted);
    assert_eq!(
        outcome.model_payload.effect,
        Some(tpi::tool::outcome::Effect::Unknown)
    );
    assert!(
        outcome.model_payload.output.contains("未自动重跑"),
        "恢复结果必须告知模型：未自动重跑（§4.3）"
    );

    // 文件零变化：工具没有被重放。
    let content = std::fs::read_to_string(&fixture).unwrap();
    assert_eq!(content, "let x = 1;\n");
}
