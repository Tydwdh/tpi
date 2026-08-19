//! 可靠编辑协议契约测试（对应 §4.2 `tests/edit_contract.rs`）。
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

/// §`10.7：commit_edit` 成功即清理 temp；backup 保留到 `ToolCompleted` 持久化后
///（agent 层清理，见 `walking_skeleton` 的全流程无残留断言）。
#[test]
fn edit_commit_cleans_up_temp() {
    use tpi::tool::edit::{Replacement, apply_edit, commit_edit, prepare_commit, revision_of};
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

/// §用户诉求：连续 edit 无需重新 read——第一次 edit 返回的 `current_revision`
/// 可直接用于第二次 `edit（apply_edit` 从磁盘重读 digest 匹配）。
#[test]
fn consecutive_edits_use_current_revision_without_read() {
    use tpi::tool::edit::{Replacement, apply_edit, commit_edit, prepare_commit};
    let dir = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("c.rs")).unwrap();
    std::fs::write(path.as_std_path(), "fn main() { let x = 1; }\n").unwrap();

    // 第一次 edit：需要先读取得 revision（read 的 digest）。
    let initial = std::fs::read_to_string(path.as_std_path()).unwrap();
    let rev1 = revision_of(initial.as_bytes());
    let r1 = apply_edit(
        &path,
        &rev1,
        &[Replacement {
            old_text: "let x = 1".into(),
            new_text: "let x = 2".into(),
        }],
    )
    .unwrap();
    commit_edit(&r1, &path, &prepare_commit(&path)).unwrap();
    // 第一次 edit 输出 current_revision = 提交后文件 digest。
    let rev2 = r1.current_revision.clone();
    assert_ne!(rev1, rev2, "edit 后 revision 必须变化");

    // 第二次 edit：直接传第一次的 current_revision，不重新 read。
    let r2 = apply_edit(
        &path,
        &rev2,
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

    // §修复 #1：旧 revision + old_text 仍唯一匹配 → 宽松应用（免重新 read）。
    // （外部空白改动后目标 old_text 仍唯一存在，无需因 revision 过期重读。）
    let relaxed = apply_edit(
        &path,
        &rev1,
        &[Replacement {
            old_text: "x = 3".into(),
            new_text: "x = 4".into(),
        }],
    )
    .expect("旧 revision 但 old_text 仍唯一匹配应宽松应用");
    assert_eq!(relaxed.applied, 1);

    // 旧 revision + old_text 不再匹配 → 仍 stale（内容失效才需重新 read）。
    let stale = apply_edit(
        &path,
        &rev1,
        &[Replacement {
            old_text: "x = 99".into(),
            new_text: "x = 4".into(),
        }],
    )
    .unwrap_err();
    assert!(
        matches!(stale, tpi::tool::edit::EditError::StaleRevision { .. }),
        "old_text 失效时旧 revision 必须 stale: {stale:?}"
    );
}
