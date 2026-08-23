//! GoalController：agent loop 的外层包装，管理 goal 生命周期。
//!
//! 参考 Claude Code 的双模型架构（worker + evaluator）：
//! - AgentLoop 负责执行（model inference + tool calls）
//! - GoalController 负责评估（goal 是否达成）和编排（是否开下一个 turn）
//!
//! Goal 状态的 source of truth 是 SessionState（增量投影），
//! GoalController 只持有运行时快照和 evaluator。
//!
//! 当前实现：self-judge（同一模型判断）；后续可替换为独立 evaluator。

use tpi_session::SessionState;

/// Goal 的 turn 级评估结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalVerdict {
    /// 目标已达成（模型调了 goal complete，或 evaluator 判断完成）。
    Achieved,
    /// 目标未达成（需要继续下一个 turn）。
    Unmet { reason: String },
    /// 目标被阻塞（遇到不可恢复的错误/用户取消）。
    Blocked { reason: String },
    /// 没有活跃的 goal（不做任何续跑）。
    NoGoal,
}

/// Goal 完成评估器 trait。
///
/// 参考 Claude Code：用独立的小模型（Haiku）评估 completion condition。
/// 当前实现：self-judge（从 session 事件判断 goal 是否 complete）。
pub trait GoalEvaluator: Send + Sync {
    /// 评估 goal 是否达成。
    ///
    /// 在 turn 结束后调用（不是每 inference 后）。
    /// 从 SessionState 读取最新的 goal phase。
    fn evaluate(&self, session_state: &SessionState) -> GoalVerdict;
}

/// 基于 session 事件的自评估器。
///
/// 检查 SessionState.goal().phase 是否为 Complete。
/// 这是最简单的 evaluator——模型自己调 goal complete 触发事件，
/// turn 结束后 evaluator 看到 phase 变化。
pub struct SessionEventEvaluator;

impl GoalEvaluator for SessionEventEvaluator {
    fn evaluate(&self, session_state: &SessionState) -> GoalVerdict {
        match session_state.goal() {
            None => GoalVerdict::NoGoal,
            Some(goal) => match goal.phase {
                tpi_core::goal::GoalPhase::Complete => GoalVerdict::Achieved,
                tpi_core::goal::GoalPhase::Paused => GoalVerdict::Blocked {
                    reason: "goal paused by user".into(),
                },
                tpi_core::goal::GoalPhase::Active => {
                    if goal.rounds >= goal.max_rounds {
                        GoalVerdict::Blocked {
                            reason: format!("max rounds ({}) reached", goal.max_rounds),
                        }
                    } else {
                        GoalVerdict::Unmet {
                            reason: "goal still active".into(),
                        }
                    }
                }
            },
        }
    }
}

/// GoalController：管理 goal 生命周期和 turn 编排。
///
/// 使用方式：
/// ```text
/// let controller = GoalController::new(Box::new(SessionEventEvaluator));
/// let verdict = controller.evaluate(&session_state);
/// match verdict {
///     GoalVerdict::Unmet => { /* 开下一个 autonomous turn */ }
///     GoalVerdict::Achieved => { /* 停止 */ }
///     ...
/// }
/// ```
pub struct GoalController {
    evaluator: Box<dyn GoalEvaluator>,
}

impl GoalController {
    pub fn new(evaluator: Box<dyn GoalEvaluator>) -> Self {
        Self { evaluator }
    }

    /// 评估 goal 状态（O(1) 从 SessionState 读取）。
    pub fn evaluate(&self, session_state: &SessionState) -> GoalVerdict {
        self.evaluator.evaluate(session_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpi_core::goal::{GoalPhase, build_goal};

    fn make_state_with_goal(phase: GoalPhase, rounds: u32) -> SessionState {
        let mut state = SessionState::default();
        let mut g = build_goal("fix tests", None).unwrap();
        g.phase = phase;
        g.rounds = rounds;
        state.apply(&tpi_session::SessionEvent::GoalSet { goal: g });
        state
    }

    #[test]
    fn active_goal_unmet() {
        let state = make_state_with_goal(GoalPhase::Active, 0);
        let controller = GoalController::new(Box::new(SessionEventEvaluator));
        assert_eq!(
            controller.evaluate(&state),
            GoalVerdict::Unmet {
                reason: "goal still active".into()
            }
        );
    }

    #[test]
    fn complete_goal_achieved() {
        let state = make_state_with_goal(GoalPhase::Complete, 5);
        let controller = GoalController::new(Box::new(SessionEventEvaluator));
        assert_eq!(controller.evaluate(&state), GoalVerdict::Achieved);
    }

    #[test]
    fn paused_goal_blocked() {
        let state = make_state_with_goal(GoalPhase::Paused, 0);
        let controller = GoalController::new(Box::new(SessionEventEvaluator));
        assert!(matches!(
            controller.evaluate(&state),
            GoalVerdict::Blocked { .. }
        ));
    }

    #[test]
    fn no_goal_returns_no_goal() {
        let state = SessionState::default();
        let controller = GoalController::new(Box::new(SessionEventEvaluator));
        assert_eq!(controller.evaluate(&state), GoalVerdict::NoGoal);
    }

    #[test]
    fn max_rounds_reached_blocked() {
        let state = make_state_with_goal(GoalPhase::Active, 256);
        let controller = GoalController::new(Box::new(SessionEventEvaluator));
        assert!(matches!(
            controller.evaluate(&state),
            GoalVerdict::Blocked { .. }
        ));
    }
}
