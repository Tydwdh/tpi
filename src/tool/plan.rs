//! `update_plan` 工具：Plan 是短期状态，不是调度器。
//!
//! 每次调用都是一个完整、显式的快照。这样模型不必猜测省略某项究竟代表完成、
//! 取消还是遗忘；也避免把历史中不完整的计划带入下一轮。

use serde::{Deserialize, Serialize};

/// 计划项状态。序列化名称必须与工具 schema 和提示词一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[serde(alias = "Pending")]
    Pending,
    #[serde(alias = "InProgress")]
    InProgress,
    #[serde(alias = "Completed")]
    Completed,
    #[serde(alias = "Cancelled")]
    Cancelled,
    #[serde(alias = "Blocked")]
    Blocked,
}

impl PlanStatus {
    /// 是否仍是开放项（Pending/InProgress/Blocked）。
    /// 供 plan_snapshot（全部终态 → 空快照）与 TUI 侧边栏（全部终态 →
    /// Todo 自动清空）共用同一判定。
    pub(crate) fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress | Self::Blocked)
    }
}

/// 已持久化的计划项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanStatus,
}

/// `update_plan` 的计划项参数。状态是必填项，禁止由 diff 推断。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct PlanItemArg {
    pub text: String,
    pub status: PlanStatus,
}

/// 原子短计划：每次 update 都会完整替换它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    pub explanation: Option<String>,
    pub items: Vec<PlanItem>,
}

/// 一个快照中的总项数上限（包括已完成、取消和阻塞项）。
pub const MAX_PLAN_ITEMS: usize = 7;
pub const MAX_PLAN_ITEM_BYTES: usize = 500;
pub const MAX_PLAN_EXPLANATION_BYTES: usize = 2_000;
pub const MAX_PLAN_CONTEXT_ITEMS: usize = MAX_PLAN_ITEMS;
pub const MAX_PLAN_CONTEXT_TEXT_BYTES: usize = 240;

/// update_plan 参数：`items` 是完整显式快照；空数组清空计划。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct UpdatePlanArgs {
    pub explanation: Option<String>,
    pub items: Vec<PlanItemArg>,
}

/// 计划校验错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    TooManyItems { count: usize },
    DuplicateItems { text: String },
    EmptyItem,
    ItemTooLong { bytes: usize },
    ExplanationTooLong { bytes: usize },
    InProgressCount { count: usize },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyItems { count } => {
                write!(f, "plan 最多 {MAX_PLAN_ITEMS} 项（收到 {count} 项）")
            }
            Self::DuplicateItems { text } => write!(f, "plan 项重复: {text:?}"),
            Self::EmptyItem => write!(f, "plan 项不能为空"),
            Self::ItemTooLong { bytes } => write!(
                f,
                "plan 项最多 {MAX_PLAN_ITEM_BYTES} 字节（收到 {bytes} 字节）"
            ),
            Self::ExplanationTooLong { bytes } => write!(
                f,
                "plan explanation 最多 {MAX_PLAN_EXPLANATION_BYTES} 字节（收到 {bytes} 字节）"
            ),
            Self::InProgressCount { count } => {
                write!(f, "plan 最多只能有一个 in_progress（收到 {count} 个）")
            }
        }
    }
}

/// 从模型参数构造新计划。`previous` 保留在签名中以兼容调用点，但完整快照不读取它。
pub fn build_plan(args: &UpdatePlanArgs, _previous: Option<&Plan>) -> Result<Plan, PlanError> {
    let explanation = args
        .explanation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(explanation) = &explanation
        && explanation.len() > MAX_PLAN_EXPLANATION_BYTES
    {
        return Err(PlanError::ExplanationTooLong {
            bytes: explanation.len(),
        });
    }

    let items = args
        .items
        .iter()
        .map(|item| PlanItem {
            text: item.text.trim().to_string(),
            status: item.status,
        })
        .collect();
    let plan = Plan { explanation, items };
    validate_invariants(&plan)?;
    Ok(plan)
}

/// 计划不变量：总数受限、文本唯一，且至多一个当前执行项。
pub fn validate_invariants(plan: &Plan) -> Result<(), PlanError> {
    if plan.items.len() > MAX_PLAN_ITEMS {
        return Err(PlanError::TooManyItems {
            count: plan.items.len(),
        });
    }
    if let Some(explanation) = plan.explanation.as_deref()
        && explanation.len() > MAX_PLAN_EXPLANATION_BYTES
    {
        return Err(PlanError::ExplanationTooLong {
            bytes: explanation.len(),
        });
    }

    let mut seen = std::collections::HashSet::new();
    for item in &plan.items {
        if item.text.is_empty() {
            return Err(PlanError::EmptyItem);
        }
        if item.text.len() > MAX_PLAN_ITEM_BYTES {
            return Err(PlanError::ItemTooLong {
                bytes: item.text.len(),
            });
        }
        if !seen.insert(item.text.as_str()) {
            return Err(PlanError::DuplicateItems {
                text: item.text.clone(),
            });
        }
    }
    let in_progress_count = plan
        .items
        .iter()
        .filter(|item| item.status == PlanStatus::InProgress)
        .count();
    if in_progress_count > 1 {
        return Err(PlanError::InProgressCount {
            count: in_progress_count,
        });
    }
    Ok(())
}

/// 执行 update_plan（同步控制操作，返回标准 tool result）。
pub fn update_plan(
    args: UpdatePlanArgs,
    ctx: &crate::tool::ToolContext,
) -> crate::tool::outcome::ToolOutcome {
    use crate::tool::outcome::{ModelPayload, ToolOutcome, ToolStatus};
    let mut current = crate::util::lock_mutex(&ctx.current_plan, "current_plan");
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

/// 完整的模型可见计划快照。历史状态也必须可见，才能安全提交下一个完整快照。
pub fn plan_snapshot(plan: Option<&Plan>) -> String {
    let Some(plan) = plan else {
        return String::new();
    };
    if plan.items.is_empty() {
        return String::new();
    }
    // §用户诉求：全部项完成/取消后计划视为结束——快照为空，build_context
    // 不再注入尾部，模型每轮看不到已结束的计划（否则“全标完成后 plan 永不
    // 消失”）。plan 本体仍保留在 current_plan（UI 展示完成态、PlanReplaced
    // 持久化），只是不再进入模型上下文。
    if !plan.items.iter().any(|item| item.status.is_open()) {
        return String::new();
    }

    let mut out = String::from("当前计划（完整快照）：");
    if let Some(explanation) = &plan.explanation {
        out.push_str(&format!(
            "（{}）",
            truncate_context_text(explanation, MAX_PLAN_CONTEXT_TEXT_BYTES)
        ));
    }
    out.push('\n');
    for item in plan.items.iter().take(MAX_PLAN_CONTEXT_ITEMS) {
        let marker = match item.status {
            PlanStatus::InProgress => "[>]",
            PlanStatus::Pending => "[ ]",
            PlanStatus::Completed => "[x]",
            PlanStatus::Cancelled => "[-]",
            PlanStatus::Blocked => "[!]",
        };
        out.push_str(&format!(
            "{marker} {} ({})\n",
            truncate_context_text(&item.text, MAX_PLAN_CONTEXT_TEXT_BYTES),
            status_name(item.status)
        ));
    }
    let open = plan
        .items
        .iter()
        .filter(|item| item.status.is_open())
        .count();
    out.push_str(&format!(
        "进度：共 {} 项，待处理 {open} 项\n",
        plan.items.len()
    ));
    out
}

fn status_name(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Pending => "pending",
        PlanStatus::InProgress => "in_progress",
        PlanStatus::Completed => "completed",
        PlanStatus::Cancelled => "cancelled",
        PlanStatus::Blocked => "blocked",
    }
}

fn truncate_context_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = "…";
    let mut end = max_bytes.saturating_sub(marker.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, status: PlanStatus) -> PlanItemArg {
        PlanItemArg {
            text: text.into(),
            status,
        }
    }

    #[test]
    fn json_uses_snake_case_and_requires_explicit_status() {
        let args: UpdatePlanArgs = serde_json::from_value(serde_json::json!({
            "items": [{"text": "inspect", "status": "in_progress"}]
        }))
        .unwrap();
        assert_eq!(args.items[0].status, PlanStatus::InProgress);
        assert!(
            serde_json::from_value::<UpdatePlanArgs>(serde_json::json!({
                "items": [{"text": "inspect"}]
            }))
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(PlanStatus::InProgress).unwrap(),
            serde_json::json!("in_progress")
        );
    }

    #[test]
    fn full_snapshot_replaces_previous_without_inference() {
        let previous = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![item("old", PlanStatus::InProgress)],
            },
            None,
        )
        .unwrap();
        let next = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![item("new", PlanStatus::InProgress)],
            },
            Some(&previous),
        )
        .unwrap();
        assert_eq!(
            next.items,
            vec![PlanItem {
                text: "new".into(),
                status: PlanStatus::InProgress
            }]
        );
    }

    #[test]
    fn accepts_terminal_and_blocked_states() {
        let plan = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![
                    item("done", PlanStatus::Completed),
                    item("not needed", PlanStatus::Cancelled),
                    item("need answer", PlanStatus::Blocked),
                ],
            },
            None,
        )
        .unwrap();
        validate_invariants(&plan).unwrap();
        let snapshot = plan_snapshot(Some(&plan));
        assert!(snapshot.contains("done (completed)"));
        assert!(snapshot.contains("not needed (cancelled)"));
        assert!(snapshot.contains("need answer (blocked)"));
    }

    #[test]
    fn all_terminal_plan_yields_empty_snapshot() {
        let plan = Plan {
            explanation: Some("已结束".into()),
            items: vec![
                PlanItem {
                    text: "done".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    text: "skip".into(),
                    status: PlanStatus::Cancelled,
                },
            ],
        };
        assert!(
            plan_snapshot(Some(&plan)).is_empty(),
            "全部完成/取消后计划结束，不再注入上下文"
        );
    }

    #[test]
    fn rejects_invalid_snapshots() {
        let error = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: (0..8)
                    .map(|i| item(&format!("x{i}"), PlanStatus::Pending))
                    .collect(),
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(error, PlanError::TooManyItems { count: 8 }));
        let error = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![
                    item("a", PlanStatus::InProgress),
                    item("b", PlanStatus::InProgress),
                ],
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(error, PlanError::InProgressCount { count: 2 }));
    }

    #[test]
    fn context_snapshot_truncates_utf8_without_splitting_characters() {
        let plan = Plan {
            explanation: Some("说明".repeat(MAX_PLAN_CONTEXT_TEXT_BYTES)),
            items: vec![PlanItem {
                text: "当前任务".repeat(MAX_PLAN_CONTEXT_TEXT_BYTES),
                status: PlanStatus::InProgress,
            }],
        };
        let snapshot = plan_snapshot(Some(&plan));
        assert!(snapshot.contains('…'));
        assert!(snapshot.is_char_boundary(snapshot.len()));
    }
}
