//! 运行时会话所有权。
//!
//! [`SessionLog`] 是 durable fact source，模型 `history` 是它的运行时投影。
//! 两者必须一起创建、恢复、重置和推进；把它们作为两个独立变量交给 app
//! 会允许“session A + history B”或“清空 session 但遗留 history”这类非法状态。

use std::path::Path;

use camino::Utf8PathBuf;

use tpi_core::ids::{RunId, SessionId, ToolCallId};
use tpi_core::message::ChatMessage;

use super::projector::ConversationProjector;
use super::protocol::Plan;
use super::store::SessionStore;
use super::{SessionEvent, SessionLog, recovery, workspace_id_for};

/// 一个会话的 durable log 与当前模型上下文投影。
///
/// `None + empty history` 表示尚未提交第一条消息的新会话。除此之外，log 与
/// history 始终作为一个整体变化。
pub struct Conversation {
    log: Option<SessionLog>,
    /// 纯投影器（P2-03 facade：history/plan 经它重建，不直接持有）。
    projector: ConversationProjector,
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new()
    }
}

impl Conversation {
    pub fn new() -> Self {
        Self {
            log: None,
            projector: ConversationProjector::new(),
        }
    }

    /// 恢复 session，并在返回前把所有中断工具的合成终态持久化。
    ///
    /// 这样调用者拿到的 history 一定由最终 durable log 重建，不需要知道
    /// recovery event 的插入顺序，也不会在重复 resume 时再次合成结果。
    pub fn resume(
        sessions_root: &Path,
        workspace_root: &Utf8PathBuf,
        session_id: SessionId,
    ) -> Result<Self, String> {
        let path = sessions_root
            .join(workspace_id_for(workspace_root.as_std_path()))
            .join(format!("{session_id}.jsonl"));
        if !path.exists() {
            return Err(format!("session 不存在: {session_id}"));
        }

        // 先取得单写者锁，再检查 pending tool。否则另一个进程可能在 recover 与
        // open 之间提交 ToolCompleted，当前进程随后会错误追加第二个 Interrupted 终态。
        // P0-2：中间坏行会导致 open 整体失败——提示用户用 `tpi sessions repair` 修复。
        let mut log = SessionLog::open(sessions_root, workspace_root.as_std_path(), session_id)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    format!(
                        "打开 session 失败: {error}\n该 session 可能损坏（中间坏行）；\
                         用 `tpi sessions repair` 诊断并修复（会自动备份并隔离坏行）"
                    )
                } else {
                    format!("打开 session 失败: {error}")
                }
            })?;
        // 用已持锁的 log 快照做 recovery（避免无锁读文件的 TOCTOU：open 后到
        // recover 之间的窗口内另一进程提交 ToolCompleted 会被漏判，导致重复合成 Interrupted）。
        let events_with_seq = log
            .events_with_seq()
            .map_err(|error| format!("恢复 session 失败: {error}"))?;
        let events_only: Vec<SessionEvent> =
            events_with_seq.iter().map(|(_, e)| e.clone()).collect();
        let recovered = recovery::recover_from_events(&events_only)
            .map_err(|error| format!("恢复 session 失败: {error}"))?;
        for (call_id, _provider_id, outcome) in &recovered.interrupted {
            let call_id = uuid::Uuid::parse_str(call_id)
                .map(ToolCallId)
                .map_err(|_| format!("恢复 session 失败: 无效 tool call id {call_id}"))?;
            log.complete_tool(call_id, outcome)
                .map_err(|error| format!("持久化中断工具结果失败: {error}"))?;
        }
        if !recovered.interrupted.is_empty() {
            log.sync_data()
                .map_err(|error| format!("同步 session 失败: {error}"))?;
        }

        let events = log
            .events_with_seq()
            .map_err(|error| format!("读取事件失败: {error}"))?;
        let projector = ConversationProjector::rebuild(&events);
        Ok(Self {
            log: Some(log),
            projector,
        })
    }

    /// 第一条消息前延迟创建 session；重复调用无副作用。
    pub fn ensure_started(
        &mut self,
        sessions_root: &Path,
        workspace_root: &Utf8PathBuf,
    ) -> Result<(), String> {
        if self.log.is_none() {
            let log =
                SessionLog::create(sessions_root, workspace_root.as_std_path(), RunId::new_v7())
                    .map_err(|error| format!("创建 session 失败: {error}"))?;
            self.log = Some(log);
        }
        Ok(())
    }

    /// 开始一个没有 durable 状态和历史投影的新会话。
    pub fn reset(&mut self) {
        self.log = None;
        self.projector = ConversationProjector::new();
    }

    pub fn log(&self) -> Option<&SessionLog> {
        self.log.as_ref()
    }

    pub fn log_mut(&mut self) -> Option<&mut SessionLog> {
        self.log.as_mut()
    }

    pub fn history(&mut self) -> &[ChatMessage] {
        self.projector.history()
    }

    pub fn plan(&mut self) -> Option<&Plan> {
        self.projector.plan()
    }

    pub fn goal(&mut self) -> Option<tpi_core::goal::Goal> {
        self.projector.goal().cloned()
    }

    /// agent run 所需的两个一致视图。
    pub fn parts_for_run(&mut self) -> Result<(&mut SessionLog, &[ChatMessage]), String> {
        let Some(log) = self.log.as_mut() else {
            return Err("内部错误：conversation 尚未创建 session".into());
        };
        let history = self.projector.history();
        Ok((log, history))
    }

    /// `AgentOutcome.messages` 是完整 context，不是增量；只能整体接纳。
    pub fn accept_context(&mut self, complete_context: Vec<ChatMessage>) {
        // facade：把完整 context 作为新投影的历史（等价于 replace 语义）。
        // 事件缓冲无法从 context 反向重建，因此标记为"外部注入"（不 dirty，
        // 下次 refresh_from_log 会用 durable log 覆盖）。
        self.projector = ConversationProjector::from_history(complete_context, None);
    }

    /// run 在返回 `Err` 前可能已经提交了 User/Assistant/Tool 事件。
    /// 从 durable log 重建可避免 app 猜测“失败到哪一步”并手工拼接残缺 history。
    pub fn refresh_from_log(&mut self) -> Result<(), String> {
        let Some(log) = self.log.as_ref() else {
            self.projector = ConversationProjector::new();
            return Ok(());
        };
        let events = log
            .events_with_seq()
            .map_err(|error| format!("run 失败后读取事件失败: {error}"))?;
        self.projector = ConversationProjector::rebuild(&events);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssistantMessage, SessionEvent};
    use tpi_core::ids::ToolCallId;
    use tpi_core::message::ToolCall;
    use tpi_core::plan::{Plan, PlanItem, PlanStatus};

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().join("workspace")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        (dir, workspace)
    }

    #[test]
    fn reset_clears_log_and_history_as_one_state_transition() {
        let (dir, workspace) = workspace();
        let sessions = dir.path().join("sessions");
        let mut conversation = Conversation::new();
        conversation.ensure_started(&sessions, &workspace).unwrap();
        conversation.accept_context(vec![ChatMessage::User("hello".into())]);

        conversation.reset();

        assert!(conversation.log().is_none());
        assert!(conversation.history().is_empty());
    }

    #[test]
    fn failed_run_refresh_replays_every_committed_fact_not_only_user_text() {
        let (dir, workspace) = workspace();
        let sessions = dir.path().join("sessions");
        let mut conversation = Conversation::new();
        conversation.ensure_started(&sessions, &workspace).unwrap();
        let (log, _) = conversation.parts_for_run().unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "inspect".into(),
        })
        .unwrap();
        log.append_event(&SessionEvent::AssistantMessageCommitted {
            message: AssistantMessage {
                content: "partial committed answer".into(),
                tool_calls: Vec::new(),
            },
        })
        .unwrap();
        log.sync_data().unwrap();

        conversation.refresh_from_log().unwrap();

        assert_eq!(
            conversation.history(),
            &[
                ChatMessage::User("inspect".into()),
                ChatMessage::Assistant {
                    content: "partial committed answer".into(),
                    tool_calls: Vec::new(),
                }
            ]
        );
    }

    #[test]
    fn resume_persists_interrupted_tool_terminal_once() {
        let (dir, workspace) = workspace();
        let sessions = dir.path().join("sessions");
        let mut log =
            SessionLog::create(&sessions, workspace.as_std_path(), RunId::new_v7()).unwrap();
        let session_id = log.session_id();
        let call = ToolCall {
            call_id: ToolCallId::new_v7(),
            provider_id: "provider-call".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "state.txt"}).to_string(),
        };
        log.append_event(&SessionEvent::ToolRequested { call })
            .unwrap();
        log.sync_data().unwrap();
        drop(log);

        drop(Conversation::resume(&sessions, &workspace, session_id).unwrap());
        drop(Conversation::resume(&sessions, &workspace, session_id).unwrap());

        let path = sessions
            .join(workspace_id_for(workspace.as_std_path()))
            .join(format!("{session_id}.jsonl"));
        let completed = super::super::read_events(&path)
            .unwrap()
            .into_iter()
            .filter(|event| matches!(event, SessionEvent::ToolCompleted { .. }))
            .count();
        assert_eq!(completed, 1, "重复 resume 不得再次合成终态");
    }

    #[test]
    fn resume_and_refresh_restore_latest_plan_separately_from_chat() {
        let (dir, workspace) = workspace();
        let sessions = dir.path().join("sessions");
        let mut log =
            SessionLog::create(&sessions, workspace.as_std_path(), RunId::new_v7()).unwrap();
        let session_id = log.session_id();
        let plan = Plan {
            explanation: None,
            items: vec![PlanItem {
                text: "继续当前修复".into(),
                status: PlanStatus::InProgress,
            }],
        };
        log.append_event(&SessionEvent::PlanReplaced { plan: plan.clone() })
            .unwrap();
        log.sync_data().unwrap();
        drop(log);

        let mut conversation = Conversation::resume(&sessions, &workspace, session_id).unwrap();
        assert_eq!(conversation.plan(), Some(&plan));
        assert!(conversation.history().is_empty(), "plan 不应伪装成聊天消息");

        let replacement = Plan {
            explanation: Some("下一阶段".into()),
            items: vec![PlanItem {
                text: "跑完整验证".into(),
                status: PlanStatus::InProgress,
            }],
        };
        let (log, _) = conversation.parts_for_run().unwrap();
        log.append_event(&SessionEvent::PlanReplaced {
            plan: replacement.clone(),
        })
        .unwrap();
        log.sync_data().unwrap();
        conversation.refresh_from_log().unwrap();
        assert_eq!(conversation.plan(), Some(&replacement));
    }
}
