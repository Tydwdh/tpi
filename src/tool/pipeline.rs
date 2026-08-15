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
use crate::tool::outcome::{StoredToolOutcome, ToolOutcome};
use crate::tool::registry::Tool;

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
    if outcome.status == crate::tool::outcome::ToolStatus::Failed {
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
            call_id: crate::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            shell_path: None,
            snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::edit::SnapshotStore::new(4, 2),
            )),
            current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
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
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
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
