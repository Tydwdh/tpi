//! goal 工具（轻量版：对齐 oh-my-pi `goal` tool 语义，借 deepseek-harness 的 phase 分层）。
//!
//! 模型可见：`goal` single op 字段
//! - `get`: 返回当前 goal（若有）
//! - `complete`: 标记当前 goal 为完成（仅在真正达成后）
//! - `drop`: 丢弃当前 goal
//!
//! 创建/修改由用户侧 `/goal` 负责；模型不直接 create/edit objective，避免把小任务误判为 goal。

use crate::tool::ToolContext;
use tpi_core::goal::{GoalPhase, transition_goal};
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
pub struct GoalArgs {
    /// 操作：get | complete | drop
    pub op: String,
}

pub fn goal(args: GoalArgs, ctx: &ToolContext) -> ToolOutcome {
    let op = args.op.trim().to_lowercase();
    let current = ctx
        .current_goal
        .as_ref()
        .map(|g| tpi_core::util::lock_mutex(g, "current_goal").clone())
        .unwrap_or(None);
    let _plan_guard = tpi_core::util::lock_mutex(&ctx.current_plan, "current_plan");
    match op.as_str() {
        "get" => {
            if let Some(g) = current {
                let payload = serde_json::json!({
                    "goal": {
                        "objective": g.objective,
                        "phase": g.phase.to_string(),
                        "revision": g.revision,
                        "rounds": g.rounds,
                        "max_rounds": g.max_rounds,
                    }
                });
                ToolOutcome::succeeded(
                    "goal",
                    format!(
                        "status: succeeded\ntool: goal\nop: get\n\n{}",
                        serde_json::to_string_pretty(&payload).unwrap()
                    ),
                )
            } else {
                ToolOutcome::succeeded(
                    "goal",
                    "status: succeeded\ntool: goal\nop: get\n\ngoal: null\n(no active goal)"
                        .to_string(),
                )
            }
        }
        "complete" => {
            let Some(g) = current else {
                return ToolOutcome::failed(
                    "goal",
                    ModelPayload {
                        status: ToolStatus::Rejected,
                        program: None,
                        exit_code: None,
                        duration_ms: 0,
                        output:
                            "status: rejected\ntool: goal\nerror: no_goal\n\n当前没有可完成的 goal"
                                .into(),
                        effect: None,
                        artifact: None,
                    },
                );
            };
            if g.phase == GoalPhase::Complete {
                return ToolOutcome::failed("goal", ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: "status: rejected\ntool: goal\nerror: already_complete\n\n该 goal 已完成".into(),
                    effect: None,
                    artifact: None,
                });
            }
            let next = transition_goal(&g, GoalPhase::Complete);
            if let Some(slot) = ctx.current_goal.as_ref() {
                *tpi_core::util::lock_mutex(slot, "current_goal") = Some(next.clone());
            }
            let payload = serde_json::json!({ "goal": { "objective": next.objective, "phase": "complete", "revision": next.revision } });
            ToolOutcome::succeeded(
                "goal",
                format!(
                    "status: succeeded\ntool: goal\nop: complete\n\n{}",
                    serde_json::to_string_pretty(&payload).unwrap()
                ),
            )
        }
        "drop" => {
            let had = current.is_some();
            if let Some(slot) = ctx.current_goal.as_ref() {
                *tpi_core::util::lock_mutex(slot, "current_goal") = None;
            }
            if had {
                ToolOutcome::succeeded(
                    "goal",
                    "status: succeeded\ntool: goal\nop: drop\n\ngoal cleared".to_string(),
                )
            } else {
                ToolOutcome::succeeded(
                    "goal",
                    "status: succeeded\ntool: goal\nop: drop\n\nno goal to drop".to_string(),
                )
            }
        }
        _ => ToolOutcome::failed(
            "goal",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!(
                    "status: rejected\ntool: goal\nerror: invalid_op\n\nop 必须是 get | complete | drop（收到 {op}）"
                ),
                effect: None,
                artifact: None,
            },
        ),
    }
}
