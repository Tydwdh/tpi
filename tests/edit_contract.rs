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
