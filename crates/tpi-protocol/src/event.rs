//! 统一 RuntimeEvent（web_desktop.md §五/§六）。
//!
//! UI 状态必须由 `Initial Snapshot + RuntimeEvent stream` 构造，**不通过读取
//! Agent 内部对象推断**。事件携带稳定身份（session_id / run_id / seq / timestamp
//! 见 [`EventEnvelope`](crate::EventEnvelope)），不依赖"上一条事件是谁"的隐式顺序。
//!
//! 事件来源：`tpi-agent` 的 `LiveEvent`（语义事实）在 runtime 层转写为协议事件，
//! 并补充 runtime 级生命周期事件（SessionCreated / RunStarted / RunCompleted /
//! InputRequested / MutationRecorded …）。

use serde::{Deserialize, Serialize};

use tpi_core::ids::{RequestId, RunId, SessionId, ToolCallId};

use crate::view::QuestionDto;

/// 流式增量类型（assistant 文本 / reasoning）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// 工具终态状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    Success,
    Failed,
    Cancelled,
    Skipped,
}

/// 运行级事件消息：`RuntimeEvent` 里出现的"事件负载"。
///
/// 这里把"消息"（聊天内容）与"状态"（运行/工具/进程）统一为同一枚举，
/// 前端按事件类型分支渲染。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    // ---- runtime 生命周期 ----
    /// 会话创建成功（CreateSession 的响应语义事件）。
    SessionCreated { session: crate::view::SessionView },
    /// 会话列表（ListSessions 的响应）。
    SessionList {
        sessions: Vec<crate::view::SessionView>,
    },
    /// 会话恢复成功（ResumeSession 的响应）。
    SessionResumed { session: crate::view::SessionView },
    /// 会话历史快照（ResumeSession 后跟随；前端据此重建 transcript）。
    SessionHistory {
        session_id: SessionId,
        messages: Vec<crate::view::ChatMessageDto>,
    },
    /// 会话状态变化（Idle → Running → AwaitingInput）。
    SessionStatusChanged {
        session_id: SessionId,
        status: crate::view::SessionStatus,
    },

    // ---- run 生命周期 ----
    RunStarted {
        session_id: SessionId,
        run_id: RunId,
    },
    /// run 正常结束（含挂起 / 取消 / 停止等所有 `CompletionReason`）。
    RunCompleted {
        session_id: SessionId,
        run_id: RunId,
        reason: CompletionReasonDto,
        /// 最终 assistant 文本（reason=Stop 时有内容）。
        assistant_text: String,
    },
    RunFailed {
        session_id: SessionId,
        run_id: RunId,
        error: crate::error::AppError,
    },

    // ---- 消息流 ----
    /// 用户消息已持久化（submit 后的第一件事）。
    UserMessageAdded {
        session_id: SessionId,
        content: String,
    },
    AssistantDelta {
        session_id: SessionId,
        run_id: RunId,
        request_id: RequestId,
        kind: DeltaKind,
        text: String,
    },

    // ---- 工具执行 ----
    ToolStarted {
        session_id: SessionId,
        run_id: RunId,
        call_id: ToolCallId,
        name: String,
        /// 模型发出的原始参数 JSON 字符串（前端 GenericToolCard 展示）。
        arguments: String,
    },
    ToolCompleted {
        session_id: SessionId,
        run_id: RunId,
        call_id: ToolCallId,
        name: String,
        status: ToolState,
        duration_ms: u64,
        exit_code: Option<i32>,
        /// 有界输出（模型/用户可见内容）。
        output: String,
        /// edit/write 的 unified diff。
        diff: Option<String>,
    },
    ToolOutputDelta {
        session_id: SessionId,
        run_id: RunId,
        call_id: ToolCallId,
        stream: u8,
        text: String,
    },

    // ---- request_input 挂起 ----
    InputRequested {
        session_id: SessionId,
        run_id: RunId,
        request_id: RequestId,
        /// 渲染后的多行文本（编号 + header + 选项）。
        text: String,
        questions: Vec<QuestionDto>,
    },
    /// 用户回答已接受（挂起已解除，run 继续）。
    InputAnswered {
        session_id: SessionId,
        request_id: RequestId,
    },
    /// 挂起被拒绝（Esc / 关闭 dialog）。
    InputRejected {
        session_id: SessionId,
        request_id: RequestId,
    },

    // ---- 派生投影 ----
    /// `update_plan` 提交后的计划状态。
    PlanUpdated {
        session_id: SessionId,
        plan: serde_json::Value,
    },
    /// 上下文占用（每次 provider 请求前）。
    ContextUsage {
        session_id: SessionId,
        projected: u64,
        usable: u64,
    },
    /// provider usage（缓存命中实时展示）。
    UsageUpdated {
        session_id: SessionId,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
    },
    /// 接近 wall-time 预算。
    BudgetWarning { session_id: SessionId },
    /// 流中断后自动续写。
    StreamRecovering { session_id: SessionId, attempt: u32 },
    /// partial tool-call 后 model turn 重新生成。
    TurnRestarting { session_id: SessionId, attempt: u32 },
    /// 手动 /compact 反馈。
    CompactionNotice {
        session_id: SessionId,
        message: String,
    },
    /// 子代理调查完成（child 有独立 session 身份；parent 可见结构化 report）。
    SubagentReported {
        child_session: SessionId,
        summary: String,
        evidence: Vec<String>,
    },

    // ---- 文件变更（Mutation Journal）----
    MutationRecorded {
        session_id: SessionId,
        /// 变更摘要（"edit file.rs"）。
        summary: String,
    },
    UndoCompleted {
        session_id: SessionId,
        summary: String,
    },
    RedoCompleted {
        session_id: SessionId,
        summary: String,
    },
}

/// run 结束原因（协议 DTO：与 `tpi_session::CompletionReason` 语义对齐，独立定义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReasonDto {
    Stop,
    Cancelled,
    Error,
    ProviderInterrupted,
    ProviderUnavailable,
    ContextOverflow,
    MaxTurns,
    MaxToolCalls,
    WallTimeExceeded,
    AwaitingUserInput,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips() {
        let ev = RuntimeEvent::ToolCompleted {
            session_id: SessionId::new_v7(),
            run_id: RunId::new_v7(),
            call_id: ToolCallId::new_v7(),
            name: "read".into(),
            status: ToolState::Success,
            duration_ms: 3,
            exit_code: None,
            output: "fn main() {}".into(),
            diff: None,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "tool_completed");
        assert_eq!(json["status"], "success");
        let back: RuntimeEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn completion_reason_round_trips() {
        let r = CompletionReasonDto::AwaitingUserInput;
        let json = serde_json::to_value(r).unwrap();
        assert_eq!(json, "awaiting_user_input");
        let back: CompletionReasonDto = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
    }
}
