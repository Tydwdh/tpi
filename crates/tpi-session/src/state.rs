//! SessionState：增量投影（O(1) 读取 session 派生状态）。
//!
//! 传统做法：每次读 goal/plan 都从 JSONL 重放全部事件（O(n)）。
//! 本模块在 `SessionLog::commit` 时增量更新，提供 O(1) 读取。
//!
//! session 重建（`--resume` / `--continue`）时，从 event log 重放一次初始化；
//! 之后增量维护，不再重复 replay。

use tpi_core::goal::Goal;
use tpi_core::plan::Plan;

use crate::protocol::SessionEvent;

/// 增量投影：从事件流实时维护的 session 派生状态。
///
/// 读取路径 O(1)，写入路径 O(1)（每个事件匹配 1-2 个字段）。
/// 不维护 conversation messages（那仍是 replay 投影，因为历史增长有界且 compaction 介入）。
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    goal: Option<Goal>,
    plan: Option<Plan>,
}

impl SessionState {
    /// 从事件序列重建（session 重建时用一次；之后增量维护）。
    pub fn from_events(events: &[(u64, SessionEvent)]) -> Self {
        let mut state = Self::default();
        for (_seq, event) in events {
            state.apply(event);
        }
        state
    }

    /// 增量应用一个事件（O(1)）。
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::GoalSet { goal } => {
                self.goal = Some(goal.clone());
            }
            SessionEvent::GoalCleared => {
                self.goal = None;
            }
            SessionEvent::PlanReplaced { plan } => {
                self.plan = Some(plan.clone());
            }
            _ => { /* 其他事件不改变 goal/plan 投影 */ }
        }
    }

    /// 读取当前 goal（O(1)）。
    pub fn goal(&self) -> Option<&Goal> {
        self.goal.as_ref()
    }

    /// 读取当前 plan（O(1)）。
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ModelRef, RunLimits};
    use tpi_core::goal::build_goal;

    fn make_goal(objective: &str) -> Goal {
        build_goal(objective, None).unwrap()
    }

    #[test]
    fn from_events_replays_goal_set_and_clear() {
        let g = make_goal("fix tests");
        let events = vec![
            (1u64, SessionEvent::GoalSet { goal: g }),
            (2, SessionEvent::GoalCleared),
        ];
        let state = SessionState::from_events(&events);
        assert!(state.goal().is_none(), "GoalCleared should clear goal");
    }

    #[test]
    fn from_events_preserves_last_goal_set() {
        let g1 = make_goal("first");
        let g2 = make_goal("second");
        let events = vec![
            (1, SessionEvent::GoalSet { goal: g1 }),
            (2, SessionEvent::GoalSet { goal: g2 }),
        ];
        let state = SessionState::from_events(&events);
        assert_eq!(state.goal().unwrap().objective, "second");
    }

    #[test]
    fn apply_increments_goal() {
        let mut state = SessionState::default();
        let g = make_goal("test");
        state.apply(&SessionEvent::GoalSet { goal: g });
        assert_eq!(state.goal().unwrap().objective, "test");

        state.apply(&SessionEvent::GoalCleared);
        assert!(state.goal().is_none());
    }

    #[test]
    fn plan_updated_incrementally() {
        let mut state = SessionState::default();
        let plan = tpi_core::plan::Plan {
            explanation: None,
            items: vec![tpi_core::plan::PlanItem {
                text: "step1".into(),
                status: tpi_core::plan::PlanStatus::Pending,
            }],
        };
        state.apply(&SessionEvent::PlanReplaced { plan });
        assert!(state.plan().is_some());
    }

    #[test]
    fn other_events_dont_affect_goal() {
        let mut state = SessionState::default();
        let g = make_goal("persistent");
        state.apply(&SessionEvent::GoalSet { goal: g });

        state.apply(&SessionEvent::RunStarted {
            model: ModelRef {
                name: "test".into(),
                provider: "openai".into(),
            },
            limits: RunLimits {
                max_turns: 10,
                max_tool_calls: 50,
            },
        });
        assert!(state.goal().is_some(), "RunStarted should not affect goal");
    }

    #[test]
    fn empty_events_gives_default() {
        let state = SessionState::from_events(&[]);
        assert!(state.goal().is_none());
        assert!(state.plan().is_none());
    }
}
