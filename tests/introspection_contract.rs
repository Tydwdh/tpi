//! Runtime Introspection 契约测试（AGENTS.md §15 / P7）。
//!
//! `runtime_inspect` 是只读能力查询：报告工具目录（含 provider/origin）、
//! skills、workspace、后台进程——Runtime 是事实来源，Agent 不靠 system prompt 猜。

mod fixtures;

use camino::Utf8PathBuf;

#[tokio::test]
async fn runtime_inspect_reports_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = fixtures::test_tool_context(&workspace);

    let args = tpi::tool::ValidatedArgs::RuntimeInspect(tpi::tool::inspect::InspectArgs {});
    let outcome =
        tpi::tool::execute(tpi::tool::BuiltinTool::RuntimeInspect, args, &ctx, None).await;
    assert_eq!(outcome.status, tpi::outcome::ToolStatus::Succeeded);

    let out = &outcome.model_payload.output;
    // 工具目录（builtin 至少包含 bash；origin 标注 builtin）。
    assert!(out.contains("[tools]"), "{out}");
    assert!(out.contains("bash (builtin)"), "{out}");
    assert!(out.contains("runtime_inspect (builtin)"), "{out}");
    // workspace（local kind + identity 前缀）。
    assert!(out.contains("[workspace]"), "{out}");
    assert!(out.contains("kind: local"), "{out}");
    assert!(out.contains("identity: local:"), "{out}");
    // skills 与 processes 区块存在（内容可为空）。
    assert!(out.contains("[skills]"), "{out}");
    assert!(out.contains("[processes]"), "{out}");
    // 只读：不修改任何状态。
    assert!(out.contains("runtime introspection"), "{out}");
}

#[tokio::test]
async fn runtime_inspect_is_listed_as_builtin_tool() {
    // implemented_tools 包含 runtime_inspect；schema 生成成功（无参数）。
    let names: Vec<&str> = tpi::tool::implemented_tools()
        .iter()
        .map(tpi::tool::BuiltinTool::name)
        .collect();
    assert!(names.contains(&"runtime_inspect"), "{names:?}");
    assert!(names.contains(&"request_input"), "{names:?}");
}
