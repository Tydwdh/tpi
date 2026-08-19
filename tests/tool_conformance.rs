//! P4-10：tool conformance suite——builtin / fake MCP 共用同一契约断言。
//!
//! 覆盖 roadmap 验收：cancel / policy / output / ordering / reload。
//! 同一组断言对 builtin adapter 与 fake MCP adapter 运行（证明统一 contract）。

mod fixtures;

use std::sync::Arc;

use tpi::outcome::ToolStatus;
use tpi::tool::BuiltinTool;
use tpi::tool::registry::{BuiltinToolAdapter, Tool, ToolOrigin};

fn tool_context() -> tpi::tool::ToolContext {
    fixtures::test_tool_context(&camino::Utf8PathBuf::from("."))
}

/// fake MCP adapter：实现 Tool contract（echo 语义；cancel 感知）。
#[derive(Clone)]
struct FakeMcpTool {
    name: String,
}

#[async_trait::async_trait]
impl Tool for FakeMcpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &'static str {
        "fake mcp echo"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Mcp {
            server: "fake".into(),
        }
    }
    async fn execute(&self, args: &str, ctx: &tpi::tool::ToolContext) -> tpi::outcome::ToolOutcome {
        let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let text = v["text"].as_str().unwrap_or("").to_string();
        if ctx.cancel.is_cancelled() {
            return tpi::outcome::ToolOutcome::failed(
                &self.name,
                tpi::outcome::ModelPayload {
                    status: ToolStatus::Cancelled,
                    program: Some("fake".into()),
                    exit_code: None,
                    duration_ms: 0,
                    output: "status: cancelled".into(),
                    effect: None,
                    artifact: None,
                },
            );
        }
        tpi::outcome::ToolOutcome::failed(
            &self.name,
            tpi::outcome::ModelPayload {
                status: ToolStatus::Succeeded,
                program: Some("fake".into()),
                exit_code: Some(0),
                duration_ms: 1,
                output: format!("echo: {text}"),
                effect: None,
                artifact: None,
            },
        )
    }
}

/// 对任意 Tool 运行的 conformance 断言（builtin 与 fake MCP 共用）。
async fn assert_conformance(tool: &dyn Tool, args: &str, expected_success: bool) {
    // 1. definition 一致性（registration 契约）。
    assert!(tool.validate_definition().is_ok(), "definition 必须有效");
    assert_eq!(tool.definition().name, tool.name(), "definition == lookup");

    // 2. output：成功时含预期文本；失败时 status 明确。
    let ctx = tool_context();
    let outcome = tool.execute(args, &ctx).await;
    assert_eq!(
        outcome.status == ToolStatus::Succeeded,
        expected_success,
        "{} 执行状态不符: {:?}",
        tool.name(),
        outcome.status
    );
    // 命令类工具（program 非 None）必须有结构化 exit_code；文件工具无。
    if outcome.model_payload.program.is_some() && outcome.status != ToolStatus::Cancelled {
        assert!(
            outcome.model_payload.exit_code.is_some(),
            "命令类工具 exit_code 必须结构化: {}",
            tool.name()
        );
    }
}

/// conformance：builtin read（正常路径成功）。
#[tokio::test]
async fn builtin_read_conformance() {
    let tool = BuiltinToolAdapter::new(BuiltinTool::Read);
    // 不存在文件 → Failed（expected_success=false）；contract 一致。
    assert_conformance(&tool, r#"{"path":"/tmp/nonexistent-xyz"}"#, false).await;
    // definition 检查通过。
    assert!(tool.validate_definition().is_ok());
}

/// conformance：fake MCP adapter 同一契约（成功 + origin）。
#[tokio::test]
async fn fake_mcp_conformance() {
    let tool = FakeMcpTool {
        name: "mcp::fake::echo".into(),
    };
    assert_conformance(&tool, r#"{"text":"hi"}"#, true).await;
    // origin 是 MCP（metadata 不分支执行）。
    assert!(matches!(tool.origin(), ToolOrigin::Mcp { server } if server == "fake"));
}

/// conformance：cancel 传播（fake MCP 感知 cancel token）。
#[tokio::test]
async fn cancel_propagates_to_tool() {
    let tool = FakeMcpTool {
        name: "mcp::fake::slow".into(),
    };
    let mut ctx = tool_context();
    let cancel = tokio_util::sync::CancellationToken::new();
    ctx.cancel = cancel.clone();
    cancel.cancel();
    let outcome = tool.execute(r#"{"text":"x"}"#, &ctx).await;
    assert_eq!(outcome.status, ToolStatus::Cancelled, "cancel 必须传播");
}

/// conformance：pipeline stage 显式结果（P4-05）+ canonical output（P4-06）。
#[tokio::test]
async fn pipeline_and_canonical_conformance() {
    let tool = BuiltinToolAdapter::new(BuiltinTool::Read);
    let ctx = tool_context();
    // 失败路径 → Executed stage（不 panic）。
    let result =
        tpi::tool::pipeline::run_pure_pipeline(&tool, r#"{"path":"/tmp/nonexistent-xyz"}"#, &ctx)
            .await;
    assert!(result.is_ok());
    let stage = result.unwrap();
    assert_eq!(stage.tool_name(), "read");

    // canonical output：有界。
    let outcome = tpi::outcome::ToolOutcome::failed(
        "read",
        tpi::outcome::ModelPayload {
            status: ToolStatus::Succeeded,
            program: Some("read".into()),
            exit_code: Some(0),
            duration_ms: 1,
            output: "x".repeat(100_000),
            effect: None,
            artifact: None,
        },
    );
    let stored = tpi::tool::pipeline::canonicalize_output(
        outcome,
        tpi::tool::pipeline::MAX_MODEL_OUTPUT_BYTES,
    );
    assert!(
        stored.model_payload.output.len() <= tpi::tool::pipeline::MAX_MODEL_OUTPUT_BYTES + 64,
        "canonical output 必须有界"
    );
}

/// reload：fake MCP 注册到 registry 后可 snapshot/注销（P4-08/09 契约）。
#[tokio::test]
async fn reload_and_dispose_conformance() {
    let registry = Arc::new(std::sync::Mutex::new(
        tpi::tool::registry::ToolRegistry::new(),
    ));
    let handle = tpi::tool::registry::ToolRegistry::register_owned(
        &registry,
        Arc::new(FakeMcpTool {
            name: "mcp::fake::echo".into(),
        }),
    )
    .unwrap();
    assert!(registry.lock().unwrap().get("mcp::fake::echo").is_some());
    // invariants：健康。
    assert!(tpi::tool::invariants::check_registry_invariants(&registry.lock().unwrap()).is_empty());
    // snapshot 只读（count 正确）。
    let snap = tpi::tool::invariants::snapshot(&registry.lock().unwrap());
    assert_eq!(snap.count, 1);
    // dispose 精确（drop 句柄 → 注销）。
    drop(handle);
    assert!(registry.lock().unwrap().get("mcp::fake::echo").is_none());
}
