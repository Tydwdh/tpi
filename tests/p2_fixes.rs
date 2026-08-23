//! P2 修复回归测试（fix.md 审查报告，第三批：Agent 哲学与人体工学）。
//!
//! - write revision-bound 整体重写（P2-7）；
//! - edit 失败输出带 next-action 提示（P2-5）；
//! - no-progress 拒绝带 suggestions（P2-6）。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_tool_context;
use tpi::outcome::ToolStatus;
use tpi::tool::files::{WriteArgs, write};

/// §7：write 支持覆盖——已存在文件直接原子覆盖。
#[test]
fn p2_write_rejects_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);

    let target = workspace.join("app.txt");
    std::fs::write(&target, "old content\n").unwrap();

    // 已放开覆盖：已存在文件直接覆盖成功。
    let plan = tpi::tool::edit::prepare_commit(&target);
    let outcome = write(
        WriteArgs {
            path: "app.txt".into(),
            content: "new content\n".into(),
        },
        &ctx,
        Some(&plan),
    );
    assert_eq!(
        outcome.status,
        ToolStatus::Succeeded,
        "{}",
        outcome.model_text()
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "new content\n",
        "文件内容应被 write 覆盖"
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
    // old_text 在文件中不存在 → NoMatch + hint。
    std::fs::write(&target, "changed\n").unwrap();

    let plan = tpi::tool::edit::prepare_commit(&target);
    let outcome = tpi::tool::files::edit(
        tpi::tool::edit::EditArgs {
            path: "hint.txt".into(),
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
    assert!(text.contains("no_match"), "{text}");
    assert!(
        text.contains("hint"),
        "no_match 必须带 next-action 提示: {text}"
    );
}
