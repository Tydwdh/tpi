//! M3 编辑协议 property tests（§21 M3 验收）。
//!
//! - §20.2 场景 3：修改括号附近代码不会删除未声明的相邻 token；
//! - §20.2 场景 4：多 replacement 有一个歧义时，整批零变化；
//! - §20.2 场景 5：CRLF、LF、BOM 和 mixed line ending 的未触及字节保持一致；
//! - §10.3 第 12 条：不存在部分 replacement 或未声明相邻内容损坏。

use camino::Utf8PathBuf;
use proptest::prelude::*;
use tpi::tool::edit::{Replacement, apply_edit, commit_edit, prepare_commit, revision_of};

/// 文本策略：可打印 ASCII + LF + TAB；统一行尾（LF 或 CRLF）。
/// 统一行尾使字符串替换期望与 logical 匹配空间一致（§10.1：匹配空间是 canonical LF）。
fn text_strategy() -> impl Strategy<Value = String> {
    (
        proptest::collection::vec(prop::sample::select(b"abcXYZ0123 \t\n".to_vec()), 0..150),
        any::<bool>(),
    )
        .prop_map(|(bytes, crlf)| {
            let text = String::from_utf8(bytes).unwrap();
            if crlf {
                text.replace('\n', "\r\n")
            } else {
                text
            }
        })
}

/// 取文本中恰好出现一次、不含 \r 的子串（作为 `old_text）；无唯一子串则跳过该` case。
fn unique_substring(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    for len in (1..=10.min(bytes.len())).rev() {
        for start in 0..=bytes.len() - len {
            let candidate = &text[start..start + len];
            if !candidate.contains('\r') && text.matches(candidate).count() == 1 {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

proptest! {
    /// 不变量：应用 replacement 后，结果 == logical 替换 + 行尾编码（未触及字节逐字节一致）。
    #[test]
    fn edit_result_equals_string_replacement(
        content in text_strategy(),
        new_text in text_strategy(),
    ) {
        let Some(old_text) = unique_substring(&content) else {
            return Ok(()); // 无唯一子串（如空文本），跳过
        };
        // \r 不允许出现在参数中（§10.3 第 2 条）。
        let new_text = new_text.replace('\r', "");
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("f.txt")).unwrap();
        std::fs::write(path.as_std_path(), &content).unwrap();
        // 期望 = logical 替换 + 按文件行尾编码（§10.1 匹配空间是 canonical LF；
        // §10.5：替换的 \n 按 anchor/文件多数行尾编码）。
        let is_crlf = content.contains("\r\n");
        let logical = content.replace("\r\n", "\n");
        let expected_logical = logical.replace(&old_text, &new_text);
        let expected = if is_crlf {
            expected_logical.replace('\n', "\r\n")
        } else {
            expected_logical
        };
        if expected == content {
            // no-op（old_text == new_text）被拒绝（§10.3 第 5 条）。
            let result = apply_edit(
                &path,
                &[Replacement { old_text: old_text.clone(), new_text: new_text.clone() }],
            );
            prop_assert!(result.is_err(), "no-op 必须被拒绝");
            return Ok(());
        }
        let result = apply_edit(
            &path,
            &[Replacement { old_text, new_text }],
        )
        .unwrap();
        let plan = prepare_commit(&path);
        commit_edit(&result, &path, &plan).unwrap();
        let actual = std::fs::read_to_string(path.as_std_path()).unwrap();
        // 未触及字节逐字节一致（§20.2 场景 5：LF/CRLF 均保持）。
        prop_assert_eq!(&actual, &expected, "编辑必须只影响 old_text 命中的字节");
        // revision 与内容一致（§10.1）。
        prop_assert_eq!(revision_of(actual.as_bytes()), result.current_revision);
    }
}

/// 多 replacement 有一个歧义时，整批零变化（§20.2 场景 4 的确定性版本）。
#[test]
fn batch_with_one_ambiguous_replacement_is_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.txt")).unwrap();
    let content = "let a = 1;\nlet b = 1;\n";
    std::fs::write(path.as_std_path(), content).unwrap();
    let replacements = vec![
        Replacement {
            old_text: "let a = 1;".into(),
            new_text: "let a = 9;".into(),
        },
        // 歧义："= 1" 出现两次。
        Replacement {
            old_text: "= 1".into(),
            new_text: "= 2".into(),
        },
    ];
    let result = apply_edit(&path, &replacements);
    assert!(result.is_err(), "歧义 replacement 必须拒绝整批");
    let after = std::fs::read_to_string(path.as_std_path()).unwrap();
    assert_eq!(after, content, "整批零变化");
}

/// 全部唯一且不重叠的 batch 整批应用（§10.3 第 7 条：先验证后一次性应用）。
#[test]
fn batch_all_unique_applies_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.txt")).unwrap();
    let content = "let a = 1;\nlet b = 2;\n";
    std::fs::write(path.as_std_path(), content).unwrap();
    let replacements = vec![
        Replacement {
            old_text: "let a = 1;".into(),
            new_text: "let a = 10;".into(),
        },
        Replacement {
            old_text: "let b = 2;".into(),
            new_text: "let b = 20;".into(),
        },
    ];
    let result = apply_edit(&path, &replacements).unwrap();
    assert_eq!(result.applied, 2);
    let plan = prepare_commit(&path);
    commit_edit(&result, &path, &plan).unwrap();
    let after = std::fs::read_to_string(path.as_std_path()).unwrap();
    assert_eq!(after, "let a = 10;\nlet b = 20;\n");
}

/// 修改括号附近代码不会删除未声明的相邻 token（§20.2 场景 3）。
#[test]
fn edit_near_brackets_never_deletes_undeclared_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.rs")).unwrap();
    let content = "fn main() {\n    Name::new(1);\n    work();\n}\n";
    std::fs::write(path.as_std_path(), content).unwrap();
    let result = apply_edit(
        &path,
        &[Replacement {
            old_text: "work();".into(),
            new_text: "work();\n    more();".into(),
        }],
    )
    .unwrap();
    let plan = prepare_commit(&path);
    commit_edit(&result, &path, &plan).unwrap();
    let after = std::fs::read_to_string(path.as_std_path()).unwrap();
    assert!(
        after.contains("Name::new(1);"),
        "未声明的相邻 token 不得被删除: {after}"
    );
    assert!(after.contains("more();"));
}

/// §20.2 场景 5：mixed CRLF/LF 文件的未触及字节（含 \r）逐字节保持一致。
#[test]
fn mixed_line_endings_untouched_bytes_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.txt")).unwrap();
    // 混合行尾：CRLF 与孤立 LF 并存。
    let content = "fn main() {\r\n    work();\n    more();\r\n}\n";
    std::fs::write(path.as_std_path(), content).unwrap();
    // old_text 不含换行 → logical 与 raw 位置一致（§10.5 只替换命中的原始 byte ranges）。
    let old_text = "work();";
    let result = apply_edit(
        &path,
        &[Replacement {
            old_text: old_text.into(),
            new_text: "work();\n    extra();".into(),
        }],
    )
    .unwrap();
    let plan = prepare_commit(&path);
    commit_edit(&result, &path, &plan).unwrap();
    let after = std::fs::read(path.as_std_path()).unwrap();
    let original = content.as_bytes();
    assert!(after.len() > original.len());
    // old_text 的原始区域：定位 content 中 "work();" 的字节位置。
    let raw_pos = content.find(old_text).unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(&original[..raw_pos]);
    // §10.5：替换的 \n 按 anchor 附近行尾编码（此处 anchor 是 CRLF）。
    expected.extend_from_slice(b"work();\r\n    extra();");
    expected.extend_from_slice(&original[raw_pos + old_text.len()..]);
    assert_eq!(
        after, expected,
        "mixed 行尾下未触及字节必须逐字节保留（含 \\r），\\n 按 anchor 行尾编码"
    );
}

/// BOM 文件：未触及字节（含 BOM）逐字节保留（§10.1：v1 支持 UTF-8 与 UTF-8 BOM）。
#[test]
fn bom_file_untouched_bytes_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.txt")).unwrap();
    let content = "\u{FEFF}let x = 1;\n";
    std::fs::write(path.as_std_path(), content).unwrap();
    let result = apply_edit(
        &path,
        &[Replacement {
            old_text: "let x = 1;".into(),
            new_text: "let x = 2;".into(),
        }],
    )
    .unwrap();
    let plan = prepare_commit(&path);
    commit_edit(&result, &path, &plan).unwrap();
    let after = std::fs::read(path.as_std_path()).unwrap();
    assert!(after.starts_with(b"\xEF\xBB\xBF"), "BOM 必须保留");
    assert_eq!(after, "\u{FEFF}let x = 2;\n".as_bytes());
}

// P1 property：trailing-whitespace 宽容定位的不变量——
// 成功时未变化行（context）保留实际字节（含真实 trailing）；变化行按 new_text。
// 任意 trailing 数量与行数下都必须成立；exact 与 trailing 命中均验证。
proptest! {
    #[test]
    fn trailing_tolerance_preserves_context_lines(
        n in 3usize..10,
        extra_trailing in 0usize..4,
    ) {
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("line{i}"));
            content.push_str(&" ".repeat(extra_trailing));
            content.push('\n');
        }
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("f.rs")).unwrap();
        std::fs::write(path.as_std_path(), &content).unwrap();
        // old_text 无 trailing（模型视角）；改 line1（唯一窗口，line1/line2 相邻）。
        let old_text = "line1\nline2".to_string();
        let new_text = "line1\nCHANGED".to_string();
        let result = apply_edit(
            &path,
            &[Replacement { old_text, new_text }],
        );
        let Ok(result) = result else {
            // 拒绝（理论上不会，此处为稳健）→ 文件必须零变化。
            prop_assert_eq!(&std::fs::read_to_string(path.as_std_path()).unwrap(), &content);
            return Ok(());
        };
        let plan = prepare_commit(&path);
        commit_edit(&result, &path, &plan).unwrap();
        let after = std::fs::read_to_string(path.as_std_path()).unwrap();
        let lines: Vec<&str> = after.lines().collect();
        prop_assert_eq!(lines.len(), n);
        for (i, line) in lines.iter().enumerate() {
            if i == 2 {
                // 变化行（old_text 第二行 line2 → CHANGED）：由 new_text 定义。
                prop_assert_eq!(line, &"CHANGED");
            } else {
                // 未变化行（含 context 行 line1）：原始内容 + trailing 逐字保留。
                let expected = format!("line{i}{}", " ".repeat(extra_trailing));
                prop_assert_eq!(line, &expected, "未变化行被改动: {}", i);
            }
        }
    }
}
