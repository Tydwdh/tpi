//! `update_plan` 工具执行（P7 拆分：依赖 ToolContext 的执行留在 tool 层；
//! Plan 纯数据在 core 层 `tpi_core::plan`）。

use crate::tool::ToolContext;
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};
use tpi_core::plan::{UpdatePlanArgs, build_plan};

/// 执行 update_plan（同步控制操作，返回标准 tool result）。
pub fn update_plan(args: UpdatePlanArgs, ctx: &ToolContext) -> ToolOutcome {
    let mut current = tpi_core::util::lock_mutex(&ctx.current_plan, "current_plan");
    match build_plan(&args, current.as_ref()) {
        Ok(plan) => {
            let output = if plan.items.is_empty() {
                "status: succeeded\ntool: update_plan\nplan: cleared".to_string()
            } else {
                format!(
                    "status: succeeded\ntool: update_plan\nitems: {}\nplan: updated",
                    plan.items.len()
                )
            };
            *current = Some(plan);
            ToolOutcome::succeeded("update_plan", output)
        }
        Err(error) => ToolOutcome::failed(
            "update_plan",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!(
                    "status: rejected\ntool: update_plan\nerror: invalid_plan\n\n{error}"
                ),
                effect: None,
                artifact: None,
            },
        ),
    }
}
