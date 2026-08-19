//! M3 竞态与恢复契约测试（§10.4/§10.7、§20.2 场景 18/19）。
//!
//! - §20.2 场景 18：写工具在副作用前已持久化 recovery metadata；分别在 replace 前、
//!   replace 后、ToolCompleted 前崩溃都能恢复/诊断；
//! - §20.2 场景 19：外部 writer 在最终 revision check 后竞争修改时，
//!   backup digest 检测到并进入恢复，而不是报告成功。

mod fixtures;

use camino::Utf8PathBuf;
use tpi::outcome::Effect;
use tpi::session::{RecoveryMetadata, SessionEvent, SessionLog};
use tpi::tool::edit::CommitPlan;
use tpi::tool::edit::{
    Replacement, apply_edit, commit_edit, prepare_commit, revision_of, verify_after_replace,
};

/// 构造一个带 recovery metadata 的 `ToolStarted` + 缺 `ToolCompleted` 的崩溃 session。
fn crashed_session(
    workspace: &Utf8PathBuf,
    relative_target: &str,
    expected_revision: &str,
    plan: &CommitPlan,
) -> SessionLog {
    let config = fixtures::test_config(workspace);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        tpi::ids::RunId::new_v7(),
    )
    .unwrap();
    let call = fixtures::fake_provider::tool_call(
        "edit",
        serde_json::json!({
            "path": relative_target,
            "revision": expected_revision,
            "replacements": [{"old_text": "x", "new_text": "y"}],
        }),
    );
    session
        .append_event(&SessionEvent::UserSubmitted {
            content: "fix".into(),
        })
        .unwrap();
    session
        .append_event(&SessionEvent::ToolRequested { call: call.clone() })
        .unwrap();
    session
        .append_event(&SessionEvent::ToolStarted {
            call_id: call.call_id,
            recovery: Some(RecoveryMetadata {
                tool: "edit".into(),
                target_path: workspace.join(relative_target).to_string(),
                expected_revision: expected_revision.into(),
                candidate_revision: None,
                temp_path: plan.temp_path.to_string_lossy().to_string(),
                backup_path: plan
                    .backup_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
            }),
        })
        .unwrap();
    session.sync_data().unwrap();
    session
}

/// 场景 18a：replace 前崩溃（temp 未创建、target 仍为旧内容）→ `effect=not_applied`。
#[tokio::test]
async fn crash_before_replace_recovers_as_not_applied() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let target = workspace.join("f.txt");
    let content = "hello x world\n";
    std::fs::write(&target, content).unwrap();
    let expected = revision_of(content.as_bytes());
    let plan = prepare_commit(&workspace.join("f.txt"));

    let session = crashed_session(&workspace, "f.txt", &expected, &plan);
    let recovery = tpi::session::recovery::recover(session.path()).unwrap();
    assert_eq!(recovery.interrupted.len(), 1);
    assert_eq!(
        recovery.interrupted[0].2.model_payload.effect,
        Some(Effect::NotApplied),
        "replace 前崩溃：target == expected → not_applied"
    );
    // 文件零变化。
    assert_eq!(std::fs::read_to_string(&target).unwrap(), content);
}

/// 场景 18b：replace 后、ToolCompleted 前崩溃（target 已更新、temp 残留）→ effect=committed。
#[tokio::test]
async fn crash_after_replace_recovers_as_committed() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let target = workspace.join("f.txt");
    let content = "hello x world\n";
    std::fs::write(&target, content).unwrap();
    let plan = prepare_commit(&workspace.join("f.txt"));

    // 真实执行提交（模拟崩溃发生在 commit 后、ToolCompleted 前）。
    let result = apply_edit(
        &workspace.join("f.txt"),
        &[Replacement {
            old_text: "x".into(),
            new_text: "y".into(),
        }],
    )
    .unwrap();
    commit_edit(&result, &workspace.join("f.txt"), &plan).unwrap();
    let expected = revision_of(content.as_bytes());
    let new_content = std::fs::read_to_string(&target).unwrap();
    assert!(new_content.contains('y'));

    let session = crashed_session(&workspace, "f.txt", &expected, &plan);
    let recovery = tpi::session::recovery::recover(session.path()).unwrap();
    assert_eq!(recovery.interrupted.len(), 1);
    assert_eq!(
        recovery.interrupted[0].2.model_payload.effect,
        Some(Effect::Committed),
        "replace 后崩溃：target == temp digest → committed"
    );
}

/// 场景 19：外部 writer 在最终 revision check 后竞争修改 → backup digest 检测到并发，
/// 恢复 target（保留外部修改）并返回 `concurrent_modification_during_commit，不报告成功`。
#[test]
fn concurrent_external_writer_detected_via_backup_digest() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let target = workspace.join("f.txt");
    let original = "hello x world\n";
    std::fs::write(&target, original).unwrap();
    let plan = prepare_commit(&target);

    // 提交（成功；backup 保留旧内容，由调用方在 ToolCompleted 后删除——这里保留以模拟竞态）。
    let result = apply_edit(
        &target,
        &[Replacement {
            old_text: "x".into(),
            new_text: "y".into(),
        }],
    )
    .unwrap();
    commit_edit(&result, &target, &plan).unwrap();
    let expected = revision_of(original.as_bytes());

    // 外部 writer 在 ReplaceFileW 前修改了旧文件：backup 内容 != expected。
    let external = b"external modification\n";
    std::fs::write(plan.backup_path.as_ref().unwrap(), external).unwrap();

    // 重新校验（模拟 ToolCompleted 前的 backup 核对，§10.7 第 4 步）：
    // backup digest 与 expected 不符 → 恢复 + 并发诊断，不得报告成功。
    let candidate = revision_of(&result.new_raw);
    let error =
        verify_after_replace(&target, &plan.backup_path, &expected, &candidate).unwrap_err();
    assert_eq!(error.code(), "concurrent_modification_during_commit");

    // 恢复流程：target 还原为 backup（外部修改保留），我们的编辑被撤销。
    let after = std::fs::read_to_string(&target).unwrap();
    assert_eq!(after, "external modification\n", "恢复必须保留外部修改");
}
