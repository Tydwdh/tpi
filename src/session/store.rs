//! Session store（P2-01 拆分）：append/read/sync/lock、head cursor 与投影。
//!
//! [`SessionLog`] 是 append-only 日志（单写者 + 文件锁 + 尾部修复）；
//! 投影（replay_messages/project_messages 等）把 durable events 重建为
//! 对话消息（P1-02：先产出 domain message，再经 adapter 生成 provider wire）。
//!
//! 依赖方向：store -> protocol（wire 类型）。store 不定义新的 durable 类型。

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ids::{RunId, SessionId, ToolCallId};
use crate::provider::{ChatMessage, ToolCall};
use crate::session::protocol::{
    self, Envelope, EventBody, RecoveryMetadata, SCHEMA_VERSION, SessionEvent,
};
use crate::tool::outcome::StoredToolOutcome;
use crate::tool::plan::Plan;

pub(crate) use protocol::MAX_SESSION_EVENTS;

/// workspace-id 由规范化 workspace path 计算（§14.1；UI/模型不展示该绝对路径）。
pub fn workspace_id_for(workspace_root: &Path) -> String {
    let canonical = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
    digest.to_hex()[..12].to_string()
}

/// `SessionStore` port（P2-02）：agent 依赖的最小 session 写接口。
///
/// - [`SessionLog`]（JSONL adapter）实现本 trait；
/// - in-memory fake（测试）实现本 trait 跑 agent_flow；
/// - agent 通过 `&mut S: SessionStore` 访问，不知道 JSONL/文件细节。
///
/// 单写者：同一时刻只有一个 `&mut S` 持有者（Rust 借用保证）；seq 由实现
/// 维护（append 返回新 seq）；recovery/单写者/seq 契约由 adapter 测试验证。
pub trait SessionStore {
    /// 当前 run 边界（durable RunStarted 前调用）。
    fn begin_run(&mut self) -> RunId;
    /// session 身份（UI/上下文展示）。
    fn session_id(&self) -> SessionId;
    /// 当前已提交事件数（seq）。
    fn seq(&self) -> u64;
    /// durable 事实源路径（诊断/恢复用；adapter 返回文件路径，fake 返回占位）。
    fn path(&self) -> &std::path::Path;
    /// append 一个 durable 事件，返回新 seq。
    fn append_event(&mut self, event: &SessionEvent) -> std::io::Result<u64>;
    /// flush 到 durable boundary（write-ahead 的 commit point）。
    fn sync_data(&mut self) -> std::io::Result<()>;
    /// 写工具执行前记录恢复信息（ToolStarted + sync）。
    fn write_ahead_tool(
        &mut self,
        call_id: ToolCallId,
        recovery: Option<RecoveryMetadata>,
    ) -> std::io::Result<()>;
    /// 工具终态（ToolCompleted + sync）。
    fn complete_tool(
        &mut self,
        call_id: ToolCallId,
        outcome: &StoredToolOutcome,
    ) -> std::io::Result<()>;
    /// 读取全部事件（含 seq）——投影/恢复的最小读取接口。
    /// agent 不直接碰文件路径（P2-02：path() 仅诊断用）。
    fn events_with_seq(&self) -> std::io::Result<Vec<(u64, SessionEvent)>>;
}

/// Append-only session 日志（JSONL adapter，实现 [`SessionStore`]）。
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
        if bytes.len() > crate::session::protocol::MAX_SESSION_EVENT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "session event 超过 {} 字节上限",
                    crate::session::protocol::MAX_SESSION_EVENT_BYTES
                ),
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

/// SessionLog 是 JSONL adapter：直接转发到既有方法（P2-02）。
impl SessionStore for SessionLog {
    fn begin_run(&mut self) -> RunId {
        SessionLog::begin_run(self)
    }
    fn session_id(&self) -> SessionId {
        SessionLog::session_id(self)
    }
    fn seq(&self) -> u64 {
        SessionLog::seq(self)
    }
    fn path(&self) -> &std::path::Path {
        SessionLog::path(self)
    }
    fn append_event(&mut self, event: &SessionEvent) -> std::io::Result<u64> {
        SessionLog::append_event(self, event)
    }
    fn sync_data(&mut self) -> std::io::Result<()> {
        SessionLog::sync_data(self)
    }
    fn write_ahead_tool(
        &mut self,
        call_id: ToolCallId,
        recovery: Option<RecoveryMetadata>,
    ) -> std::io::Result<()> {
        SessionLog::write_ahead_tool(self, call_id, recovery)
    }
    fn complete_tool(
        &mut self,
        call_id: ToolCallId,
        outcome: &StoredToolOutcome,
    ) -> std::io::Result<()> {
        SessionLog::complete_tool(self, call_id, outcome)
    }
    fn events_with_seq(&self) -> std::io::Result<Vec<(u64, SessionEvent)>> {
        crate::session::read_events_with_seq(SessionLog::path(self))
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

/// latest_plan 的 events 版（P2-02）：从 `events_with_seq` 读取，不依赖文件路径。
pub fn latest_plan_from_events(events: &[(u64, SessionEvent)]) -> std::io::Result<Option<Plan>> {
    Ok(events.iter().rev().find_map(|(_, event)| match event {
        SessionEvent::PlanReplaced { plan } => Some(plan.clone()),
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

/// 从 durable log 重建 **domain messages**（P1-02：session projection 先输出
/// domain message，provider converter 再生成旧 `ChatMessage`）。
/// 等价性：`replay_domain_messages -> ChatMessage::from` 与 `replay_messages`
/// 语义等价（`tests/domain_message.rs` golden parity 验证）。
pub fn replay_domain_messages(path: &Path) -> std::io::Result<Vec<crate::message::DomainMessage>> {
    let events = read_events_with_seq(path)?;
    Ok(project_domain_messages(&events))
}

/// 投影 + recent prune（replay 语义，P0-3）：被 compaction 保留的 recent
/// 消息（事件 seq ∈ [covered.end, compaction 事件 seq)）应用与 runtime
/// 相同的 deterministic prune（§15.3）——compaction 后 runtime 保留的是
/// pruned 版本，replay 必须一致。
///
/// P1-02：内部投影先产出 `DomainMessage`，再转回 provider wire `ChatMessage`
/// （direction: events -> domain -> provider）。
pub fn project_messages(events: &[(u64, SessionEvent)]) -> Vec<ChatMessage> {
    project_domain_messages(events)
        .into_iter()
        .map(|message| ChatMessage::from(&message))
        .collect()
}

/// events -> domain messages（无 recent-prune；`project_messages` 的 domain 形态）。
pub fn project_domain_messages(
    events: &[(u64, SessionEvent)],
) -> Vec<crate::message::DomainMessage> {
    let ranges = project_messages_with_ranges(events);
    let (Some(end), Some(comp_seq), _) = compacted_range(events) else {
        return ranges.into_iter().map(|(m, _, _)| m).collect();
    };
    let mut result = Vec::with_capacity(ranges.len());
    for (message, start, _) in ranges {
        if start >= end && start < comp_seq {
            let single = crate::context::prune_messages(vec![ChatMessage::from(&message)]);
            match single.into_iter().next() {
                Some(item) => result.push(crate::message::DomainMessage::from(&item)),
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
) -> Vec<(crate::message::DomainMessage, u64, u64)> {
    use crate::message::{DomainContentBlock, DomainMessage, DomainRole};
    // 1. 最新 compaction 覆盖范围（P0-8：covered.end exclusive，跳过 seq < end）。
    let (compacted_up_to, _, summary_text) = compacted_range(events);

    // 2. 重建消息（跳过被覆盖的事件），记录每条消息的起始 seq。
    // pending_calls 在所有事件上收集（P0-3/§18.2 防御）：ToolRequested 即使
    // 被覆盖也不影响其 ToolCompleted 的关联（正常路径下消息单元原子，
    // 不会出现 request 覆盖而 completed 保留）。
    // P1-02：投影直接产出 DomainMessage（events -> domain），provider wire
    // 由调用方（project_messages）经 adapter 生成。
    let mut raw: Vec<(u64, DomainMessage)> = Vec::new();
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
                raw.push((*seq, DomainMessage::text(DomainRole::User, content.clone())));
                last_assistant_idx = None;
            }
            SessionEvent::AssistantMessageCommitted { message } => {
                let mut blocks = Vec::with_capacity(message.tool_calls.len() + 1);
                if !message.content.is_empty() {
                    blocks.push(DomainContentBlock::Text(message.content.clone()));
                }
                blocks.extend(
                    message
                        .tool_calls
                        .iter()
                        .cloned()
                        .map(DomainContentBlock::ToolCall),
                );
                raw.push((
                    *seq,
                    DomainMessage {
                        role: DomainRole::Assistant,
                        content: blocks,
                    },
                ));
                last_assistant_idx = Some(raw.len() - 1);
            }
            SessionEvent::ToolRequested { call } => {
                if let Some(idx) = last_assistant_idx
                    && let (_, DomainMessage {
                        role: DomainRole::Assistant,
                        content: blocks,
                    }) = &mut raw[idx]
                    && !blocks
                        .iter()
                        .any(|b| matches!(b, DomainContentBlock::ToolCall(c) if c.call_id == call.call_id))
                {
                    blocks.push(DomainContentBlock::ToolCall(call.clone()));
                }
            }
            SessionEvent::ToolCompleted { call_id, outcome } => {
                if let Some(call) = pending_calls.remove(call_id) {
                    raw.push((
                        *seq,
                        DomainMessage {
                            role: DomainRole::Tool,
                            content: vec![DomainContentBlock::ToolResult {
                                tool_call_id: call.provider_id,
                                name: call.name,
                                content: outcome.model_payload.output.clone(),
                            }],
                        },
                    ));
                }
            }
            _ => {}
        }
    }

    // 3. 每条消息的 seq 边界（end = 下一条消息的 start；最后一条 = max_seq + 1）。
    let max_seq = events.iter().map(|(seq, _)| *seq).max().unwrap_or(0);
    let mut out: Vec<(DomainMessage, u64, u64)> = Vec::with_capacity(raw.len() + 1);
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
                DomainMessage::text(
                    DomainRole::User,
                    format!("（此前会话的压缩摘要，见 CompactionCommitted）\n{summary}"),
                ),
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
pub fn read_envelopes(path: &Path) -> std::io::Result<Vec<Envelope>> {
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

pub(crate) struct EnvelopeRead {
    envelopes: Vec<Envelope>,
    tail_repair: TailRepair,
    protocol: SessionProtocolState,
}

#[derive(Default)]
pub(crate) struct SessionProtocolState {
    requested_calls: std::collections::HashSet<ToolCallId>,
    started_calls: std::collections::HashSet<ToolCallId>,
    completed_calls: std::collections::HashSet<ToolCallId>,
}

impl SessionProtocolState {
    pub(crate) fn validate(&self, body: &EventBody, seq: u64) -> Result<(), &'static str> {
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

    pub(crate) fn apply(&mut self, body: &EventBody) {
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
    read_envelopes_state_with_limits(
        path,
        crate::session::protocol::MAX_SESSION_EVENT_BYTES,
        MAX_SESSION_EVENTS,
    )
}

pub(crate) fn read_envelopes_state_with_limits(
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

pub(crate) fn open_and_lock_session(session_path: &Path) -> std::io::Result<File> {
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
