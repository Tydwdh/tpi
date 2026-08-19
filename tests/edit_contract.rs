//! 可靠编辑协议契约测试（对应 §4.2 `tests/edit_contract.rs`）。
//!
//! §5/§6：模型不再可见 revision token——内部 BLAKE3 仅用于 CAS 和 journal。

use tpi::tool::edit::{is_valid_revision, revision_of};

#[test]
fn revision_of_produces_valid_b3() {
    // §10.1：BLAKE3 完整 256 bit digest。
    let raw = b"fn main() {}\n";
    let revision = revision_of(raw);
    assert!(is_valid_revision(&revision));
    assert_eq!(revision.len(), 3 + 64);
}

/// §`10.7：commit_edit` 成功即清理 temp；backup 保留到 `ToolCompleted` 持久化后
///（agent 层清理，见 `walking_skeleton` 的全流程无残留断言）。
#[test]
fn edit_commit_cleans_up_temp() {
    use tpi::tool::edit::{Replacement, apply_edit, commit_edit, prepare_commit};
    let dir = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
    let original = "fn main() {}\n";
    std::fs::write(path.as_std_path(), original).unwrap();
    let result = apply_edit(
        &path,
        &[Replacement {
            old_text: "main".into(),
            new_text: "run".into(),
        }],
    )
    .unwrap();
    let plan = prepare_commit(&path);
    commit_edit(&result, &path, &plan).unwrap();

    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".tpi-") && n.ends_with(".tmp") && n.contains("edit"))
        .collect();
    assert!(leftovers.is_empty(), "commit 后 temp 未清理: {leftovers:?}");
    // backup 必须仍在（崩溃恢复判定依赖它；§10.7 第 6 步）。
    assert!(
        plan.backup_path.unwrap().exists(),
        "ToolCompleted 持久化前 backup 必须保留"
    );
}

/// §10.6：write 成功后 temp 清理（no-clobber 安装不留 .tmp）。
#[test]
fn write_new_file_cleans_up_temp() {
    use tpi::tool::edit::{prepare_commit, write_new_file};
    let dir = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("b.rs")).unwrap();
    let plan = prepare_commit(&path);
    write_new_file(&path, b"fn b() {}\n", &plan).unwrap();
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "write 后目录残留临时文件: {leftovers:?}"
    );
}

/// 连续 edit 无需重新 read——old_text 匹配是唯一的 precondition。
#[test]
fn consecutive_edits_use_old_text_matching() {
    use tpi::tool::edit::{Replacement, apply_edit, commit_edit, prepare_commit};
    let dir = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("c.rs")).unwrap();
    std::fs::write(path.as_std_path(), "fn main() { let x = 1; }\n").unwrap();

    // 第一次 edit
    let r1 = apply_edit(
        &path,
        &[Replacement {
            old_text: "let x = 1".into(),
            new_text: "let x = 2".into(),
        }],
    )
    .unwrap();
    commit_edit(&r1, &path, &prepare_commit(&path)).unwrap();

    // 第二次 edit：直接基于当前文件内容，不需要传 revision。
    let r2 = apply_edit(
        &path,
        &[Replacement {
            old_text: "let x = 2".into(),
            new_text: "let x = 3".into(),
        }],
    )
    .unwrap();
    commit_edit(&r2, &path, &prepare_commit(&path)).unwrap();
    assert_eq!(r2.applied, 1);
    let final_text = std::fs::read_to_string(path.as_std_path()).unwrap();
    assert!(
        final_text.contains("let x = 3"),
        "第二次 edit 生效: {final_text}"
    );

    // old_text 仍唯一匹配 → 正常应用。
    let relaxed = apply_edit(
        &path,
        &[Replacement {
            old_text: "x = 3".into(),
            new_text: "x = 4".into(),
        }],
    )
    .expect("唯一匹配应正常应用");
    assert_eq!(relaxed.applied, 1);

    // old_text 不再匹配 → NoMatch。
    let no_match = apply_edit(
        &path,
        &[Replacement {
            old_text: "x = 99".into(),
            new_text: "x = 4".into(),
        }],
    )
    .unwrap_err();
    assert!(
        matches!(no_match, tpi::tool::edit::EditError::NoMatch { .. }),
        "old_text 失效时必须 NoMatch: {no_match:?}"
    );
}
