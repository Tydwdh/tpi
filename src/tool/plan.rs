//! `update_plan` 工具：Plan 是短期状态，不是调度器。
//!
//! 原生同步控制操作：不进入普通工具调度队列（但仍返回标准 tool result、
//! 计入 call budget 并记录 durable event）。所谓"隐藏"只是不在聊天
//! transcript 渲染调用噪声（§16.4 由 UI 策略决定）。
//!
//! 不变量（§13）：
//! - 活跃项（Pending/InProgress）最多 7 项，文本去空白后不可重复；
//! - 存在未完成项时，必须且只能有一个 `InProgress`；
//! - 每次更新替换完整计划，不使用逐条 CRUD；
//! - 完成的项作为完成历史保留（最近 [`MAX_PLAN_HISTORY`] 条），不占活跃项
//!   名额——否则「整体替换满 7 项计划」和「长任务中途加新项」都会被
//!   7 项上限顶爆（§用户诉求：修复计划技能锁死）。

use serde::{Deserialize, Serialize};

/// 计划项状态（§13）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// 计划项（§13）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanStatus,
}

/// 计划项参数：显式状态或纯文本（§用户诉求：修复隐式状态——
/// 「全部完成」此前无法表达）。
///
/// - `"text"`（纯文本）：与旧计划 diff 推断状态（消失=Completed、
///   保留=旧状态、新项=InProgress 候选）；
/// - `{ "text": "...", "status": "completed" }`：显式指定状态
///   （Completed 是终态；缺省 status 等同纯文本推断）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PlanItemArg {
    Text(String),
    Full {
        text: String,
        #[serde(default)]
        status: Option<PlanStatus>,
    },
}

/// 原子短计划（§13）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    pub explanation: Option<String>,
    pub items: Vec<PlanItem>,
}

/// 计划上限（§13：活跃项最多 7 项）。
pub const MAX_PLAN_ITEMS: usize = 7;
/// 完成历史上限（Completed 项保留最近 N 条；超限丢弃最旧，防止长任务膨胀）。
pub const MAX_PLAN_HISTORY: usize = 7;
pub const MAX_PLAN_ITEM_BYTES: usize = 500;
pub const MAX_PLAN_EXPLANATION_BYTES: usize = 2_000;
/// 模型每轮真正需要关注的计划项：当前项 + 最近的两个下一步。
/// 完整计划仍保留在 session/TUI；这里只约束 runtime context，避免远期 Todo
/// 稀释当前任务，也避免频繁更新把大块动态文本塞进 prompt 尾部。
pub const MAX_PLAN_CONTEXT_ITEMS: usize = 3;
pub const MAX_PLAN_CONTEXT_TEXT_BYTES: usize = 240;

/// update_plan 参数（§13：每次提交完整计划）。
///
/// `items` 支持显式状态（`{"text": "...", "status": "completed"}`）
/// 或纯文本（`"text"`，按 diff 推断）——§用户诉求：完成项可显式标记，
/// 全部完成不必清空计划。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct UpdatePlanArgs {
    pub explanation: Option<String>,
    pub items: Vec<PlanItemArg>,
}

/// 计划校验错误（§13 不变量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    TooManyItems {
        count: usize,
    },
    DuplicateItems {
        text: String,
    },
    EmptyItem,
    ItemTooLong {
        bytes: usize,
    },
    ExplanationTooLong {
        bytes: usize,
    },
    /// Completed 完成历史超过 [`MAX_PLAN_HISTORY`]（build 时裁剪，仅防御性检查触发）。
    CompletedHistoryOverflow {
        count: usize,
    },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::TooManyItems { count } => {
                write!(f, "plan 最多 {MAX_PLAN_ITEMS} 项（收到 {count} 项）")
            }
            PlanError::DuplicateItems { text } => write!(f, "plan 项重复: {text:?}"),
            PlanError::EmptyItem => write!(f, "plan 项不能为空"),
            PlanError::ItemTooLong { bytes } => write!(
                f,
                "plan 项最多 {MAX_PLAN_ITEM_BYTES} 字节（收到 {bytes} 字节）"
            ),
            PlanError::ExplanationTooLong { bytes } => write!(
                f,
                "plan explanation 最多 {MAX_PLAN_EXPLANATION_BYTES} 字节（收到 {bytes} 字节）"
            ),
            PlanError::CompletedHistoryOverflow { count } => write!(
                f,
                "plan 完成历史最多保留 {MAX_PLAN_HISTORY} 条（收到 {count} 条）"
            ),
        }
    }
}

/// 从模型参数构造新计划（§13：完整替换；状态由 runtime 推断或显式指定）。
///
/// 状态解析（§用户诉求：显式状态修复隐式缺陷）：
/// - 纯文本项：与旧计划 diff——消失的旧项保留并标记 Completed；
///   重新提交的旧项保持旧状态；新项为 InProgress 候选；
/// - 显式 `{text, status}` 项：采用给定状态（Completed 是终态，不降级）；
/// - 空 items 清空计划。
pub fn build_plan(args: &UpdatePlanArgs, previous: Option<&Plan>) -> Result<Plan, PlanError> {
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
    // 解析为 (text, 显式状态) 列表：Text → None（推断）；Full → 显式。
    let mut parsed: Vec<(String, Option<PlanStatus>)> = Vec::with_capacity(args.items.len());
    for item in &args.items {
        let (text, explicit) = match item {
            PlanItemArg::Text(text) => (text.as_str(), None),
            PlanItemArg::Full { text, status } => (text.as_str(), *status),
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(PlanError::EmptyItem);
        }
        if text.len() > MAX_PLAN_ITEM_BYTES {
            return Err(PlanError::ItemTooLong { bytes: text.len() });
        }
        if parsed.iter().any(|(t, _)| *t == text) {
            return Err(PlanError::DuplicateItems { text });
        }
        parsed.push((text, explicit));
    }
    if parsed.len() > MAX_PLAN_ITEMS {
        return Err(PlanError::TooManyItems {
            count: parsed.len(),
        });
    }
    // 空提交：清空计划（Completed 也不保留）。
    if parsed.is_empty() {
        return Ok(Plan {
            explanation,
            items: Vec::new(),
        });
    }

    let previous_items: Vec<PlanItem> = previous.map(|plan| plan.items.clone()).unwrap_or_default();
    let previous_texts: Vec<&str> = previous_items.iter().map(|i| i.text.as_str()).collect();
    let submitted: Vec<&str> = parsed.iter().map(|(t, _)| t.as_str()).collect();

    let mut items: Vec<PlanItem> = Vec::new();
    // 1. 消失的旧项 → Completed（保持旧顺序排在前）。已 Completed 的旧项也保留
    //    为历史（不在本次提交中不丢弃——完成记录跨轮保留，由 MAX_PLAN_HISTORY
    //    裁剪，防止长任务膨胀）；未完成旧项消失视为完成。
    for old in &previous_items {
        if !submitted.contains(&old.text.as_str()) {
            items.push(PlanItem {
                text: old.text.clone(),
                status: PlanStatus::Completed,
            });
        }
    }
    // 2. 提交项：显式状态优先；否则重新提交的旧项保持旧状态；新项 → InProgress 候选。
    for (text, explicit) in parsed {
        let status = match explicit {
            Some(status) => status,
            None => {
                let old_idx = previous_texts.iter().position(|t| *t == text);
                match old_idx {
                    Some(i) => previous_items[i].status,
                    None => PlanStatus::InProgress,
                }
            }
        };
        items.push(PlanItem { text, status });
    }
    // 3. 唯一 InProgress：显式/推断后若有不完全项，必须且只能有一个 InProgress。
    //    多个 InProgress → 第一个保留，其余降 Pending；无 InProgress → 第一个
    //    Pending 升 InProgress（显式全部 Completed 时无未完成项，跳过）。
    let has_unfinished = items.iter().any(|i| i.status != PlanStatus::Completed);
    if has_unfinished {
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
        if !assigned
            && let Some(first) = items
                .iter_mut()
                .find(|item| item.status == PlanStatus::Pending)
        {
            first.status = PlanStatus::InProgress;
        }
    }
    // 4. 上限（§用户诉求：修复计划技能锁死）——活跃项（Pending/InProgress）
    //    受 MAX_PLAN_ITEMS 约束；Completed 作为完成历史保留但 ≤ MAX_PLAN_HISTORY，
    //    超限丢弃最旧。这样「整体替换满 7 项计划」「长任务中途加新项」都
    //    不会因消失旧项转 Completed 而顶爆 7 项上限。
    let active = items
        .iter()
        .filter(|i| i.status != PlanStatus::Completed)
        .count();
    if active > MAX_PLAN_ITEMS {
        return Err(PlanError::TooManyItems { count: active });
    }
    let mut completed = items
        .iter()
        .filter(|i| i.status == PlanStatus::Completed)
        .count();
    if completed > MAX_PLAN_HISTORY {
        let mut kept: Vec<PlanItem> = Vec::with_capacity(items.len());
        for item in items {
            if item.status == PlanStatus::Completed && completed > MAX_PLAN_HISTORY {
                completed -= 1;
                continue; // 丢弃最旧的完成记录（Completed 排在前，先到的先丢）。
            }
            kept.push(item);
        }
        items = kept;
    }

    Ok(Plan { explanation, items })
}

/// 计划完成状态（§13：存在未完成项时必须且只能有一个 InProgress）。
pub fn validate_invariants(plan: &Plan) -> Result<(), PlanError> {
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
        if text.len() > MAX_PLAN_ITEM_BYTES {
            return Err(PlanError::ItemTooLong { bytes: text.len() });
        }
    }
    if let Some(explanation) = plan.explanation.as_deref()
        && explanation.len() > MAX_PLAN_EXPLANATION_BYTES
    {
        return Err(PlanError::ExplanationTooLong {
            bytes: explanation.len(),
        });
    }
    // §用户诉求：活跃项 ≤ MAX_PLAN_ITEMS；Completed 历史 ≤ MAX_PLAN_HISTORY
    //（build_plan 已裁剪，这里做防御性不变量检查）。
    let active = plan
        .items
        .iter()
        .filter(|item| item.status != PlanStatus::Completed)
        .count();
    if active > MAX_PLAN_ITEMS {
        return Err(PlanError::TooManyItems { count: active });
    }
    let completed = plan.items.len() - active;
    if completed > MAX_PLAN_HISTORY {
        return Err(PlanError::CompletedHistoryOverflow { count: completed });
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
    let mut current = crate::util::lock_mutex(&ctx.current_plan, "current_plan");
    match build_plan(&args, current.as_ref()) {
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

/// 计划的模型可见投影（每次 model request 注入，§13：runtime snapshot）。
///
/// 这是“焦点投影”，不是完整历史：只包含当前项和最多两个下一步，完成项只给
/// 计数。完整 [`Plan`] 仍由 `PlanReplaced` 持久化并供 TUI 展示。这样长任务不会
/// 因早已完成/很远期的 Todo 分散注意力，计划频繁变化时也只改变 prompt 尾部的
/// 一小段，稳定历史仍可命中 provider prompt cache。
pub fn plan_snapshot(plan: Option<&Plan>) -> String {
    let Some(plan) = plan else {
        return String::new();
    };
    if plan.items.is_empty() {
        return String::new();
    }
    let mut out = String::from("当前焦点：");
    if let Some(explanation) = &plan.explanation {
        out.push_str(&format!(
            "（{}）",
            truncate_context_text(explanation, MAX_PLAN_CONTEXT_TEXT_BYTES)
        ));
    }
    out.push('\n');
    let mut visible = 0usize;
    for item in plan
        .items
        .iter()
        .filter(|item| item.status != PlanStatus::Completed)
        .take(MAX_PLAN_CONTEXT_ITEMS)
    {
        let marker = match item.status {
            PlanStatus::InProgress => "[>]",
            PlanStatus::Pending => "[ ]",
            PlanStatus::Completed => continue,
        };
        out.push_str(&format!(
            "{marker} {}\n",
            truncate_context_text(&item.text, MAX_PLAN_CONTEXT_TEXT_BYTES)
        ));
        visible += 1;
    }
    let completed = plan
        .items
        .iter()
        .filter(|item| item.status == PlanStatus::Completed)
        .count();
    let remaining = plan
        .items
        .iter()
        .filter(|item| item.status != PlanStatus::Completed)
        .count()
        .saturating_sub(visible);
    if completed > 0 || remaining > 0 {
        out.push_str(&format!(
            "进度：已完成 {completed}，另有 {remaining} 个后续项\n"
        ));
    }
    out
}

fn truncate_context_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = "…";
    let budget = max_bytes.saturating_sub(marker.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_items_become_completed() {
        // 初始计划：两项。
        let previous = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![PlanItemArg::Text("a".into()), PlanItemArg::Text("b".into())],
            },
            None,
        )
        .unwrap();
        assert_eq!(previous.items.len(), 2);
        assert_eq!(previous.items[0].status, PlanStatus::InProgress);
        assert_eq!(previous.items[1].status, PlanStatus::Pending);

        // 提交时移除 "a"（隐含完成）：§13 "新 items 中消失的旧项视为 Completed"。
        let next = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![PlanItemArg::Text("b".into())],
            },
            Some(&previous),
        )
        .unwrap();
        assert_eq!(
            next.items.len(),
            2,
            "消失的旧项必须保留并标记 Completed，而不是被丢弃"
        );
        assert_eq!(next.items[0].text, "a");
        assert_eq!(next.items[0].status, PlanStatus::Completed);
        assert_eq!(next.items[1].text, "b");
        assert_eq!(next.items[1].status, PlanStatus::InProgress);
    }

    #[test]
    fn plan_status_transitions_keep_single_in_progress() {
        let plan = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![
                    PlanItemArg::Text("a".into()),
                    PlanItemArg::Text("b".into()),
                    PlanItemArg::Text("c".into()),
                ],
            },
            None,
        )
        .unwrap();
        // 完成 a（移除），进行 b（保留）。
        let next = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![PlanItemArg::Text("b".into()), PlanItemArg::Text("c".into())],
            },
            Some(&plan),
        )
        .unwrap();
        assert_eq!(next.items[0].status, PlanStatus::Completed);
        assert_eq!(next.items[1].status, PlanStatus::InProgress);
        assert_eq!(next.items[2].status, PlanStatus::Pending);
        validate_invariants(&next).unwrap();
    }

    #[test]
    fn rejects_unbounded_plan_text() {
        let error = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![PlanItemArg::Text("x".repeat(MAX_PLAN_ITEM_BYTES + 1))],
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(error, PlanError::ItemTooLong { .. }));

        let error = build_plan(
            &UpdatePlanArgs {
                explanation: Some("x".repeat(MAX_PLAN_EXPLANATION_BYTES + 1)),
                items: vec![PlanItemArg::Text("step".into())],
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(error, PlanError::ExplanationTooLong { .. }));
    }

    /// §用户诉求：显式 `{text, status}` 让「全部完成」可表达——此前隐式推断
    /// 只有「清空计划」或「全部 InProgress」两个极端。
    #[test]
    fn explicit_completed_expresses_all_done() {
        let plan = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![
                    PlanItemArg::Full {
                        text: "a".into(),
                        status: Some(PlanStatus::Completed),
                    },
                    PlanItemArg::Full {
                        text: "b".into(),
                        status: Some(PlanStatus::Completed),
                    },
                ],
            },
            None,
        )
        .unwrap();
        assert_eq!(plan.items.len(), 2);
        assert!(plan.items.iter().all(|i| i.status == PlanStatus::Completed));
        // 全部完成 → 无不完全项 → validate_invariants 不再要求 InProgress。
        validate_invariants(&plan).unwrap();
    }

    /// §用户诉求：显式 InProgress 与 Pending 混用，仍归一化为唯一 InProgress。
    #[test]
    fn explicit_statuses_keep_single_in_progress() {
        let plan = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![
                    PlanItemArg::Full {
                        text: "a".into(),
                        status: Some(PlanStatus::InProgress),
                    },
                    PlanItemArg::Full {
                        text: "b".into(),
                        status: Some(PlanStatus::InProgress),
                    },
                ],
            },
            None,
        )
        .unwrap();
        // 多个显式 InProgress → 第一个保留，其余降 Pending。
        let in_progress = plan
            .items
            .iter()
            .filter(|i| i.status == PlanStatus::InProgress)
            .count();
        assert_eq!(in_progress, 1, "必须归一化为唯一 InProgress");
        assert_eq!(plan.items[0].status, PlanStatus::InProgress);
        assert_eq!(plan.items[1].status, PlanStatus::Pending);
        validate_invariants(&plan).unwrap();
    }

    /// §用户诉求：纯文本与显式混用——显式 Completed 保持，纯文本走推断。
    #[test]
    fn mixed_text_and_explicit_status() {
        let plan = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![
                    PlanItemArg::Full {
                        text: "done".into(),
                        status: Some(PlanStatus::Completed),
                    },
                    PlanItemArg::Text("next".into()),
                ],
            },
            None,
        )
        .unwrap();
        assert_eq!(plan.items[0].status, PlanStatus::Completed);
        assert_eq!(plan.items[1].status, PlanStatus::InProgress);
        validate_invariants(&plan).unwrap();
    }

    /// §用户诉求（修复计划技能锁死）：整体替换满 7 项未完成计划不得被拒。
    /// 消失的旧项转 Completed 作为历史保留，活跃项（新 7 项）≤ MAX_PLAN_ITEMS。
    #[test]
    fn replacing_full_plan_with_new_7_items_succeeds() {
        let previous = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: (0..7)
                    .map(|i| PlanItemArg::Text(format!("old{i}")))
                    .collect(),
            },
            None,
        )
        .unwrap();
        assert_eq!(previous.items.len(), 7);
        let next = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: (0..7)
                    .map(|i| PlanItemArg::Text(format!("new{i}")))
                    .collect(),
            },
            Some(&previous),
        )
        .unwrap();
        // 7 条完成历史 + 7 条活跃，全部保留（历史 ≤ MAX_PLAN_HISTORY）。
        assert_eq!(next.items.len(), 14);
        let completed = next
            .items
            .iter()
            .filter(|i| i.status == PlanStatus::Completed)
            .count();
        let active = next.items.len() - completed;
        assert_eq!(completed, 7);
        assert_eq!(active, 7);
        validate_invariants(&next).unwrap();
    }

    /// §用户诉求（修复计划技能锁死）：长任务累积的完成历史超限时裁剪最旧，
    /// 不锁死后续 update_plan。
    #[test]
    fn completed_history_is_pruned_to_cap() {
        // 逐步完成：每轮把当前 InProgress 移除，累计 >MAX_PLAN_HISTORY 条历史。
        let mut plan = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: vec![
                    PlanItemArg::Text("s0".into()),
                    PlanItemArg::Text("keep".into()),
                ],
            },
            None,
        )
        .unwrap();
        for i in 1..=MAX_PLAN_HISTORY + 2 {
            plan = build_plan(
                &UpdatePlanArgs {
                    explanation: None,
                    items: vec![
                        PlanItemArg::Text("keep".into()),
                        PlanItemArg::Text(format!("s{i}")),
                    ],
                },
                Some(&plan),
            )
            .unwrap();
            validate_invariants(&plan).unwrap();
        }
        let completed = plan
            .items
            .iter()
            .filter(|i| i.status == PlanStatus::Completed)
            .count();
        assert_eq!(completed, MAX_PLAN_HISTORY, "完成历史必须裁剪到上限");
        // 最旧的完成记录（s0）被丢弃，最新的（s9）保留。
        assert!(!plan.items.iter().any(|i| i.text == "s0"));
        assert!(plan.items.iter().any(|i| i.text == "keep"));
    }

    /// §用户诉求：活跃项仍严格 ≤ MAX_PLAN_ITEMS（Completed 历史不占名额，
    /// 但新提交的 8 个活跃项依旧被拒）。
    #[test]
    fn active_items_still_capped_at_seven() {
        let error = build_plan(
            &UpdatePlanArgs {
                explanation: None,
                items: (0..8).map(|i| PlanItemArg::Text(format!("x{i}"))).collect(),
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(error, PlanError::TooManyItems { count: 8 }));
    }

    #[test]
    fn context_snapshot_keeps_only_current_and_two_next_items() {
        let plan = Plan {
            explanation: Some("聚焦当前阶段".into()),
            items: vec![
                PlanItem {
                    text: "已经完成".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    text: "当前工作".into(),
                    status: PlanStatus::InProgress,
                },
                PlanItem {
                    text: "下一步一".into(),
                    status: PlanStatus::Pending,
                },
                PlanItem {
                    text: "下一步二".into(),
                    status: PlanStatus::Pending,
                },
                PlanItem {
                    text: "遥远步骤".into(),
                    status: PlanStatus::Pending,
                },
            ],
        };

        let snapshot = plan_snapshot(Some(&plan));

        assert!(snapshot.contains("当前工作"));
        assert!(snapshot.contains("下一步一"));
        assert!(snapshot.contains("下一步二"));
        assert!(!snapshot.contains("已经完成"), "完成历史不应抢占模型注意力");
        assert!(!snapshot.contains("遥远步骤"), "远期项只保留计数");
        assert!(snapshot.contains("已完成 1，另有 1 个后续项"));
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
