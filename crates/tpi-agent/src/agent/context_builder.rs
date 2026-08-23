//! ContextBuilder：结构化槽位，替换 flat message list。
//!
//! 参考 Claude Code / Codex 的做法：
//! - Static instructions：system prompt（可缓存）
//! - Turn overlay：goal/workspace/process（per-turn immutable snapshot）
//! - Dynamic overlay：reports/ephemeral（per-inference 变化）
//! - Transcript：conversation history（每 inference 增长）
//!
//! Goal 在 turn 内保持同一个 snapshot 渲染——不因 tool call 变化。

use crate::agent::turn_context::TurnContext;
use crate::provider::ChatMessage;

/// 结构化上下文各部分。
pub struct ContextParts<'a> {
    /// base system prompt（含 extra + skills；可缓存，turn 间不变）。
    pub base_prompt: &'a str,
    /// per-turn immutable snapshot（goal/workspace/process）。
    pub turn: &'a TurnContext<'a>,
    /// per-inference dynamic overlays（reports/ephemeral）。
    pub dynamic: DynamicContext<'a>,
    /// conversation history（User/Assistant/Tool；每 inference 增长）。
    pub transcript: &'a [ChatMessage],
}

/// per-inference 变化的 context 部分。
pub struct DynamicContext<'a> {
    /// 子代理报告（每 turn 可能更新）。
    pub pending_reports: Option<&'a str>,
    /// ephemeral system instructions（recovery/retry/final-turn）。
    pub ephemeral_system: Option<&'a str>,
}

impl<'a> ContextParts<'a> {
    /// 构造最终 request messages。
    ///
    /// 顺序：
    /// 1. System prompt（base + ephemeral 合并）
    /// 2. Workspace identity（turn snapshot）
    /// 3. Subagent reports（dynamic）
    /// 4. Transcript（conversation history）
    /// 5. Process snapshot（turn snapshot）
    /// 6. Goal context（turn snapshot）
    pub fn build(&self) -> Vec<ChatMessage> {
        let mut out = Vec::with_capacity(self.transcript.len() + 6);

        // 1. System prompt（base + ephemeral 合并）
        out.push(ChatMessage::System(build_system_prompt(
            self.base_prompt,
            self.dynamic.ephemeral_system,
        )));

        // 2. Workspace identity（turn snapshot）
        let ws = &self.turn.workspace;
        out.push(ChatMessage::System(format!(
            "[当前 workspace]\nWorkspace: {}\nShell cwd: {}\n\n（工作区状态由 harness 管理，无需模型自行执行 ssh/cd 确认。）",
            ws.id, ws.cwd,
        )));

        // 3. Subagent reports（dynamic overlay）
        if let Some(reports) = self.dynamic.pending_reports
            && !reports.is_empty()
        {
            out.push(ChatMessage::System(format!(
                "[子代理报告]（以下为后台调查的最新结果；用 `agent` 工具查看/等待更多详情）：\n{reports}"
            )));
        }

        // 4. Transcript（conversation history）
        out.extend_from_slice(self.transcript);

        // 5. Process snapshot（turn snapshot）
        if let Some(snapshot) = &self.turn.process_snapshot {
            out.push(ChatMessage::System(format!(
                "[Managed processes]\n{snapshot}\n\n（后台进程由 TPI 管理；需要结果时用 `process` wait/status，不要频繁轮询）"
            )));
        }

        // 6. Goal context（turn snapshot）
        if let Some(ctx) = self
            .turn
            .goal
            .and_then(|g| tpi_core::goal::goal_context(Some(g)))
        {
            out.push(ChatMessage::System(ctx));
        }

        out
    }
}

/// 合并 base prompt 和 ephemeral system instructions。
fn build_system_prompt(base: &str, ephemeral: Option<&str>) -> String {
    match ephemeral {
        Some(eph) => format!("{base}\n\n{eph}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::turn_context::{TurnContext, WorkspaceIdentity};
    use tpi_core::goal::build_goal;

    fn make_ws() -> WorkspaceIdentity {
        WorkspaceIdentity {
            id: "test-ws".into(),
            cwd: "/tmp".into(),
        }
    }

    #[test]
    fn build_includes_goal_context() {
        let g = build_goal("fix tests", None).unwrap();
        let tc = TurnContext::new(Some(&g), make_ws(), None);
        let parts = ContextParts {
            base_prompt: "base",
            turn: &tc,
            dynamic: DynamicContext {
                pending_reports: None,
                ephemeral_system: None,
            },
            transcript: &[],
        };
        let messages = parts.build();
        let has_goal = messages
            .iter()
            .any(|m| matches!(m, ChatMessage::System(text) if text.contains("fix tests")));
        assert!(has_goal, "goal context must be in messages");
    }

    #[test]
    fn build_excludes_goal_when_none() {
        let tc = TurnContext::new(None, make_ws(), None);
        let parts = ContextParts {
            base_prompt: "base",
            turn: &tc,
            dynamic: DynamicContext {
                pending_reports: None,
                ephemeral_system: None,
            },
            transcript: &[],
        };
        let messages = parts.build();
        let has_goal = messages
            .iter()
            .any(|m| matches!(m, ChatMessage::System(text) if text.contains("goal_context")));
        assert!(!has_goal, "no goal context when goal is None");
    }

    #[test]
    fn build_goal_is_same_snapshot_across_calls() {
        let g = build_goal("fix tests", None).unwrap();
        let tc = TurnContext::new(Some(&g), make_ws(), None);

        // 第一次 build
        let parts1 = ContextParts {
            base_prompt: "base",
            turn: &tc,
            dynamic: DynamicContext {
                pending_reports: None,
                ephemeral_system: None,
            },
            transcript: &[],
        };
        let msgs1 = parts1.build();
        let goal_text1 = msgs1
            .iter()
            .find_map(|m| match m {
                ChatMessage::System(text) if text.contains("goal_context") => Some(text.clone()),
                _ => None,
            })
            .unwrap();

        // 第二次 build（同一个 TurnContext）
        let parts2 = ContextParts {
            base_prompt: "base",
            turn: &tc,
            dynamic: DynamicContext {
                pending_reports: Some("new report"),
                ephemeral_system: None,
            },
            transcript: &[],
        };
        let msgs2 = parts2.build();
        let goal_text2 = msgs2
            .iter()
            .find_map(|m| match m {
                ChatMessage::System(text) if text.contains("goal_context") => Some(text.clone()),
                _ => None,
            })
            .unwrap();

        // goal 文本相同（同一 snapshot）
        assert_eq!(
            goal_text1, goal_text2,
            "goal must be same snapshot across inferences"
        );
    }

    #[test]
    fn build_ephemeral_merges_into_system_prompt() {
        let tc = TurnContext::new(None, make_ws(), None);
        let parts = ContextParts {
            base_prompt: "base prompt",
            turn: &tc,
            dynamic: DynamicContext {
                pending_reports: None,
                ephemeral_system: Some("ephemeral instruction"),
            },
            transcript: &[],
        };
        let messages = parts.build();
        let system_msg = match &messages[0] {
            ChatMessage::System(text) => text,
            _ => panic!("first message should be system"),
        };
        assert!(system_msg.contains("base prompt"));
        assert!(system_msg.contains("ephemeral instruction"));
    }
}
