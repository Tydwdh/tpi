//! P2 修复回归测试（fix.md 审查报告，第三批：Agent 哲学与人体工学）。
//!
//! - write revision-bound 整体重写（P2-7）；
//! - edit 失败输出带 next-action 提示（P2-5）；
//! - no-progress 拒绝带 suggestions（P2-6）。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_tool_context;
use tpi::tool::files::{write, WriteArgs};
use tpi::tool::outcome::ToolStatus;

/// P2：write 支持 revision-bound 整体重写（此前只能新建文件）。
/// - 已存在文件不带 revision → 明确拒绝（给出下一步）；
/// - revision 不匹配 → stale_rejected（可恢复引导）；
/// - revision 匹配 → 整体重写成功，返回 previous/current revision。
#[test]
fn p2_write_rewrite_requires_matching_revision() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);

    let target = workspace.join("app.txt");
    std::fs::write(&target, "old content\n").unwrap();
    let current = tpi::tool::edit::revision_of(b"old content\n");

    // 1. 不带 revision → 拒绝并提示。
    let plan = tpi::tool::edit::prepare_commit(&target);
    let outcome = write(
        WriteArgs {
            path: "app.txt".into(),
            content: "new content\n".into(),
            revision: None,
        },
        &ctx,
        Some(&plan),
    );
    assert_eq!(outcome.status, ToolStatus::Failed, "{}", outcome.model_text());
    assert!(
        outcome.model_text().contains("already_exists"),
        "已存在文件必须明确拒绝: {}",
        outcome.model_text()
    );

    // 2. revision 不匹配 → stale_rejected + hint。
    let plan = tpi::tool::edit::prepare_commit(&target);
    let outcome = write(
        WriteArgs {
            path: "app.txt".into(),
            content: "new content\n".into(),
            revision: Some("b3:wrong-revision".into()),
        },
        &ctx,
        Some(&plan),
    );
    assert_eq!(outcome.status, ToolStatus::Failed, "{}", outcome.model_text());
    assert!(
        outcome.model_text().contains("stale_revision"),
        "{}",
        outcome.model_text()
    );

    // 3. revision 匹配 → 重写成功。
    let plan = tpi::tool::edit::prepare_commit(&target);
    let outcome = write(
        WriteArgs {
            path: "app.txt".into(),
            content: "new content\n".into(),
            revision: Some(current.clone()),
        },
        &ctx,
        Some(&plan),
    );
    assert_eq!(outcome.status, ToolStatus::Succeeded, "{}", outcome.model_text());
    let text = outcome.model_text();
    assert!(text.contains("rewritten: true"), "{text}");
    assert!(text.contains(&current), "previous_revision: {text}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "new content\n",
        "文件内容必须被替换"
    );
}

/// P2：edit 失败输出带明确的下一步动作（可恢复引导，而非硬拒绝）。
#[test]
fn p2_edit_failure_includes_next_action_hint() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);

    let target = workspace.join("hint.txt");
    std::fs::write(&target, "line one\n").unwrap();
    let revision = tpi::tool::edit::revision_of(b"line one\n");

    // 先改文件使 revision 过期。
    std::fs::write(&target, "changed\n").unwrap();

    let plan = tpi::tool::edit::prepare_commit(&target);
    let outcome = tpi::tool::files::edit(
        tpi::tool::edit::EditArgs {
            path: "hint.txt".into(),
            revision: revision.clone(),
            replacements: vec![tpi::tool::edit::Replacement {
                old_text: "line one".into(),
                new_text: "line two".into(),
            }],
        },
        &ctx,
        Some(&plan),
    );
    assert_eq!(outcome.status, ToolStatus::Failed);
    let text = outcome.model_text();
    assert!(text.contains("stale_revision"), "{text}");
    assert!(
        text.contains("重新 read"),
        "stale 必须带 next-action 提示: {text}"
    );
}
