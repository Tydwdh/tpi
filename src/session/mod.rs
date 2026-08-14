//! Session 持久层。
//!
//! [`SessionEvent`] 是 durable 事件的唯一集合：session log 是事实源，
//! TUI transcript、上下文和统计都是可重建的 projection。
//!
//! 文件布局：`~/.tpi/sessions/<workspace-id>/<session-id>.jsonl`。

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ids::{EventId, RequestId, RunId, SessionId, ToolCallId};
use crate::provider::{ChatMessage, ToolCall};
use crate::tool::outcome::StoredToolOutcome;
use serde::{Deserialize, Serialize};

pub mod artifact;
pub mod conversation;
pub mod recovery;
pub mod repair;

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

/// 已提交的 assistant turn（P0-2：一个 provider assistant response 必须
/// 原子表达——即使 `content` 为空（纯 tool-call 轮）也必须持久化，
/// 否则 resume 重建会缺失 assistant 载体、ToolRequested 挂错位置）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: String,
    /// 该 turn 发起的工具调用（与 ToolRequested 同一数据源）。
    /// 旧 session 文件无此字段：`#[serde(default)]` 兼容读取。
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// 原子短计划（§13）。
pub use crate::tool::plan::Plan;

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
const MAX_SESSION_EVENTS: usize = 1_000_000;

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

/// workspace-id 由规范化 workspace path 计算（§14.1；UI/模型不展示该绝对路径）。
pub fn workspace_id_for(workspace_root: &Path) -> String {
    let canonical = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
    digest.to_hex()[..12].to_string()
}

/// Append-only session 日志。
pub struct SessionLog {
    session_id: SessionId,
    run_id: RunId,
    workspace_id: String,
    path: PathBuf,
    file: File,
    /// 独占锁持有在 sidecar 上，避免 Windows 下锁住 JSONL 后连投影读取也被拒绝。
    _lock_file: File,
    seq: u64,
    /// 是否需要 fsync 后才算 durable（写工具 write-ahead 使用）。
    pending_sync: bool,
    /// 无法回滚的部分写会使 append 边界失去可信性；此后拒绝继续追加。
    poisoned: bool,
    protocol: SessionProtocolState,
}

impl SessionLog {
    /// 创建新 session（事件由调用方通过 [`append_event`](Self::append_event) 写入）。
    pub fn create(
        sessions_root: &Path,
        workspace_root: &Path,
        run_id: RunId,
    ) -> std::io::Result<Self> {
        let session_id = SessionId::new_v7();
        Self::create_with_id(sessions_root, workspace_root, run_id, session_id)
    }

    pub fn create_with_id(
        sessions_root: &Path,
        workspace_root: &Path,
        run_id: RunId,
        session_id: SessionId,
    ) -> std::io::Result<Self> {
        let workspace_id = workspace_id_for(workspace_root);
        let dir = sessions_root.join(&workspace_id);
        std::fs::create_dir_all(&dir)?;
        if crate::util::is_symlink_or_reparse(&dir)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session workspace 目录不能是符号链接或 reparse point",
            ));
        }
        let path = dir.join(format!("{session_id}.jsonl"));
        let lock_file = open_and_lock_session(&path)?;
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let log = Self {
            session_id,
            run_id,
            workspace_id,
            path: path.clone(),
            file,
            _lock_file: lock_file,
            seq: 0,
            pending_sync: false,
            poisoned: false,
            protocol: SessionProtocolState::default(),
        };
        Ok(log)
    }

    /// 打开既有 session 并恢复全部事件。
    ///
    /// §14.2：最后一行若因崩溃不完整，只丢弃残行；恢复器根据
    /// ToolRequested/ToolCompleted 差集生成 Interrupted 结果（见 [`recovery`]）。
    pub fn open(
        sessions_root: &Path,
        workspace_root: &Path,
        session_id: SessionId,
    ) -> std::io::Result<Self> {
        let workspace_id = workspace_id_for(workspace_root);
        let path = sessions_root
            .join(&workspace_id)
            .join(format!("{session_id}.jsonl"));
        if crate::util::is_symlink_or_reparse(
            path.parent()
                .ok_or_else(|| std::io::Error::other("session 路径缺少父目录"))?,
        )? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session workspace 目录不能是符号链接或 reparse point",
            ));
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session 路径不是普通文件",
            ));
        }
        let lock_file = open_and_lock_session(&path)?;

        // 锁内完成验证和尾部修复，确保随后 append 总是从完整 JSONL 边界开始。
        let parsed = read_envelopes_state(&path)?;
        repair_session_tail(&path, parsed.tail_repair)?;
        let file = OpenOptions::new().read(true).append(true).open(&path)?;
        let max_seq = parsed.envelopes.last().map_or(0, |envelope| envelope.seq);
        let protocol = parsed.protocol;
        let log = Self {
            session_id,
            run_id: RunId::new_v7(),
            workspace_id,
            path: path.clone(),
            file,
            _lock_file: lock_file,
            seq: max_seq,
            pending_sync: false,
            poisoned: false,
            protocol,
        };
        Ok(log)
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// 为新的用户 run 分配 envelope run_id；session_id 保持不变。
    pub fn begin_run(&mut self) -> RunId {
        self.run_id = RunId::new_v7();
        self.run_id
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// 当前事件序号（§14.2 envelope seq；compaction 的 covered 范围用）。
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加事件；返回 seq。
    ///
    /// 写工具 write-ahead（§14.2）：对 `Write`/`WorkspaceUnknown` call 必须先
    /// append `ToolStarted` 并等待 [`sync_data`](Self::sync_data) 成功，随后才能产生外部副作用。
    pub fn append_event(&mut self, event: &SessionEvent) -> std::io::Result<u64> {
        if self.poisoned {
            return Err(std::io::Error::other(
                "session log 在不可恢复的部分写后已停止追加",
            ));
        }
        let next_seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("session seq 已耗尽"))?;
        let envelope = Envelope::new(next_seq, self.session_id, self.run_id, event);
        self.protocol
            .validate(&envelope.body, envelope.seq)
            .map_err(std::io::Error::other)?;
        let mut bytes = serde_json::to_vec(&envelope)
            .map_err(|e| std::io::Error::other(format!("serialize event: {e}")))?;
        if bytes.len() > MAX_SESSION_EVENT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("session event 超过 {MAX_SESSION_EVENT_BYTES} 字节上限"),
            ));
        }
        let original_len = self.file.metadata()?.len();
        bytes.push(b'\n');
        if let Err(write_error) = self.file.write_all(&bytes) {
            if let Err(rollback_error) = self.file.set_len(original_len) {
                self.poisoned = true;
                return Err(std::io::Error::other(format!(
                    "追加 session 失败: {write_error}; 回滚部分写失败: {rollback_error}"
                )));
            }
            return Err(write_error);
        }
        self.protocol.apply(&envelope.body);
        self.seq = next_seq;
        self.pending_sync = true;
        Ok(self.seq)
    }

    /// 等待数据落盘（write-ahead 的 durable boundary）。
    pub fn sync_data(&mut self) -> std::io::Result<()> {
        if self.pending_sync {
            self.file.sync_data()?;
            self.pending_sync = false;
        }
        Ok(())
    }

    /// 写工具执行序列：append ToolStarted → sync → 副作用 → append ToolCompleted → sync。
    pub fn write_ahead_tool(
        &mut self,
        call_id: ToolCallId,
        recovery: Option<RecoveryMetadata>,
    ) -> std::io::Result<()> {
        self.append_event(&SessionEvent::ToolStarted { call_id, recovery })?;
        self.sync_data()
    }

    pub fn complete_tool(
        &mut self,
        call_id: ToolCallId,
        outcome: &StoredToolOutcome,
    ) -> std::io::Result<()> {
        self.append_event(&SessionEvent::ToolCompleted {
            call_id,
            outcome: outcome.clone(),
        })?;
        self.sync_data()
    }
}

/// 读取 session 的全部事件（含残行丢弃）。
pub fn read_events(path: &Path) -> std::io::Result<Vec<SessionEvent>> {
    Ok(read_events_and_max_seq(path)?.0)
}

/// 返回 session 中最后一次完整计划替换。
///
/// Plan 是 durable harness state，但不属于聊天消息投影；恢复 run/TUI 时必须从
/// 事件流单独读取，否则跨 run 会静默丢失 Todo，模型也无法继续当前焦点。
pub fn latest_plan(path: &Path) -> std::io::Result<Option<Plan>> {
    Ok(read_events(path)?
        .into_iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::PlanReplaced { plan } => Some(plan),
            _ => None,
        }))
}

/// 读取全部事件并保留 envelope seq（P0-3/§19B：中间存在损坏行时
/// index != seq，compaction 覆盖范围与投影跳过必须基于真实 seq，
/// 不能拿 vector index 假装等于事件 seq）。
pub fn read_events_with_seq(path: &Path) -> std::io::Result<Vec<(u64, SessionEvent)>> {
    Ok(read_envelopes(path)?
        .into_iter()
        .map(|envelope| (envelope.seq, envelope.to_session_event()))
        .collect())
}

/// 从 session 文件重建对话消息（P0-3 replay 入口）：模拟进程重启后
/// 从 JSONL 重建与 runtime 语义等价的投影（跳过被 compaction 覆盖的事件、
/// 对保留的 recent 消息应用与 runtime 相同的 deterministic prune、注入
/// 最新 compaction summary）。
pub fn replay_messages(path: &Path) -> std::io::Result<Vec<ChatMessage>> {
    let events = read_events_with_seq(path)?;
    Ok(project_messages(&events))
}

/// 投影 + recent prune（replay 语义，P0-3）：被 compaction 保留的 recent
/// 消息（事件 seq ∈ [covered.end, compaction 事件 seq)）应用与 runtime
/// 相同的 deterministic prune（§15.3）——compaction 后 runtime 保留的是
/// pruned 版本，replay 必须一致。
pub fn project_messages(events: &[(u64, SessionEvent)]) -> Vec<ChatMessage> {
    let ranges = project_messages_with_ranges(events);
    let (Some(end), Some(comp_seq), _) = compacted_range(events) else {
        return ranges.into_iter().map(|(m, _, _)| m).collect();
    };
    let mut result = Vec::with_capacity(ranges.len());
    for (message, start, _) in ranges {
        if start >= end && start < comp_seq {
            let single = crate::context::prune_messages(vec![message]);
            match single.into_iter().next() {
                Some(item) => result.push(item),
                None => {
                    // prune 不改变消息数量是不变量；异常时丢日志并跳过该消息
                    // （保留其余消息，避免整个投影失败）。
                    tracing::error!(
                        seq = start,
                        "project: prune_messages 返回空（内部不变量破坏），跳过该消息",
                    );
                }
            }
        } else {
            result.push(message);
        }
    }
    result
}

/// 最新 compaction 的 (covered.end, compaction 事件 seq, summary 文本)。
/// "最新" = covered.end 最大者（compaction 覆盖范围单调扩大）。
pub fn compacted_range(
    events: &[(u64, SessionEvent)],
) -> (Option<u64>, Option<u64>, Option<String>) {
    let mut end: Option<u64> = None;
    let mut comp_seq: Option<u64> = None;
    let mut summary: Option<String> = None;
    for (seq, event) in events {
        if let SessionEvent::CompactionCommitted {
            covered,
            summary: s,
        } = event
        {
            let Ok(start_seq) = u64::try_from(covered.start.0.as_u128()) else {
                tracing::warn!(seq, "ignoring compaction with out-of-range covered.start");
                continue;
            };
            let Ok(end_seq) = u64::try_from(covered.end.0.as_u128()) else {
                tracing::warn!(seq, "ignoring compaction with out-of-range covered.end");
                continue;
            };
            if start_seq == 0 || start_seq >= end_seq || end_seq > *seq {
                tracing::warn!(seq, start_seq, end_seq, "ignoring invalid compaction range");
                continue;
            }
            if end.is_none_or(|prev| {
                end_seq > prev
                    || (end_seq == prev && comp_seq.is_none_or(|prev_seq| *seq > prev_seq))
            }) {
                end = Some(end_seq);
                comp_seq = Some(*seq);
                summary = Some(s.text.clone());
            }
        }
    }
    (end, comp_seq, summary)
}

/// 事件投影（P0-3）：从带 seq 的事件重建对话消息，附每条消息的事件 seq
/// 边界 (start_seq, end_seq_exclusive)。供 compaction 计算 covered 范围
/// （只覆盖真正被压缩的事件，runtime 保留的 recent 在 replay 端也必须保留）。
pub fn project_messages_with_ranges(
    events: &[(u64, SessionEvent)],
) -> Vec<(ChatMessage, u64, u64)> {
    // 1. 最新 compaction 覆盖范围（P0-8：covered.end exclusive，跳过 seq < end）。
    let (compacted_up_to, _, summary_text) = compacted_range(events);

    // 2. 重建消息（跳过被覆盖的事件），记录每条消息的起始 seq。
    // pending_calls 在所有事件上收集（P0-3/§18.2 防御）：ToolRequested 即使
    // 被覆盖也不影响其 ToolCompleted 的关联（正常路径下消息单元原子，
    // 不会出现 request 覆盖而 completed 保留）。
    let mut raw: Vec<(u64, ChatMessage)> = Vec::new();
    let mut last_assistant_idx: Option<usize> = None;
    let mut pending_calls: std::collections::HashMap<ToolCallId, ToolCall> =
        std::collections::HashMap::new();
    for (seq, event) in events {
        if let SessionEvent::ToolRequested { call } = event {
            pending_calls.insert(call.call_id, call.clone());
        }
        if let Some(up_to) = compacted_up_to
            && *seq < up_to
        {
            continue;
        }
        match event {
            SessionEvent::UserSubmitted { content } => {
                raw.push((*seq, ChatMessage::User(content.clone())));
                last_assistant_idx = None;
            }
            SessionEvent::AssistantMessageCommitted { message } => {
                raw.push((
                    *seq,
                    ChatMessage::Assistant {
                        content: message.content.clone(),
                        tool_calls: message.tool_calls.clone(),
                    },
                ));
                last_assistant_idx = Some(raw.len() - 1);
            }
            SessionEvent::ToolRequested { call } => {
                if let Some(idx) = last_assistant_idx
                    && let (_, ChatMessage::Assistant { tool_calls, .. }) = &mut raw[idx]
                    && !tool_calls.iter().any(|c| c.call_id == call.call_id)
                {
                    tool_calls.push(call.clone());
                }
            }
            SessionEvent::ToolCompleted { call_id, outcome } => {
                if let Some(call) = pending_calls.remove(call_id) {
                    raw.push((
                        *seq,
                        ChatMessage::Tool {
                            tool_call_id: call.provider_id,
                            name: call.name,
                            content: outcome.model_payload.output.clone(),
                        },
                    ));
                }
            }
            _ => {}
        }
    }

    // 3. 每条消息的 seq 边界（end = 下一条消息的 start；最后一条 = max_seq + 1）。
    let max_seq = events.iter().map(|(seq, _)| *seq).max().unwrap_or(0);
    let mut out: Vec<(ChatMessage, u64, u64)> = Vec::with_capacity(raw.len() + 1);
    for (i, (start, message)) in raw.iter().enumerate() {
        let end = raw
            .get(i + 1)
            .map(|(next_start, _)| *next_start)
            .unwrap_or(max_seq.saturating_add(1));
        out.push((message.clone(), *start, end));
    }

    // 4. 最新 summary 以固定前缀 user 消息注入（§15.4：不伪装成新 user intent）。
    if let Some(summary) = summary_text {
        out.insert(
            0,
            (
                ChatMessage::User(format!(
                    "（此前会话的压缩摘要，见 CompactionCommitted）\n{summary}"
                )),
                0,
                0,
            ),
        );
    }
    out
}

/// 读取全部事件并返回历史最大 envelope seq（P0-12：恢复 seq 必须基于
/// max_seq 而不是 events.len()——中间存在损坏行时两者不一致，继续 append
/// 会导致 seq 重复/倒退，破坏 envelope seq 单调性契约）。
pub fn read_events_and_max_seq(path: &Path) -> std::io::Result<(Vec<SessionEvent>, u64)> {
    let envelopes = read_envelopes(path)?;
    let max_seq = envelopes.last().map_or(0, |envelope| envelope.seq);
    Ok((
        envelopes
            .into_iter()
            .map(|envelope| envelope.to_session_event())
            .collect(),
        max_seq,
    ))
}

/// 读取并验证完整 envelope 序列。只容忍“文件末尾没有换行且 JSON 不完整”这一种
/// 崩溃形态；中间损坏、已换行的坏记录、schema/session/seq 异常都必须报错。
pub(crate) fn read_envelopes(path: &Path) -> std::io::Result<Vec<Envelope>> {
    Ok(read_envelopes_state(path)?.envelopes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailRepair {
    None,
    /// 丢弃从该 byte offset 开始的未完成尾部。
    Truncate(u64),
    /// 最后一条 envelope 完整但缺少 JSONL 分隔换行。
    AppendNewline,
}

struct EnvelopeRead {
    envelopes: Vec<Envelope>,
    tail_repair: TailRepair,
    protocol: SessionProtocolState,
}

#[derive(Default)]
struct SessionProtocolState {
    requested_calls: std::collections::HashSet<ToolCallId>,
    started_calls: std::collections::HashSet<ToolCallId>,
    completed_calls: std::collections::HashSet<ToolCallId>,
}

impl SessionProtocolState {
    fn validate(&self, body: &EventBody, seq: u64) -> Result<(), &'static str> {
        match body {
            EventBody::ToolRequested { payload } => {
                if payload.call.provider_id.trim().is_empty() || payload.call.name.trim().is_empty()
                {
                    Err("工具调用缺少 provider_id/name")
                } else if self.requested_calls.contains(&payload.call.call_id) {
                    Err("tool call id 重复")
                } else {
                    Ok(())
                }
            }
            EventBody::ToolStarted { payload } => {
                if !self.requested_calls.contains(&payload.call_id)
                    || self.completed_calls.contains(&payload.call_id)
                    || self.started_calls.contains(&payload.call_id)
                {
                    Err("ToolStarted 顺序无效")
                } else {
                    Ok(())
                }
            }
            EventBody::ToolCompleted { payload } => {
                if !self.requested_calls.contains(&payload.call_id)
                    || self.completed_calls.contains(&payload.call_id)
                {
                    Err("ToolCompleted 顺序无效")
                } else {
                    Ok(())
                }
            }
            EventBody::CompactionCommitted { payload } => {
                let start = u64::try_from(payload.covered.start.0.as_u128());
                let end = u64::try_from(payload.covered.end.0.as_u128());
                if matches!((start, end), (Ok(start), Ok(end)) if start > 0 && start < end && end <= seq)
                {
                    Ok(())
                } else {
                    Err("compaction 覆盖范围无效")
                }
            }
            _ => Ok(()),
        }
    }

    fn apply(&mut self, body: &EventBody) {
        match body {
            EventBody::ToolRequested { payload } => {
                self.requested_calls.insert(payload.call.call_id);
            }
            EventBody::ToolStarted { payload } => {
                self.started_calls.insert(payload.call_id);
            }
            EventBody::ToolCompleted { payload } => {
                self.completed_calls.insert(payload.call_id);
            }
            _ => {}
        }
    }
}

/// 读取 envelope，并返回下一次追加前需要在独占锁内完成的尾部修复。
fn read_envelopes_state(path: &Path) -> std::io::Result<EnvelopeRead> {
    read_envelopes_state_with_limits(path, MAX_SESSION_EVENT_BYTES, MAX_SESSION_EVENTS)
}

fn read_envelopes_state_with_limits(
    path: &Path,
    max_event_bytes: usize,
    max_events: usize,
) -> std::io::Result<EnvelopeRead> {
    let file = File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let expected_from_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
        .map(SessionId);
    let mut expected_session = expected_from_name;
    let mut previous_seq = 0u64;
    let mut event_ids = std::collections::HashSet::new();
    let mut protocol = SessionProtocolState::default();
    let mut envelopes = Vec::new();
    let mut tail_repair = TailRepair::None;
    let mut offset = 0u64;
    let mut line_number = 0usize;

    loop {
        let line_start = offset;
        let line = match crate::util::read_line_bounded(&mut reader, max_event_bytes)? {
            crate::util::BoundedLineRead::Eof => break,
            crate::util::BoundedLineRead::TooLong => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "session 第 {} 行超过 {} 字节上限",
                        line_number.saturating_add(1),
                        max_event_bytes
                    ),
                ));
            }
            crate::util::BoundedLineRead::Line(line) => line,
        };
        line_number = line_number.saturating_add(1);
        offset = offset
            .checked_add(line.consumed_bytes)
            .ok_or_else(|| std::io::Error::other("session 文件偏移溢出"))?;
        let has_newline = line.terminated;
        let bytes = line.bytes;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            if !has_newline {
                tail_repair = TailRepair::Truncate(line_start);
            }
            continue;
        }
        let envelope = match serde_json::from_slice::<Envelope>(&bytes) {
            Ok(envelope) => envelope,
            Err(error) if !has_newline => {
                tracing::warn!(line = line_number, %error, "dropping incomplete trailing session line");
                tail_repair = TailRepair::Truncate(line_start);
                break;
            }
            Err(error) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("session 第 {line_number} 行损坏: {error}"),
                ));
            }
        };
        if envelope.schema != SCHEMA_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "session 第 {} 行 schema={}，当前仅支持 {}",
                    line_number, envelope.schema, SCHEMA_VERSION
                ),
            ));
        }
        let session_id = *expected_session.get_or_insert(envelope.session_id);
        if envelope.session_id != session_id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session 第 {line_number} 行 session_id 不一致"),
            ));
        }
        if envelope.seq <= previous_seq {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "session 第 {} 行 seq={} 未严格递增（前一条 {}）",
                    line_number, envelope.seq, previous_seq
                ),
            ));
        }
        if !event_ids.insert(envelope.event_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session 第 {line_number} 行 event_id 重复"),
            ));
        }
        if time::OffsetDateTime::parse(
            &envelope.timestamp,
            &time::format_description::well_known::Rfc3339,
        )
        .is_err()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session 第 {line_number} 行 timestamp 无效"),
            ));
        }
        if let Err(error) = protocol.validate(&envelope.body, envelope.seq) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session 第 {line_number} 行 {error}"),
            ));
        }
        protocol.apply(&envelope.body);
        previous_seq = envelope.seq;
        envelopes.push(envelope);
        if envelopes.len() > max_events {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session 事件数超过 {max_events} 上限"),
            ));
        }
        if !has_newline {
            tail_repair = TailRepair::AppendNewline;
        }
    }
    Ok(EnvelopeRead {
        envelopes,
        tail_repair,
        protocol,
    })
}

fn open_and_lock_session(session_path: &Path) -> std::io::Result<File> {
    let mut lock_name = session_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(PathBuf::from(lock_name))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "session 正被另一个 TPI 实例使用",
        )),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn repair_session_tail(path: &Path, repair: TailRepair) -> std::io::Result<()> {
    match repair {
        TailRepair::None => Ok(()),
        TailRepair::Truncate(len) => {
            let file = OpenOptions::new().write(true).open(path)?;
            file.set_len(len)?;
            file.sync_data()
        }
        TailRepair::AppendNewline => {
            let mut file = OpenOptions::new().append(true).open(path)?;
            file.write_all(b"\n")?;
            file.sync_data()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;
    use camino::Utf8PathBuf;

    /// 中间/已换行的损坏记录不能静默跳过；只有未换行的尾部残片可丢弃。
    #[test]
    fn middle_corruption_is_rejected_but_incomplete_tail_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let workspace_id = workspace_id_for(workspace.as_std_path());
        let session_id = SessionId::new_v7();
        let path = sessions_root
            .join(&workspace_id)
            .join(format!("{session_id}.jsonl"));

        // 先写两条完整事件。
        let mut log = SessionLog::create_with_id(
            &sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
            session_id,
        )
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "one".into(),
        })
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "two".into(),
        })
        .unwrap();
        drop(log);
        let valid = std::fs::read(&path).unwrap();
        let mut corrupt = valid.clone();
        corrupt.extend_from_slice(b"this-is-not-json\n");
        std::fs::write(&path, corrupt).unwrap();
        let error = SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .err()
            .expect("完整坏行必须拒绝");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        // 崩溃只可能留下未换行尾部残片；该残片丢弃后仍可恢复完整事件。
        let mut trailing = valid;
        trailing.extend_from_slice(b"{\"schema\":1");
        std::fs::write(&path, trailing).unwrap();
        let mut reopened = SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .expect("未完成尾部可恢复");
        assert_eq!(reopened.seq(), 2, "恢复游标等于最后完整 seq");
        let seq = reopened
            .append_event(&SessionEvent::UserSubmitted {
                content: "three".into(),
            })
            .unwrap();
        assert_eq!(seq, 3, "修复残片后必须连续续写");
        drop(reopened);
        assert_eq!(read_events(&path).unwrap().len(), 3);
    }

    #[test]
    fn session_reader_rejects_an_oversized_physical_event_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, format!("{}\n", "x".repeat(33))).unwrap();
        let error = read_envelopes_state_with_limits(&path, 32, 10)
            .err()
            .expect("oversized line must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("超过 32 字节上限"), "{error}");
    }

    #[test]
    fn open_repairs_missing_newline_before_append() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let mut log =
            SessionLog::create(&sessions_root, workspace.as_std_path(), RunId::new_v7()).unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "one".into(),
        })
        .unwrap();
        let session_id = log.session_id();
        let path = log.path().to_path_buf();
        drop(log);

        let mut raw = std::fs::read(&path).unwrap();
        assert_eq!(raw.pop(), Some(b'\n'));
        std::fs::write(&path, raw).unwrap();

        let mut reopened =
            SessionLog::open(&sessions_root, workspace.as_std_path(), session_id).unwrap();
        assert_eq!(reopened.seq(), 1);
        assert_eq!(
            reopened
                .append_event(&SessionEvent::UserSubmitted {
                    content: "two".into(),
                })
                .unwrap(),
            2
        );
        drop(reopened);
        assert_eq!(read_events(&path).unwrap().len(), 2);
    }

    #[test]
    fn session_has_a_single_exclusive_writer() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let first =
            SessionLog::create(&sessions_root, workspace.as_std_path(), RunId::new_v7()).unwrap();
        let session_id = first.session_id();

        let error = SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .err()
            .expect("第二个 writer 必须被拒绝");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

        drop(first);
        SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .expect("writer 退出后应能恢复 session");
    }

    #[test]
    fn append_rejects_invalid_tool_protocol_without_advancing_seq() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let mut log =
            SessionLog::create(&sessions_root, workspace.as_std_path(), RunId::new_v7()).unwrap();
        let call_id = ToolCallId::new_v7();
        let outcome =
            crate::tool::outcome::ToolOutcome::succeeded("read", "ok".into()).into_stored();

        assert!(
            log.append_event(&SessionEvent::ToolCompleted { call_id, outcome })
                .is_err()
        );
        assert_eq!(log.seq(), 0, "被拒事件不得消耗 seq");

        log.append_event(&SessionEvent::ToolRequested {
            call: ToolCall {
                call_id,
                provider_id: "provider-call".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        })
        .unwrap();
        assert_eq!(log.seq(), 1);
    }

    #[test]
    fn compacted_range_rejects_invalid_ranges_and_prefers_later_equal_coverage() {
        let summary = |text: &str| SessionEvent::CompactionCommitted {
            covered: EventRange {
                start: EventId::from_u128(1),
                end: EventId::from_u128(3),
            },
            summary: CompactSummary { text: text.into() },
        };
        let events = vec![
            (
                2,
                SessionEvent::CompactionCommitted {
                    covered: EventRange {
                        start: EventId::from_u128(1),
                        end: EventId::from_u128(99),
                    },
                    summary: CompactSummary {
                        text: "invalid".into(),
                    },
                },
            ),
            (3, summary("first")),
            (5, summary("latest")),
        ];

        assert_eq!(
            compacted_range(&events),
            (Some(3), Some(5), Some("latest".into()))
        );
    }

    #[test]
    fn begin_run_rotates_envelope_run_id_without_changing_session() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        let initial_run = RunId::new_v7();
        let mut log =
            SessionLog::create(&sessions_root, workspace.as_std_path(), initial_run).unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "first".into(),
        })
        .unwrap();
        let next_run = log.begin_run();
        assert_ne!(initial_run, next_run);
        log.append_event(&SessionEvent::UserSubmitted {
            content: "second".into(),
        })
        .unwrap();
        let session_id = log.session_id();
        let envelopes = read_envelopes(log.path()).unwrap();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].run_id, initial_run);
        assert_eq!(envelopes[1].run_id, next_run);
        assert!(envelopes.iter().all(|event| event.session_id == session_id));
    }

    /// §16：WallTimeExceeded 是新增完成原因，必须能持久化并读回（session 文件兼容）。
    #[test]
    fn wall_time_exceeded_round_trips_through_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = PathBuf::from(dir.path());
        let mut log = SessionLog::create(
            &dir.path().join("sessions"),
            workspace.as_path(),
            RunId::new_v7(),
        )
        .unwrap();
        log.append_event(&SessionEvent::RunCompleted {
            reason: CompletionReason::WallTimeExceeded,
            usage: Usage::default(),
        })
        .unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let events = read_events(&path).unwrap();
        assert!(
            matches!(
                events.first(),
                Some(SessionEvent::RunCompleted {
                    reason: CompletionReason::WallTimeExceeded,
                    ..
                })
            ),
            "WallTimeExceeded 必须可序列化/反序列化"
        );
    }

    /// §4.3：AssistantAttemptInterrupted 是记录型事件——partial content 持久化但
    /// 不进入对话投影（与 AssistantMessageCommitted 语义区分）。
    #[test]
    fn assistant_attempt_interrupted_round_trips_and_skips_projection() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = PathBuf::from(dir.path());
        let mut log = SessionLog::create(
            &dir.path().join("sessions"),
            workspace.as_path(),
            RunId::new_v7(),
        )
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "hello".into(),
        })
        .unwrap();
        log.append_event(&SessionEvent::AssistantAttemptInterrupted {
            request_id: RequestId::new_v7(),
            content: "部分输出已经".into(),
            cause: InterruptCause::Connection,
            saw_tool_calls: false,
        })
        .unwrap();
        log.append_event(&SessionEvent::RunCompleted {
            reason: CompletionReason::ProviderInterrupted,
            usage: Usage::default(),
        })
        .unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let events = read_events(&path).unwrap();
        assert!(
            matches!(
                events.get(1),
                Some(SessionEvent::AssistantAttemptInterrupted {
                    content,
                    cause: InterruptCause::Connection,
                    saw_tool_calls: false,
                    ..
                }) if content == "部分输出已经"
            ),
            "中断事件必须可序列化/反序列化"
        );
        // 投影：中断的 attempt 不产生 assistant 消息，也不中断后续投影。
        let messages = replay_messages(&path).unwrap();
        assert_eq!(messages.len(), 1, "只有 user 消息进入投影");
        assert!(matches!(messages[0], ChatMessage::User(_)));
    }
}
