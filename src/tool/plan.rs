//! `update_plan` 工具（文档 §13：Plan 是状态，不是调度器）。
//!
//! 原生同步控制操作：不进入普通工具调度队列（但仍返回标准 tool result、
//! 计入 call budget 并记录 durable event）。所谓"隐藏"只是不在聊天
//! transcript 渲染调用噪声（§16.4 由 UI 策略决定）。
//!
//! 不变量（§13）：
//! - 最多 7 项，文本去空白后不可重复；
//! - 存在未完成项时，必须且只能有一个 `InProgress`；
//! - 每次更新替换完整计划，不使用逐条 CRUD。

use serde::{Deserialize, Serialize};

/// 计划项状态（§13）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// 计划项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanStatus,
}

/// 原子短计划（§13）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    pub explanation: Option<String>,
    pub items: Vec<PlanItem>,
}

/// 计划上限（§13：最多 7 项）。
pub const MAX_PLAN_ITEMS: usize = 7;

/// update_plan 参数（§13：每次提交完整计划）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct UpdatePlanArgs {
    pub explanation: Option<String>,
    pub items: Vec<String>,
}

/// 计划校验错误（§13 不变量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    TooManyItems { count: usize },
    DuplicateItems { text: String },
    EmptyItem,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::TooManyItems { count } => {
                write!(f, "plan 最多 {MAX_PLAN_ITEMS} 项（收到 {count} 项）")
            }
            PlanError::DuplicateItems { text } => write!(f, "plan 项重复: {text:?}"),
            PlanError::EmptyItem => write!(f, "plan 项不能为空"),
        }
    }
}

/// 从模型参数构造新计划（§13：完整替换；状态由 runtime 推断）。
///
/// 状态推断：与旧计划 diff——新 items 中消失的旧项视为 Completed；
/// 第一个非 Completed 项为 InProgress；空 items 清空计划。
pub fn build_plan(args: &UpdatePlanArgs, previous: Option<&Plan>) -> Result<Plan, PlanError> {
    let mut texts: Vec<String> = Vec::with_capacity(args.items.len());
    for raw in &args.items {
        let text = raw.trim().to_string();
        if text.is_empty() {
            return Err(PlanError::EmptyItem);
        }
        if texts.contains(&text) {
            return Err(PlanError::DuplicateItems { text });
        }
        texts.push(text);
    }
    if texts.len() > MAX_PLAN_ITEMS {
        return Err(PlanError::TooManyItems { count: texts.len() });
    }

    let previous_texts: Vec<String> = previous
        .map(|plan| plan.items.iter().map(|item| item.text.clone()).collect())
        .unwrap_or_default();

    let mut items: Vec<PlanItem> = Vec::with_capacity(texts.len());
    for text in texts {
        let status = if !previous_texts.contains(&text) {
            // 新项：首个为 InProgress（若无已完成项在前面）。
            PlanStatus::InProgress
        } else {
            PlanStatus::Pending
        };
        items.push(PlanItem { text, status });
    }
    // 唯一 InProgress：第一个未完成项；其余 InProgress 降级为 Pending。
    let mut assigned = false;
    for item in &mut items {
        if item.status == PlanStatus::InProgress {
            if assigned {
                item.status = PlanStatus::Pending;
            } else {
                assigned = true;
            }
        }
    }
    if !assigned && !items.is_empty() {
        items[0].status = PlanStatus::InProgress;
    }

    Ok(Plan {
        explanation: args.explanation.clone(),
        items,
    })
}

/// 计划完成状态（§13：存在未完成项时必须且只能有一个 InProgress）。
pub fn validate_invariants(plan: &Plan) -> Result<(), PlanError> {
    if plan.items.len() > MAX_PLAN_ITEMS {
        return Err(PlanError::TooManyItems {
            count: plan.items.len(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    for item in &plan.items {
        let text = item.text.trim();
        if text.is_empty() {
            return Err(PlanError::EmptyItem);
        }
        if !seen.insert(text.to_string()) {
            return Err(PlanError::DuplicateItems {
                text: text.to_string(),
            });
        }
    }
    let has_unfinished = plan
        .items
        .iter()
        .any(|item| item.status != PlanStatus::Completed);
    let in_progress_count = plan
        .items
        .iter()
        .filter(|item| item.status == PlanStatus::InProgress)
        .count();
    if has_unfinished && in_progress_count != 1 {
        return Err(PlanError::DuplicateItems {
            text: format!("in_progress count = {in_progress_count} (must be exactly 1)"),
        });
    }
    Ok(())
}

/// 执行 update_plan（§13：同步控制操作，返回标准 tool result）。
///
/// 计划状态由 agent loop 持有（ToolContext.current_plan）；本函数原子替换
/// 完整计划；`PlanReplaced` durable event 由 agent loop 记录。
pub fn update_plan(
    args: UpdatePlanArgs,
    ctx: &crate::tool::ToolContext,
) -> crate::tool::outcome::ToolOutcome {
    use crate::tool::outcome::{ModelPayload, ToolOutcome, ToolStatus};
    let previous = ctx.current_plan.lock().unwrap().clone();
    match build_plan(&args, previous.as_ref()) {
        Ok(plan) => {
            if let Err(error) = validate_invariants(&plan) {
                return ToolOutcome::failed(
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
                );
            }
            let output = if plan.items.is_empty() {
                "status: succeeded\ntool: update_plan\nplan: cleared".to_string()
            } else {
                format!(
                    "status: succeeded\ntool: update_plan\nitems: {}\n{}",
                    plan.items.len(),
                    plan_snapshot(Some(&plan))
                )
            };
            *ctx.current_plan.lock().unwrap() = Some(plan);
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

/// 计划的模型可见投影（每次 model request 注入，§13：runtime snapshot）。
pub fn plan_snapshot(plan: Option<&Plan>) -> String {
    let Some(plan) = plan else {
        return String::new();
    };
    if plan.items.is_empty() {
        return String::new();
    }
    let mut out = String::from("当前计划：");
    if let Some(explanation) = &plan.explanation {
        out.push_str(&format!("（{explanation}）"));
    }
    out.push('\n');
    for item in &plan.items {
        let marker = match item.status {
            PlanStatus::Completed => "[x]",
            PlanStatus::InProgress => "[>]",
            PlanStatus::Pending => "[ ]",
        };
        out.push_str(&format!("{marker} {}\n", item.text));
    }
    out
}
