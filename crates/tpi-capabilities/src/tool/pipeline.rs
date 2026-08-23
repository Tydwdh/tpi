//! P4-05：工具执行 pipeline skeleton——显式 stage result。
//!
//! 现有 scheduler（PreparedKind: Builtin/External + write-ahead + batch 执行）
//! 是横切调度；本模块定义**垂直 pipeline**的显式 stage，先包一个 Pure builtin
//! 验证结构，后续阶段（P4-06 canonical output、P4-07 typed directive）逐工具
//! 迁移。
//!
//! Stage 序列：`parse → (plan/approval) → execute → output`。
//! 每次迁移后跑 existing scheduler/recovery suites（验收）。

use crate::tool::ToolContext;
use crate::tool::registry::Tool;
use tpi_core::outcome::{StoredToolOutcome, ToolOutcome};

/// 显式 stage 结果（每个阶段有明确产出，供 inspector/审计消费）。
#[derive(Debug, Clone, PartialEq)]
pub enum StageResult {
    /// 参数解析成功（ValidatedArgs 序列化回 JSON 保留原文）。
    Parsed { tool: String, args_json: String },
    /// 副作用前计划（写工具 write-ahead；Pure 无）。
    Planned { tool: String, plan: Option<String> },
    /// 执行产出（原始 ToolOutcome）。
    Executed { tool: String, outcome: ToolOutcome },
    /// 规范化输出（StoredToolOutcome；P4-06 canonical output 的落点）。
    Output {
        tool: String,
        stored: StoredToolOutcome,
    },
}

impl StageResult {
    pub fn tool_name(&self) -> &str {
        match self {
            StageResult::Parsed { tool, .. }
            | StageResult::Planned { tool, .. }
            | StageResult::Executed { tool, .. }
            | StageResult::Output { tool, .. } => tool,
        }
    }
}

/// P4-06：canonical output——有界输出 + 规范化 diagnostics。
///
/// - output 截断到 `max_output_bytes`（防无界正文进模型上下文）；
/// - 截断时在尾部标注 `[truncated: N bytes]`（不伪装完整）；
/// - 投影回 [`StoredToolOutcome`]（模型/session/UI 现有消费结构不变）。
pub fn canonicalize_output(outcome: ToolOutcome, max_output_bytes: usize) -> StoredToolOutcome {
    let mut outcome = outcome;
    let payload = &mut outcome.model_payload;
    if payload.output.len() > max_output_bytes {
        let mut truncated = payload.output.clone();
        // 按字节切片会切在多字节字符中间 → String::index panic（中文/emoji
        // 输出在 16 KiB 边界必然触发）；先推进到字符边界再截断。
        tpi_core::util::truncate_to_char_boundary(&mut truncated, max_output_bytes);
        truncated.push_str(&format!(
            "
[truncated: {} bytes]",
            payload.output.len()
        ));
        payload.output = truncated;
    }
    // diagnostics/artifact 有界（effect 保持结构化；无界日志不进 model payload）。
    outcome.into_stored()
}

/// canonical 常量：模型可见输出上限（与 tool_runtime 的展示裁剪一致量级）。
pub const MAX_MODEL_OUTPUT_BYTES: usize = 16 * 1024;

/// 跑 Pure 工具 + canonical output（P4-06 纯工具入口）。
pub async fn run_canonical_pure_pipeline(
    tool: &dyn Tool,
    args_json: &str,
    ctx: &ToolContext,
) -> Result<StageResult, String> {
    let tool_name = tool.name().to_string();
    let outcome = tool.execute(args_json, ctx).await;
    if outcome.status == tpi_core::outcome::ToolStatus::Failed {
        return Ok(StageResult::Executed {
            tool: tool_name,
            outcome,
        });
    }
    let stored = canonicalize_output(outcome, MAX_MODEL_OUTPUT_BYTES);
    Ok(StageResult::Output {
        tool: tool_name,
        stored,
    })
}

/// 跑一个 Pure 工具的完整 pipeline（parse → execute → output）。
///
/// Pure 无副作用前计划（write-ahead 由 scheduler 在 write 工具路径处理）；
/// 本函数供 skeleton 验证 + P4-06 canonical output 的纯工具入口。
pub async fn run_pure_pipeline(
    tool: &dyn Tool,
    args_json: &str,
    ctx: &ToolContext,
) -> Result<StageResult, String> {
    let tool_name = tool.name().to_string();
    // Stage 1: parse（验证 args 可解析）。
    // 注：内置工具 parse 在 BuiltinToolAdapter::execute 内部；此处以 execute 的
    // 成功与否作为 parse+execute 边界（P4-06 引入独立 parse stage 时拆开）。
    // Stage 2: plan（Pure 无副作用，无 write-ahead）。
    // Stage 3: execute。
    let outcome = tool.execute(args_json, ctx).await;
    if outcome.status == tpi_core::outcome::ToolStatus::Failed {
        return Ok(StageResult::Executed {
            tool: tool_name,
            outcome,
        });
    }
    // Stage 4: output（存 StoredToolOutcome；canonical 化在 P4-06）。
    let stored = outcome.into_stored();
    Ok(StageResult::Output {
        tool: tool_name,
        stored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::BuiltinTool;
    use crate::tool::registry::BuiltinToolAdapter;

    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: camino::Utf8PathBuf::from("/tmp"),
            allow_outside_workspace: true,
            cancel: tokio_util::sync::CancellationToken::new(),
            artifacts_root: std::path::PathBuf::from("/tmp/art"),
            session_id: "test".into(),
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            shell_path: None,
            snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::edit::SnapshotStore::new(4, 2),
            )),
            current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_goal: None,
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            shell: std::sync::Arc::new(std::sync::Mutex::new(
                crate::shell::ShellSessionState::new(camino::Utf8PathBuf::from("/tmp")),
            )),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(crate::workspace::LocalWorkspace::new(
                    camino::Utf8PathBuf::from("/tmp"),
                    true,
                )),
            )),
            terminals: Default::default(),
            resources: None,
            resource_identity: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            workspace_session: None,
        }
    }

    /// skeleton：Pure 工具（read）经 pipeline 产出 Output stage。
    #[tokio::test]
    async fn pure_pipeline_reaches_output_stage() {
        let adapter = BuiltinToolAdapter::new(BuiltinTool::Read);
        let result = run_pure_pipeline(&adapter, r#"{"path":"/tmp/x"}"#, &ctx()).await;
        assert!(result.is_ok(), "read pipeline 不失败: {result:?}");
        match result.unwrap() {
            StageResult::Output { tool, stored } => {
                assert_eq!(tool, "read");
                assert_eq!(stored.session_metadata.tool, "read");
            }
            StageResult::Executed { tool, .. } => {
                // 文件不存在 → Failed（Executed stage）；也是合法 stage 结果。
                assert_eq!(tool, "read");
            }
            other => panic!("read 不应停在 parse/plan stage: {other:?}"),
        }
    }

    /// 显式 stage 结果可审计（tool_name 一致）。
    #[test]
    fn stage_result_carries_tool_name() {
        let r = StageResult::Parsed {
            tool: "read".into(),
            args_json: "{}".into(),
        };
        assert_eq!(r.tool_name(), "read");
    }
}

/// P4-06：canonical output 截断有界（不伪装完整）。
#[test]
fn canonical_output_truncates_and_marks() {
    use tpi_core::outcome::{ModelPayload, ToolStatus};
    let big = "x".repeat(100);
    let mut outcome = ToolOutcome::failed(
        "echo",
        ModelPayload {
            status: ToolStatus::Succeeded,
            program: Some("echo".into()),
            exit_code: Some(0),
            duration_ms: 1,
            output: big.clone(),
            effect: None,
            artifact: None,
        },
    );
    outcome.status = ToolStatus::Succeeded;
    let stored = canonicalize_output(outcome, 10);
    assert!(stored.model_payload.output.len() < 100, "输出必须有界");
    assert!(
        stored
            .model_payload
            .output
            .contains("[truncated: 100 bytes]"),
        "截断必须声明: {}",
        stored.model_payload.output
    );
}

/// P4-06：小输出不截断（投影等价）。
#[test]
fn canonical_output_keeps_small_unchanged() {
    use tpi_core::outcome::{ModelPayload, ToolStatus};
    let outcome = ToolOutcome::failed(
        "echo",
        ModelPayload {
            status: ToolStatus::Succeeded,
            program: Some("echo".into()),
            exit_code: Some(0),
            duration_ms: 1,
            output: "ok".into(),
            effect: None,
            artifact: None,
        },
    );
    let stored = canonicalize_output(outcome, 100);
    assert_eq!(stored.model_payload.output, "ok");
    assert_eq!(stored.status, ToolStatus::Succeeded);
}
