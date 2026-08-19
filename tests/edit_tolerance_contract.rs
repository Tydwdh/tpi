//! P1/P2：whitespace 宽容定位契约测试。
//!
//! 核心原则（§P1/P2）：
//! - relaxation 只发生在 **where**（定位），不传播到 **what**（重建）；
//! - match against normalized view, mutate original view——未变化行（context）
//!   保持实际文件字节（含真实 trailing whitespace），不因宽容定位写丢；
//! - tab/space 不做等价换算；relative indentation 破坏一律拒绝；
//! - Makefile 的 `\t` 是语法，uniform-indent 被禁用。

use camino::Utf8PathBuf;
use tpi::tool::edit::{
    EditError, MatchTier, Replacement, WhitespacePolicy, apply_edit, commit_edit, prepare_commit,
    revision_of, whitespace_policy_for,
};

fn edit_file(
    dir: &tempfile::TempDir,
    name: &str,
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Result<tpi::tool::edit::EditResult, EditError> {
    let path = Utf8PathBuf::from_path_buf(dir.path().join(name)).unwrap();
    std::fs::write(path.as_std_path(), content).unwrap();
    let revision = revision_of(content.as_bytes());
    let result = apply_edit(
        &path,
        &revision,
        &[Replacement {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }],
    )?;
    commit_edit(&result, &path, &prepare_commit(&path)).unwrap();
    Ok(result)
}

fn read_file(dir: &tempfile::TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).unwrap()
}

/// Tier 2：文件行有 trailing whitespace，模型 `old_text` 无 → 宽容定位成功；
/// 未变化行（context）的真实 trailing 必须保留，只替换真正变化的行。
#[test]
fn trailing_tolerance_keeps_context_line_trailing_whitespace() {
    let dir = tempfile::tempdir().unwrap();
    let content = "    a()   \n    old()   \n    b()   \n";
    // context 行 a()/b() 在 old_text 与 new_text 中不变，实际行尾空格必须保留；
    // 变化行 old()→new() 由 new_text 定义（new 行无 trailing → 无 trailing）。
    let result = edit_file(
        &dir,
        "f.rs",
        content,
        "    a()\n    old()\n    b()",
        "    a()\n    new()\n    b()",
    )
    .unwrap();
    assert_eq!(result.tier, MatchTier::TrailingInsensitive);
    assert_eq!(
        read_file(&dir, "f.rs"),
        "    a()   \n    new()\n    b()   \n",
        "context 行 trailing 保留；变化行按 new_text"
    );
}

/// Tier 2：歧义 → 拒绝，不自动替换（安全失败）。exact 多匹配报
/// MultipleMatches；trailing 多窗口命中报 NoMatch——两者都是安全拒绝。
#[test]
fn trailing_tolerance_ambiguous_rewrite_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let content = "x()   \nx()\n";
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.rs")).unwrap();
    std::fs::write(path.as_std_path(), content).unwrap();
    let revision = revision_of(content.as_bytes());
    let err = apply_edit(
        &path,
        &revision,
        &[Replacement {
            old_text: "x()".into(),
            new_text: "y()".into(),
        }],
    )
    .unwrap_err();
    assert!(
        matches!(err, EditError::NoMatch { .. })
            || matches!(err, EditError::MultipleMatches { .. }),
        "歧义必须安全拒绝: {err:?}"
    );
    // 文件零变化。
    assert_eq!(
        std::fs::read_to_string(path.as_std_path()).unwrap(),
        content
    );
}

/// Tier 3：uniform outer-indent——模型整体少 8 空格（坐标系平移）→ 成功，
/// `new_text` 按实际前缀重建缩进。
#[test]
fn uniform_indent_shifts_outer_coordinate_system() {
    let dir = tempfile::tempdir().unwrap();
    let content = "fn main() {\n        if x {\n            foo();\n        }\n}\n";
    let result = edit_file(
        &dir,
        "f.rs",
        content,
        "if x {\n    foo();\n}",
        "if x {\n    bar();\n}",
    )
    .unwrap();
    assert_eq!(result.tier, MatchTier::UniformOuterIndent);
    assert_eq!(
        read_file(&dir, "f.rs"),
        "fn main() {\n        if x {\n            bar();\n        }\n}\n",
        "变化行 bar() 按实际 8/12 空格前缀重建"
    );
}

/// Tier 3：relative indentation 破坏（Python 嵌套层级变化）→ 拒绝。
#[test]
fn uniform_indent_rejects_relative_indentation_change() {
    let dir = tempfile::tempdir().unwrap();
    let content = "def f():\n    if x:\n        foo()\n";
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.py")).unwrap();
    std::fs::write(path.as_std_path(), content).unwrap();
    let revision = revision_of(content.as_bytes());
    // 模型把嵌套层级写平（bar() 少 4 空格）→ 相对缩进破坏 → 拒绝。
    let err = apply_edit(
        &path,
        &revision,
        &[Replacement {
            old_text: "if x:\n    foo()".into(),
            new_text: "if x:\nbar()".into(),
        }],
    )
    .unwrap_err();
    assert!(matches!(err, EditError::NoMatch { .. }), "{err:?}");
    assert_eq!(
        std::fs::read_to_string(path.as_std_path()).unwrap(),
        content
    );
}

/// P2b：Makefile 的 tab 是语法——空格版 `old_text` 不宽容（NoMatch），
/// 精确 tab 版本正常。
#[test]
fn makefile_tab_indent_is_not_lenient() {
    let dir = tempfile::tempdir().unwrap();
    let content = "target:\n\tcommand\n";
    // policy 检查。
    let path = Utf8PathBuf::from_path_buf(dir.path().join("Makefile")).unwrap();
    assert_eq!(
        whitespace_policy_for(&path),
        WhitespacePolicy::TrailingInsensitive
    );
    // 空格版 → NoMatch。
    let err = edit_file(
        &dir,
        "Makefile",
        content,
        "target:\n    command",
        "target:\n    newcmd",
    )
    .unwrap_err();
    assert!(matches!(err, EditError::NoMatch { .. }), "{err:?}");
    // 精确 tab 版本 → 成功。
    let ok = edit_file(
        &dir,
        "Makefile",
        content,
        "target:\n\tcommand",
        "target:\n\tnewcmd",
    )
    .unwrap();
    assert_eq!(ok.tier, MatchTier::Exact);
    assert_eq!(read_file(&dir, "Makefile"), "target:\n\tnewcmd\n");
}

/// P1：宽容定位 + `未变化行保留——old_text` 带 trailing（模型复制了读到的
/// 行尾空白）时，因 `old_text` 精确存在于文件 → Exact 命中（同样正确）。
#[test]
fn trailing_tolerance_accepts_both_sides_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let content = "fn a() {   \n    work();\n}\n";
    let result = edit_file(
        &dir,
        "f.rs",
        content,
        "fn a() {   \n    work();",
        "fn a() {   \n    run();",
    )
    .unwrap();
    // old_text 精确存在于文件 → Exact（Tier1 优先）；结果一致。
    assert!(
        matches!(
            result.tier,
            MatchTier::Exact | MatchTier::TrailingInsensitive
        ),
        "{:?}",
        result.tier
    );
    assert_eq!(read_file(&dir, "f.rs"), "fn a() {   \n    run();\n}\n");
}

/// `P0b：no_match` 现在带结构化诊断（差异类型/行/相似度），模型免 read 即可
/// 知道差异在缩进还是文本。
#[test]
fn no_match_carries_structured_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("f.rs")).unwrap();
    let content = "fn main() {\n        if x {\n            foo();\n        }\n}\n";
    std::fs::write(path.as_std_path(), content).unwrap();
    let revision = revision_of(content.as_bytes());
    // 缩进差异：old_text 无 outer indent → NoMatch + Indentation 诊断。
    let err = apply_edit(
        &path,
        &revision,
        &[Replacement {
            old_text: "if y {\n    foo();\n}".into(),
            new_text: "if y {\n    bar();\n}".into(),
        }],
    )
    .unwrap_err();
    match err {
        EditError::NoMatch { diagnostic, .. } => {
            let d = diagnostic.expect("NoMatch 必须带结构化诊断");
            assert_eq!(d.kind, tpi::tool::edit::MismatchKind::Textual);
            assert!(d.similarity_bp > 0, "相似度应 >0: {d:?}");
            assert!(d.first_difference.is_some());
        }
        other => panic!("期望 NoMatch: {other:?}"),
    }
}
