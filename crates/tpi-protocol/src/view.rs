//! 查询视图 DTO（web_desktop.md §二十六：Anti-Corruption Layer）。
//!
//! 前端只能看到这些投影视图，**永远看不到** `SessionLog` / `Conversation` /
//! `AgentOutcome` / provider 等内部 struct。

use serde::{Deserialize, Serialize};

use tpi_core::ids::SessionId;

/// session 对外状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// 空闲（可提交新消息）。
    Idle,
    /// 有 run 正在执行。
    Running,
    /// 挂起等待用户输入（request_input）。
    AwaitingInput,
}

/// 会话摘要视图（session 列表 / 会话卡片）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: SessionId,
    /// 首条用户消息的摘要（无则空串）。
    pub title: String,
    /// workspace 名（目录名；诊断/展示用）。
    pub workspace: String,
    pub status: SessionStatus,
    /// unix epoch 毫秒。
    pub created_at_ms: u64,
    /// unix epoch 毫秒（最后事件时间）。
    pub updated_at_ms: u64,
}

/// `request_input` 挂起问题的协议 DTO（与 capabilities 的
/// `RequestInputQuestion` 字段语义对齐，但独立定义，protocol 不依赖 capabilities）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOptionDto {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionDto {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<QuestionOptionDto>,
    #[serde(default)]
    pub multiple: bool,
    /// 是否允许自定义回答（默认 true；无选项时必须允许，否则无法回答）。
    #[serde(default = "default_true")]
    pub custom: bool,
}

fn default_true() -> bool {
    true
}

/// 会话历史中的单条消息（断线重连 / 页面刷新后重建 transcript 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
    Tool,
}

/// 会话历史消息快照（Anti-Corruption Layer：前端只看到 role/content，
/// 看不到内部 ChatMessage 结构）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageDto {
    pub role: MessageRoleDto,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_view_round_trips() {
        let view = SessionView {
            id: SessionId::new_v7(),
            title: "修复测试".into(),
            workspace: "tpi".into(),
            status: SessionStatus::Idle,
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"status\":\"idle\""));
        let back: SessionView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, view);
    }

    #[test]
    fn question_dto_tolerates_missing_fields() {
        let json = r#"{"question":"继续吗？"}"#;
        let q: QuestionDto = serde_json::from_str(json).unwrap();
        assert_eq!(q.question, "继续吗？");
        assert!(q.custom, "无选项时必须允许自定义");
        assert!(q.options.is_empty());
        assert!(!q.multiple);
    }

    #[test]
    fn chat_message_dto_round_trips() {
        let m = ChatMessageDto {
            role: MessageRoleDto::User,
            content: "修复测试".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        let back: ChatMessageDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, MessageRoleDto::User);
    }
}
