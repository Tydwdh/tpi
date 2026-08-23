//! Goal 领域类型（对标 plan.rs：core 层纯数据契约）。
//!
//! 与 `plan` 正交：`plan` 是短原子 Todo（max 7 项，每转更新）；
//! `goal` 是跨轮 completion objective（durable + 自动续跑驱动）。

use serde::{Deserialize, Serialize};

/// Goal 持久 phase（参考 deepseek-harness GoalPhase + oh-my-pi GoalStatus 取最小闭环）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    Active,
    Paused,
    Complete,
}

impl std::fmt::Display for GoalPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Paused => write!(f, "paused"),
            Self::Complete => write!(f, "complete"),
        }
    }
}

/// Durable goal 快照（全量，与 Plan 同为 SessionEvent payload）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Goal {
    pub objective: String,
    pub phase: GoalPhase,
    /// 单调递增 revision（CAS 用；每次 GoalSet +1）。
    pub revision: u64,
    /// 已自动续跑轮数（driver 维护；无单独轮数概念时可作展示用）。
    pub rounds: u32,
    /// 总轮上限（默认 256，防无限）。
    pub max_rounds: u32,
}

impl Default for Goal {
    fn default() -> Self {
        Self {
            objective: String::new(),
            phase: GoalPhase::Active,
            revision: 1,
            rounds: 0,
            max_rounds: 256,
        }
    }
}

pub const MAX_GOAL_OBJECTIVE_BYTES: usize = 4_000;
pub const DEFAULT_MAX_GOAL_ROUNDS: u32 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalError {
    EmptyObjective,
    ObjectiveTooLong { bytes: usize },
    InvalidMaxRounds { value: u32 },
}

impl std::fmt::Display for GoalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyObjective => write!(f, "goal objective 不能为空"),
            Self::ObjectiveTooLong { bytes } => {
                write!(f, "goal objective 最多 {MAX_GOAL_OBJECTIVE_BYTES} 字节（收到 {bytes} 字节）")
            }
            Self::InvalidMaxRounds { value } => write!(f, "max_rounds 必须为正整数（收到 {value}）"),
        }
    }
}

impl std::error::Error for GoalError {}

/// 校验 objective（trim 后非空、字节上限）。
pub fn validate_objective(objective: &str) -> Result<String, GoalError> {
    let trimmed = objective.trim().to_string();
    if trimmed.is_empty() {
        return Err(GoalError::EmptyObjective);
    }
    if trimmed.len() > MAX_GOAL_OBJECTIVE_BYTES {
        return Err(GoalError::ObjectiveTooLong { bytes: trimmed.len() });
    }
    Ok(trimmed)
}

/// 创建或更新 goal（CAS：若 existing 存在则 revision+1）。
pub fn build_goal(objective: &str, existing: Option<&Goal>) -> Result<Goal, GoalError> {
    let objective = validate_objective(objective)?;
    let (revision, rounds, max_rounds, phase) = match existing {
        Some(g) => (g.revision + 1, g.rounds, g.max_rounds, GoalPhase::Active),
        None => (1, 0, DEFAULT_MAX_GOAL_ROUNDS, GoalPhase::Active),
    };
    Ok(Goal { objective, phase, revision, rounds, max_rounds })
}

/// 从已有 goal 派生新 phase（revision+1）。
pub fn transition_goal(goal: &Goal, phase: GoalPhase) -> Goal {
    let mut next = goal.clone();
    next.phase = phase;
    next.revision += 1;
    next
}

/// 模型可见的 goal 上下文块（build_context 注入用）。
pub fn goal_context(goal: Option<&Goal>) -> Option<String> {
    let goal = goal?;
    if goal.phase == GoalPhase::Complete {
        return None;
    }
    let status = match goal.phase {
        GoalPhase::Active => "active",
        GoalPhase::Paused => "paused",
        GoalPhase::Complete => "complete",
    };
    Some(format!(
        "<goal_context>\nGoal: {}\nStatus: {} (revision {}, rounds {}/{})\n\n\
         这是用户设定的完成目标（human objective），不是可自行缩小的子任务。完成前不要以“已完成”结束 run。\n\
         需要继续时保持目标为 active；真正达成后再调用 goal 工具标记完成（调用前必须举证：读取文件、运行检查等）。\n\
         </goal_context>",
        goal.objective, status, goal.revision, goal.rounds, goal.max_rounds
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_trims_and_rejects_empty() {
        assert_eq!(validate_objective("  hello  ").unwrap(), "hello");
        assert!(validate_objective("   ").is_err());
    }

    #[test]
    fn goal_context_none_when_complete_or_absent() {
        assert!(goal_context(None).is_none());
        let g = Goal { phase: GoalPhase::Complete, objective: "x".into(), ..Default::default() };
        assert!(goal_context(Some(&g)).is_none());
    }
}
