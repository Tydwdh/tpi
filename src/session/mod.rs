//! Session 持久层（文档 §14）。
//!
//! [`SessionEvent`] 是 durable 事件的唯一集合：session log 是事实源，
//! TUI transcript、上下文和统计都是 projection（§3.2 不变量 8）。
//!
//! 文件布局（§14.1）：`~/.tpi/sessions/<workspace-id>/<session-id>.jsonl`。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::ids::{EventId, RunId, SessionId, ToolCallId};
use crate::provider::{ChatMessage, ToolCall};
use crate::tool::outcome::StoredToolOutcome;
use serde::{Deserialize, Serialize};

pub mod artifact;
pub mod recovery;

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
    /// 压缩与 prune 后上下文仍超出模型窗口（P1-4：不再发起必然失败的请求）。
    ContextOverflow,
    /// 长度限制、内容过滤或协议错误。
    Error,
}

/// token 用量（provider 返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// 写工具执行前的 recovery metadata（§10.7/§14.2 write-ahead）。
///
/// 在产生任何外部副作用前持久化；崩溃恢复据此判定 `effect`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryMetadata {
    pub tool: String,
    /// 目标文件路径（workspace-relative）。
    pub target_path: String,
    /// 期望的 target revision（写入前的 current revision）。
    pub expected_revision: String,
    /// 临时文件路径（同目录唯一 temp）。
    pub temp_path: String,
    /// 备份路径（§10.7 Windows ReplaceFileW backup；M3 起非空）。
    pub backup_path: Option<String>,
}

/// Durable session 事件（文档 §4.3 的完整枚举）。
///
/// 只有已提交事实才能成为事件：私有 reasoning、未提交的流式增量
/// 都不是长期事实来源（§3.2 不变量 10）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    UserSubmitted {
        content: String,
    },
    RunStarted {
        model: ModelRef,
        limits: RunLimits,
    },
    AssistantMessageCommitted {
        message: AssistantMessage,
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
            SessionEvent::RunStarted { .. } => "run_started",
            SessionEvent::AssistantMessageCommitted { .. } => "assistant_message_committed",
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
    RunStarted {
        payload: RunStartedPayload,
    },
    AssistantMessageCommitted {
        payload: AssistantMessageCommittedPayload,
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
pub struct RunStartedPayload {
    pub model: ModelRef,
    pub limits: RunLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessageCommittedPayload {
    pub message: AssistantMessage,
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

/// 由事件恢复为 session 事件（envelope 反序列化结果）。
impl Envelope {
    pub fn to_session_event(&self) -> SessionEvent {
        match &self.body {
            EventBody::UserSubmitted { payload } => SessionEvent::UserSubmitted {
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
    seq: u64,
    /// 是否需要 fsync 后才算 durable（写工具 write-ahead 使用）。
    pending_sync: bool,
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
        let path = dir.join(format!("{session_id}.jsonl"));
        let log = Self {
            session_id,
            run_id,
            workspace_id,
            path: path.clone(),
            file: OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&path)?,
            seq: 0,
            pending_sync: false,
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
        let mut log = Self {
            session_id,
            run_id: RunId::new_v7(),
            workspace_id,
            path: path.clone(),
            file: OpenOptions::new().append(true).open(&path)?,
            seq: 0,
            pending_sync: false,
        };
        // 恢复 seq：历史最大 envelope seq + 1（P0-12：不能按 events.len()——
        // 中间有损坏行时 len 小于 max_seq，续写会导致 seq 重复/倒退）。
        let (_, max_seq) = read_events_and_max_seq(&path)?;
        log.seq = max_seq + 1;
        Ok(log)
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn run_id(&self) -> RunId {
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
        self.seq += 1;
        let envelope = Envelope::new(self.seq, self.session_id, self.run_id, event);
        let line = serde_json::to_string(&envelope)
            .map_err(|e| std::io::Error::other(format!("serialize event: {e}")))?;
        writeln!(self.file, "{line}")?;
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

/// 读取全部事件并保留 envelope seq（P0-3/§19B：中间存在损坏行时
/// index != seq，compaction 覆盖范围与投影跳过必须基于真实 seq，
/// 不能拿 vector index 假装等于事件 seq）。
pub fn read_events_with_seq(path: &Path) -> std::io::Result<Vec<(u64, SessionEvent)>> {
    let file = File::open(path)?;
    let mut lines = BufReader::new(file).lines();
    let mut events = Vec::new();
    loop {
        match lines.next() {
            Some(Ok(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Envelope>(&line) {
                    Ok(envelope) => events.push((envelope.seq, envelope.to_session_event())),
                    Err(error) => {
                        // 非最后一行损坏：记录并跳过。
                        tracing::warn!(%error, "skipping unparseable session line");
                    }
                }
            }
            Some(Err(error)) => {
                // 最后一行因崩溃不完整（§14.2）：只丢弃残行。
                if events.is_empty() || error.kind() == std::io::ErrorKind::UnexpectedEof {
                    tracing::warn!(%error, "dropping incomplete trailing session line");
                }
                break;
            }
            None => break,
        }
    }
    Ok(events)
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
            result.push(single.into_iter().next().expect("prune 不改变消息数量"));
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
            let end_seq = covered.end.0.as_u128() as u64;
            if end.is_none_or(|prev| end_seq > prev) {
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
    let mut pending_calls: Vec<ToolCall> = Vec::new();
    for (seq, event) in events {
        if let SessionEvent::ToolRequested { call } = event {
            pending_calls.push(call.clone());
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
                if let Some(call) = pending_calls.iter().find(|call| call.call_id == *call_id) {
                    raw.push((
                        *seq,
                        ChatMessage::Tool {
                            tool_call_id: call.provider_id.clone(),
                            name: call.name.clone(),
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
            .unwrap_or(max_seq + 1);
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
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut max_seq: u64 = 0;
    let mut lines = reader.lines();
    loop {
        let line = lines.next();
        match line {
            Some(Ok(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Envelope>(&line) {
                    Ok(envelope) => {
                        max_seq = max_seq.max(envelope.seq);
                        events.push(envelope.to_session_event());
                    }
                    Err(error) => {
                        // 非最后一行损坏：记录并跳过。
                        tracing::warn!(%error, "skipping unparseable session line");
                    }
                }
            }
            Some(Err(error)) => {
                // 最后一行因崩溃不完整（§14.2）：只丢弃残行。
                if events.is_empty() || error.kind() == std::io::ErrorKind::UnexpectedEof {
                    tracing::warn!(%error, "dropping incomplete trailing session line");
                }
                break;
            }
            None => break,
        }
    }
    Ok((events, max_seq))
}

/// 该工具是否属于需要 write-ahead 的类型（§12.1：Write / WorkspaceUnknown）。
pub fn requires_write_ahead(tool_name: &str) -> bool {
    matches!(tool_name, "edit" | "write" | "bash")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;
    use camino::Utf8PathBuf;

    /// P0-12：session 文件中存在损坏行时，恢复 seq 必须基于历史最大 envelope
    /// seq（而不是 events.len()），否则继续 append 会 seq 重复，破坏单调性。
    #[test]
    fn seq_after_open_is_max_seq_plus_one_even_with_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let workspace_id = workspace_id_for(workspace.as_std_path());
        let session_id = SessionId::new_v7();
        let path = sessions_root
            .join(&workspace_id)
            .join(format!("{session_id}.jsonl"));

        // 手工构造：seq 1、seq 2、损坏行、seq 4（3 是垃圾）。
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
        std::fs::write(
            &path,
            std::fs::read_to_string(&path).unwrap() + "this-is-not-json\n",
        )
        .unwrap();
        // 崩溃后恢复：open 既有 session，追加一条（应为 seq 4）。
        let mut log =
            SessionLog::open(&sessions_root, workspace.as_std_path(), session_id).unwrap();
        let seq = log
            .append_event(&SessionEvent::UserSubmitted {
                content: "four".into(),
            })
            .unwrap();
        assert_eq!(seq, 4, "损坏行之后的 append 必须是 max_seq+1");
        drop(log);

        let (events, max_seq) = read_events_and_max_seq(&path).unwrap();
        assert_eq!(max_seq, 4, "max_seq 必须包含损坏行之后的事件");
        assert_eq!(events.len(), 3, "损坏行被跳过");

        // 重新 open 后 seq 必须是 max_seq+1，append 的事件 seq 单调且不重复。
        let reopened = SessionLog::open(&sessions_root, workspace.as_std_path(), session_id)
            .expect("open session");
        assert_eq!(reopened.seq(), 5, "恢复 seq 必须基于 max_seq（P0-12）");
    }
}
