//! TurnContext：per-turn immutable snapshot。
//!
//! Turn 开始时构造一次，turn 内所有 inference 共享同一个 snapshot。
//! Goal 在 turn 内保持稳定（不因 tool call 变化）；
//! GoalController 在 turn 结束后评估是否达成，再决定是否开下一个 turn。

use tpi_core::goal::Goal;

/// 一次 turn 的不可变快照。
///
/// 在 `'run_loop` 循环顶部（turn += 1 之后）构造一次。
/// 随后所有 inference 的 ContextBuilder 引用同一个 TurnContext。
/// Goal 在 turn 内保持稳定（不因 tool call 变化）；
/// GoalController 在 turn 结束后从 SessionState 读取最新值评估。
#[derive(Debug, Clone)]
pub struct TurnContext<'a> {
    /// goal snapshot：从 SessionState O(1) 读取后传入。
    /// turn 内不变——即使模型调了 goal complete，此 snapshot 仍是 turn 开始时的值。
    pub goal: Option<&'a Goal>,
    /// workspace identity（路径 + cwd）。
    pub workspace: WorkspaceIdentity,
    /// managed process 快照（active + 近期状态变化）。
    pub process_snapshot: Option<String>,
}

/// workspace 身份信息（turn 级 snapshot）。
#[derive(Debug, Clone)]
pub struct WorkspaceIdentity {
    pub id: String,
    pub cwd: String,
}

impl<'a> TurnContext<'a> {
    /// 从 goal snapshot 和 workspace/process 快照构造。
    pub fn new(
        goal: Option<&'a Goal>,
        workspace: WorkspaceIdentity,
        process_snapshot: Option<String>,
    ) -> Self {
        Self {
            goal,
            workspace,
            process_snapshot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpi_core::goal::build_goal;

    #[test]
    fn turn_context_captures_goal_snapshot() {
        let g = build_goal("fix tests", None).unwrap();
        let ws = WorkspaceIdentity {
            id: "test-ws".into(),
            cwd: "/tmp".into(),
        };
        let tc = TurnContext::new(Some(&g), ws, None);

        assert!(tc.goal.is_some());
        assert_eq!(tc.goal.unwrap().objective, "fix tests");
    }

    #[test]
    fn turn_context_shares_same_goal_snapshot() {
        let g = build_goal("fix tests", None).unwrap();
        let ws = WorkspaceIdentity {
            id: "test-ws".into(),
            cwd: "/tmp".into(),
        };
        let tc = TurnContext::new(Some(&g), ws, None);

        // goal 在 turn 内保持不变
        let goal1 = tc.goal.unwrap().clone();
        let goal2 = tc.goal.unwrap().clone();
        assert_eq!(goal1.objective, goal2.objective);
        assert_eq!(goal1.revision, goal2.revision);
    }

    #[test]
    fn turn_context_none_when_no_goal() {
        let ws = WorkspaceIdentity {
            id: "test-ws".into(),
            cwd: "/tmp".into(),
        };
        let tc = TurnContext::new(None, ws, None);
        assert!(tc.goal.is_none());
    }
}
