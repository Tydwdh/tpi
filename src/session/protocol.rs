//! Session durable protocol（P2-01 拆分）：durable domain event、envelope、
//! schema version 与 serde wire type。
//!
//! [`SessionEvent`] 是 durable 事件的唯一集合（领域 API）；[`Envelope`] 是
//! 稳定 wire（schema/seq/event_id/timestamp + EventBody）。wire 格式是
//! 长期兼容面：**P2-01 拆分不改任何字段/序列化**（golden hash 证明）。

use crate::ids::{EventId, RequestId, RunId, SessionId, ToolCallId};
use crate::message::ToolCall;
use crate::outcome::StoredToolOutcome;
use serde::{Deserialize, Serialize};

/// 模型引用（§7.2 模型角色；M1 只有 primary）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub name: String,
    pub provider: String,
}

/// 单次 run 的执行预算（§12.4 的 Run budgets 在 M4 完整化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimits {
    pub max_turns: u32,
    pub max_tool_calls: u32,
}

/// 已提交的 assistant message（词汇审计 §3.3：即目标词汇的 Step 级输出——
/// 一次 provider response 必须原子表达；即使 `content` 为空（纯 tool-call 轮）
/// 也必须持久化，否则 resume 重建会缺失 assistant 载体、ToolRequested 挂错位置）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: String,
    /// 该 turn 发起的工具调用（与 ToolRequested 同一数据源）。
    /// 旧 session 文件无此字段：`#[serde(default)]` 兼容读取。
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// 原子短计划（§13）。
pub use crate::plan::Plan;

/// 事件区间，用于 compaction 覆盖范围（§15.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRange {
    pub start: EventId,
    pub end: EventId,
}

/// Compaction 摘要（§15.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactSummary {
    pub text: String,
}

/// run 结束原因（§6.2 明确的完成/失败分类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionReason {
    Stop,
    MaxTurns,
    /// 工具调用预算超限（P1-2：与 Error 区分，UI 可明确提示）。
    MaxToolCalls,
    Cancelled,
    /// §13（AGENTS.md）：模型请求用户输入后 run 挂起——等待输入**不等于**完成。
    /// 用户回答以 `UserInputReceived` 记录，后续 run 继续；事件对完整保留。
    AwaitingUserInput,
    /// 压缩与 prune 后上下文仍超出模型窗口（P1-4：不再发起必然失败的请求）。
    ContextOverflow,
    /// §16：wall-clock 预算到期被 watchdog 自动取消（不是用户取消）。
    WallTimeExceeded,
    /// provider 连接不可用（未收到任何语义事件；pre-stream 重试预算耗尽）。
    ProviderUnavailable,
    /// 流中途断联且已收到部分文本（已记录 `AssistantAttemptInterrupted`）。
    ProviderInterrupted,
    /// 长度限制、内容过滤或协议错误。
    Error,
}

/// token 用量（provider 返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 缓存命中（prompt cache read）的输入 token（§16.2：缓存命中/花费展示）。
    #[serde(default)]
    pub cache_read_tokens: u64,
}

/// 写工具执行前的 recovery metadata（§10.7/§14.2 write-ahead）。
///
/// 在产生任何外部副作用前持久化；崩溃恢复据此判定 `effect`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryMetadata {
    pub tool: String,
    /// 已解析的目标文件绝对路径（仅内部恢复使用，不发送给模型）。
    pub target_path: String,
    /// 期望的 target revision（写入前的 current revision）。
    pub expected_revision: String,
    /// 候选新内容的 revision；新建文件提交后 temp 已被移动时用于确认 committed。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_revision: Option<String>,
    /// 临时文件路径（同目录唯一 temp）。
    pub temp_path: String,
    /// 备份路径（§10.7 Windows ReplaceFileW backup；M3 起非空）。
    pub backup_path: Option<String>,
}

/// 会话中断原因（§4.3：provider 断联是记录型事实，不伪装成已提交内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterruptCause {
    /// 连接/传输层失败（DNS/reset/timeout/5xx 等）。
    Connection,
    /// 收到事件后的协议错误（SSE 截断、解析失败、流在 [DONE] 前结束等）。
    Protocol,
    /// 服务端限流（429）。
    RateLimited,
    /// 认证失败（401/403）。
    Auth,
    /// 其他/未知。
    Other,
}

/// Durable session 事件的完整枚举。
///
/// 只有已提交事实才能成为事件：私有 reasoning、未提交的流式增量
/// 都不是长期事实来源（§3.2 不变量 10）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    UserSubmitted {
        content: String,
    },
    /// §13（AGENTS.md）：模型通过 `request_input` 请求用户输入（run 挂起）。
    /// 与 [`SessionEvent::UserInputReceived`] 成对；`prompt` 是模型提出的问题。
    UserInputRequested {
        prompt: String,
    },
    /// §13：用户对挂起问题的回答（durable 事实；对话投影仍由 UserSubmitted 承担，
    /// 该事件是“这是对挂起请求的响应”这一语义的记录）。
    UserInputReceived {
        content: String,
    },
    RunStarted {
        model: ModelRef,
        limits: RunLimits,
    },
    AssistantMessageCommitted {
        message: AssistantMessage,
    },
    /// 流中断的 assistant attempt（§4.3）：与 [`AssistantMessageCommitted`] 语义不同——
    /// 这不是一个完整 turn，不进入对话历史投影；只记录"部分输出曾存在"这一事实，
    /// 供恢复/诊断与 UI 一致性。partial content 已发给 UI 但未提交为上下文。
    AssistantAttemptInterrupted {
        request_id: RequestId,
        content: String,
        cause: InterruptCause,
        saw_tool_calls: bool,
    },
    ToolRequested {
        call: ToolCall,
    },
    /// 写工具在副作用前必须持久化 recovery metadata（§14.2 write-ahead）。
    ToolStarted {
        call_id: ToolCallId,
        recovery: Option<RecoveryMetadata>,
    },
    ToolCompleted {
        call_id: ToolCallId,
        outcome: StoredToolOutcome,
    },
    PlanReplaced {
        plan: Plan,
    },
    CompactionCommitted {
        covered: EventRange,
        summary: CompactSummary,
    },
    RunCompleted {
        reason: CompletionReason,
        usage: Usage,
    },
}

impl SessionEvent {
    /// 事件类型名（envelope 的 `type` 字段）。
    pub fn type_name(&self) -> &'static str {
        match self {
            SessionEvent::UserSubmitted { .. } => "user_submitted",
            SessionEvent::UserInputRequested { .. } => "user_input_requested",
            SessionEvent::UserInputReceived { .. } => "user_input_received",
            SessionEvent::RunStarted { .. } => "run_started",
            SessionEvent::AssistantMessageCommitted { .. } => "assistant_message_committed",
            SessionEvent::AssistantAttemptInterrupted { .. } => "assistant_attempt_interrupted",
            SessionEvent::ToolRequested { .. } => "tool_requested",
            SessionEvent::ToolStarted { .. } => "tool_started",
            SessionEvent::ToolCompleted { .. } => "tool_completed",
            SessionEvent::PlanReplaced { .. } => "plan_replaced",
            SessionEvent::CompactionCommitted { .. } => "compaction_committed",
            SessionEvent::RunCompleted { .. } => "run_completed",
        }
    }
}

/// JSONL envelope（§14.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub schema: u32,
    pub seq: u64,
    pub event_id: EventId,
    pub timestamp: String,
    pub session_id: SessionId,
    pub run_id: RunId,
    #[serde(flatten)]
    pub body: EventBody,
}

/// Envelope 的 type + payload 部分（§14.2：`{"type": ..., "payload": {...}}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventBody {
    UserSubmitted {
        payload: UserSubmittedPayload,
    },
    UserInputRequested {
        payload: UserInputRequestedPayload,
    },
    UserInputReceived {
        payload: UserInputReceivedPayload,
    },
    RunStarted {
        payload: RunStartedPayload,
    },
    AssistantMessageCommitted {
        payload: AssistantMessageCommittedPayload,
    },
    AssistantAttemptInterrupted {
        payload: AssistantAttemptInterruptedPayload,
    },
    ToolRequested {
        payload: ToolRequestedPayload,
    },
    ToolStarted {
        payload: ToolStartedPayload,
    },
    ToolCompleted {
        payload: ToolCompletedPayload,
    },
    PlanReplaced {
        payload: PlanReplacedPayload,
    },
    CompactionCommitted {
        payload: CompactionCommittedPayload,
    },
    RunCompleted {
        payload: RunCompletedPayload,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSubmittedPayload {
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputRequestedPayload {
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputReceivedPayload {
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStartedPayload {
    pub model: ModelRef,
    pub limits: RunLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessageCommittedPayload {
    pub message: AssistantMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantAttemptInterruptedPayload {
    pub request_id: RequestId,
    pub content: String,
    pub cause: InterruptCause,
    pub saw_tool_calls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequestedPayload {
    pub call: ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStartedPayload {
    pub call_id: ToolCallId,
    pub recovery: Option<RecoveryMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCompletedPayload {
    pub call_id: ToolCallId,
    pub outcome: StoredToolOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReplacedPayload {
    pub plan: Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionCommittedPayload {
    pub covered: EventRange,
    pub summary: CompactSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCompletedPayload {
    pub reason: CompletionReason,
    pub usage: Usage,
}

impl Envelope {
    pub fn new(seq: u64, session_id: SessionId, run_id: RunId, event: &SessionEvent) -> Self {
        let body = match event {
            SessionEvent::UserSubmitted { content } => EventBody::UserSubmitted {
                payload: UserSubmittedPayload {
                    content: content.clone(),
                },
            },
            SessionEvent::UserInputRequested { prompt } => EventBody::UserInputRequested {
                payload: UserInputRequestedPayload {
                    prompt: prompt.clone(),
                },
            },
            SessionEvent::UserInputReceived { content } => EventBody::UserInputReceived {
                payload: UserInputReceivedPayload {
                    content: content.clone(),
                },
            },
            SessionEvent::RunStarted { model, limits } => EventBody::RunStarted {
                payload: RunStartedPayload {
                    model: model.clone(),
                    limits: *limits,
                },
            },
            SessionEvent::AssistantMessageCommitted { message } => {
                EventBody::AssistantMessageCommitted {
                    payload: AssistantMessageCommittedPayload {
                        message: message.clone(),
                    },
                }
            }
            SessionEvent::AssistantAttemptInterrupted {
                request_id,
                content,
                cause,
                saw_tool_calls,
            } => EventBody::AssistantAttemptInterrupted {
                payload: AssistantAttemptInterruptedPayload {
                    request_id: *request_id,
                    content: content.clone(),
                    cause: cause.clone(),
                    saw_tool_calls: *saw_tool_calls,
                },
            },
            SessionEvent::ToolRequested { call } => EventBody::ToolRequested {
                payload: ToolRequestedPayload { call: call.clone() },
            },
            SessionEvent::ToolStarted { call_id, recovery } => EventBody::ToolStarted {
                payload: ToolStartedPayload {
                    call_id: *call_id,
                    recovery: recovery.clone(),
                },
            },
            SessionEvent::ToolCompleted { call_id, outcome } => EventBody::ToolCompleted {
                payload: ToolCompletedPayload {
                    call_id: *call_id,
                    outcome: outcome.clone(),
                },
            },
            SessionEvent::PlanReplaced { plan } => EventBody::PlanReplaced {
                payload: PlanReplacedPayload { plan: plan.clone() },
            },
            SessionEvent::CompactionCommitted { covered, summary } => {
                EventBody::CompactionCommitted {
                    payload: CompactionCommittedPayload {
                        covered: *covered,
                        summary: summary.clone(),
                    },
                }
            }
            SessionEvent::RunCompleted { reason, usage } => EventBody::RunCompleted {
                payload: RunCompletedPayload {
                    reason: *reason,
                    usage: *usage,
                },
            },
        };
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        Self {
            schema: SCHEMA_VERSION,
            seq,
            event_id: EventId::new_v7(),
            timestamp,
            session_id,
            run_id,
            body,
        }
    }
}

/// envelope schema 版本（§14.2：migration 是纯函数）。
pub const SCHEMA_VERSION: u32 = 1;
/// 单条 durable event 的序列化上限。Provider、工具和编辑器各自有更窄的
/// 输入上限；session 在事实源边界统一兜底，防止异常调用者或损坏文件耗尽内存。
pub const MAX_SESSION_EVENT_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_SESSION_EVENTS: usize = 1_000_000;

/// 由事件恢复为 session 事件（envelope 反序列化结果）。
impl Envelope {
    pub fn to_session_event(&self) -> SessionEvent {
        match &self.body {
            EventBody::UserSubmitted { payload } => SessionEvent::UserSubmitted {
                content: payload.content.clone(),
            },
            EventBody::UserInputRequested { payload } => SessionEvent::UserInputRequested {
                prompt: payload.prompt.clone(),
            },
            EventBody::UserInputReceived { payload } => SessionEvent::UserInputReceived {
                content: payload.content.clone(),
            },
            EventBody::RunStarted { payload } => SessionEvent::RunStarted {
                model: payload.model.clone(),
                limits: payload.limits,
            },
            EventBody::AssistantMessageCommitted { payload } => {
                SessionEvent::AssistantMessageCommitted {
                    message: payload.message.clone(),
                }
            }
            EventBody::AssistantAttemptInterrupted { payload } => {
                SessionEvent::AssistantAttemptInterrupted {
                    request_id: payload.request_id,
                    content: payload.content.clone(),
                    cause: payload.cause.clone(),
                    saw_tool_calls: payload.saw_tool_calls,
                }
            }
            EventBody::ToolRequested { payload } => SessionEvent::ToolRequested {
                call: payload.call.clone(),
            },
            EventBody::ToolStarted { payload } => SessionEvent::ToolStarted {
                call_id: payload.call_id,
                recovery: payload.recovery.clone(),
            },
            EventBody::ToolCompleted { payload } => SessionEvent::ToolCompleted {
                call_id: payload.call_id,
                outcome: payload.outcome.clone(),
            },
            EventBody::PlanReplaced { payload } => SessionEvent::PlanReplaced {
                plan: payload.plan.clone(),
            },
            EventBody::CompactionCommitted { payload } => SessionEvent::CompactionCommitted {
                covered: payload.covered,
                summary: payload.summary.clone(),
            },
            EventBody::RunCompleted { payload } => SessionEvent::RunCompleted {
                reason: payload.reason,
                usage: payload.usage,
            },
        }
    }
}
