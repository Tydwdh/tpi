//! 统一 ClientCommand（web_desktop.md §四）。
//!
//! **所有**修改 Runtime 状态的动作必须经过统一 Application Boundary——
//! 前端（TUI / Web / Desktop）绝不能直接调用 `AgentLoop::run()` /
//! `ToolExecutor::execute()` / `SessionStore::append()` / `Provider::request()`。
//!
//! 每个 command 携带 `request_id`（client 生成）：服务器 `CommandAck` 用它
//! 关联响应；后续 `InputRequested` 等需要异步回填的请求用它做相关性和去重。

use serde::{Deserialize, Serialize};

use tpi_core::ids::{RequestId, SessionId};

use crate::error::AppError;

/// 客户端命令：前端唯一修改入口。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    /// 创建新会话（当前 workspace）。返回 `SessionCreated`。
    CreateSession {
        /// 可选：会话标题（首条消息摘要生成前的人为标题；留空则用首条消息）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// 列出当前 workspace 的可恢复会话。返回 `SessionList`。
    ListSessions,
    /// 恢复指定 session（完整 id 或唯一前缀）。返回 `SessionResumed`。
    ResumeSession { session_id: String },
    /// 提交一条用户消息（开始一次 run）。run 中提交同 session 返回 `Busy`。
    SubmitMessage {
        session_id: SessionId,
        content: String,
    },
    /// 取消当前 run（幂等；无 run 时 no-op 成功）。
    CancelRun { session_id: SessionId },
    /// 重试上次 run（复用历史，不重复 UserSubmitted）。
    RetryRun { session_id: SessionId },
    /// 回答 `request_input` 挂起（重复回答：第一个成功，其余 `InputAlreadyAnswered`）。
    AnswerInput {
        session_id: SessionId,
        request_id: RequestId,
        answer: String,
    },
    /// 撤销最近一次文件变更（Mutation Journal）。
    Undo {
        session_id: SessionId,
        #[serde(default)]
        all: bool,
        #[serde(default)]
        force: bool,
    },
    /// 重做已撤销的文件变更。
    Redo {
        session_id: SessionId,
        #[serde(default)]
        all: bool,
        #[serde(default)]
        force: bool,
    },
    /// 请求优雅关闭 Runtime（Server 停止 / Desktop 退出）。
    Shutdown,
}

impl ClientCommand {
    /// 该命令关联的 session（无则 None）。
    pub fn session_id(&self) -> Option<SessionId> {
        match self {
            ClientCommand::CreateSession { .. }
            | ClientCommand::ListSessions
            | ClientCommand::Shutdown => None,
            ClientCommand::ResumeSession { .. } => None,
            ClientCommand::SubmitMessage { session_id, .. }
            | ClientCommand::CancelRun { session_id }
            | ClientCommand::RetryRun { session_id }
            | ClientCommand::AnswerInput { session_id, .. }
            | ClientCommand::Undo { session_id, .. }
            | ClientCommand::Redo { session_id, .. } => Some(*session_id),
        }
    }
}

/// 命令被接受与否。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AckStatus {
    /// 命令已接受并排入执行队列（异步副作用随后经 RuntimeEvent 可见）。
    Accepted,
    /// 命令被同步拒绝（非法 / 状态不允许）。
    Rejected(AppError),
}

/// 命令确认（web_desktop.md §十：服务器响应 `{"type":"ack","request_id":...}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAck {
    pub request_id: RequestId,
    #[serde(flatten)]
    pub status: AckStatus,
}

impl CommandAck {
    pub fn accepted(request_id: RequestId) -> Self {
        Self {
            request_id,
            status: AckStatus::Accepted,
        }
    }

    pub fn rejected(request_id: RequestId, error: AppError) -> Self {
        Self {
            request_id,
            status: AckStatus::Rejected(error),
        }
    }

    pub fn is_accepted(&self) -> bool {
        matches!(self.status, AckStatus::Accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trips_with_tags() {
        let cmd = ClientCommand::SubmitMessage {
            session_id: SessionId::new_v7(),
            content: "修复测试".into(),
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["type"], "submit_message");
        assert_eq!(json["content"], "修复测试");
        let back: ClientCommand = serde_json::from_value(json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn ack_round_trips() {
        let ack = CommandAck::accepted(RequestId::new_v7());
        let json = serde_json::to_value(&ack).unwrap();
        assert_eq!(json["status"], "accepted");
        let back: CommandAck = serde_json::from_value(json).unwrap();
        assert!(back.is_accepted());

        let rejected = CommandAck::rejected(
            RequestId::new_v7(),
            AppError::new(crate::ErrorCode::SessionNotFound, "missing"),
        );
        let json = serde_json::to_value(&rejected).unwrap();
        assert_eq!(json["status"], "rejected");
        assert_eq!(json["code"], "session_not_found");
        let back: CommandAck = serde_json::from_value(json).unwrap();
        assert!(!back.is_accepted());
    }

    #[test]
    fn unknown_command_variant_is_rejected() {
        let bad = r#"{"type":"no_such_command"}"#;
        assert!(serde_json::from_str::<ClientCommand>(bad).is_err());
    }
}
