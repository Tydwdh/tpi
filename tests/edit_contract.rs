//! 可靠编辑协议契约测试（对应 §4.2 tests/edit_contract.rs）。
//!
//! §2.2：`read` 展示的 revision 是单独稳定字段，展示值必须可原样回传给 `edit`。
//! §10.1：协议传完整 256 bit digest：`b3:<64-hex>`。

use tpi::tool::edit::{
    format_revision_header, is_valid_revision, parse_revision_token, revision_of,
};

#[test]
fn revision_header_round_trips_unchanged() {
    // §10.1：BLAKE3 完整 256 bit digest。
    let raw = b"fn main() {}\n";
    let revision = revision_of(raw);
    assert!(is_valid_revision(&revision));
    assert_eq!(revision.len(), 3 + 64);

    let header = format_revision_header(&revision);
    assert_eq!(
        parse_revision_token(&header).as_deref(),
        Some(revision.as_str()),
        "read 输出的 revision 必须能原样回传给 edit（§2.2）"
    );
}

#[test]
fn bare_revision_token_is_accepted() {
    let revision = format!("b3:{}", "a".repeat(64));
    assert_eq!(
        parse_revision_token(&revision).as_deref(),
        Some(revision.as_str()),
        "edit 必须接受裸 HASH（§2.2：header 或 HASH 均可）"
    );
}

#[test]
fn invalid_revision_is_rejected() {
    assert_eq!(parse_revision_token("b3:short"), None);
    assert_eq!(parse_revision_token("sha256:abcd"), None);
    assert_eq!(parse_revision_token(""), None);
}

/// §10.7：commit_edit 成功即清理 temp；backup 保留到 ToolCompleted 持久化后
///（agent 层清理，见 walking_skeleton 的全流程无残留断言）。
#[test]
fn edit_commit_cleans_up_temp() {
    use tpi::tool::edit::{apply_edit, commit_edit, prepare_commit, revision_of, Replacement};
    let dir = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
    let original = "fn main() {}\n";
    std::fs::write(path.as_std_path(), original).unwrap();
    let revision = revision_of(original.as_bytes());
    let result = apply_edit(
        &path,
        &revision,
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
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".tpi-") && n.ends_with(".tmp") && n.contains("edit"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "commit 后 temp 未清理: {leftovers:?}"
    );
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
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "write 后目录残留临时文件: {leftovers:?}");
}
