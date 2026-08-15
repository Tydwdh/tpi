//! O3 session telemetry projector（P2-09）：committed event → telemetry record。
//!
//! - 纯 projector：`project(event)` 增量应用、`rebuild(events)` 全量重建，
//!   live 与 prefix replay 共用同一实现（`incremental == replay` 验收）；
//! - 去重：`(session_id, event_seq, projector_version)` 三元组——handoff
//!   允许重复（同 seq 幂等），不允许无声明缺口（seq 跳变 → Gap 记录）；
//! - Standard 不含正文：telemetry record 只含元数据（type/seq/timestamp/
//!   counts），正文（用户文本/tool output）走独立 sidecar reference
//!   （`sidecar_seq`，Verbose 才解析）。
//!
//! 验收：任意合法 prefix、incremental == replay、sink drop 不影响 append。

use crate::protocol::SessionEvent;
use tpi_core::ids::SessionId;

/// projector 版本：去重三元组的组成部分；语义变化时递增（旧 telemetry 重新投影）。
pub const PROJECTOR_VERSION: u32 = 1;

/// 一条 telemetry record（Standard 不含正文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryRecord {
    pub session_id: SessionId,
    pub event_seq: u64,
    pub projector_version: u32,
    /// 事件类型名（user_submitted/run_started/...）。
    pub event_type: &'static str,
    /// 单调投影序号（本 projector 输出顺序）。
    pub projected_seq: u64,
    /// 事件内计数（assistant 消息数 / tool call 数等；正文不进 Standard）。
    pub counts: TelemetryCounts,
    /// Verbose 正文的 sidecar 引用（None = Standard 不含正文）。
    pub sidecar_seq: Option<u64>,
}

/// 事件级计数（metadata，非正文）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryCounts {
    pub tool_calls: u64,
    pub interrupted: u64,
}

/// 声明的缺口（seq 跳变；handoff 允许重复、不允许无声明缺口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryGap {
    pub session_id: SessionId,
    pub from_seq: u64,
    pub to_seq: u64,
    pub reason: &'static str,
}

/// 纯 projector 状态。
#[derive(Debug, Clone)]
pub struct SessionTelemetryProjector {
    session_id: SessionId,
    /// 已投影的最大 event_seq。
    last_seq: u64,
    /// 投影输出序号（单调）。
    projected_seq: u64,
    /// 声明的缺口（handoff 时 seq 跳变记录）。
    gaps: Vec<TelemetryGap>,
    records: Vec<TelemetryRecord>,
}

impl SessionTelemetryProjector {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            last_seq: 0,
            projected_seq: 0,
            gaps: Vec::new(),
            records: Vec::new(),
        }
    }

    /// 增量应用一条 committed event（按 event_seq）。
    ///
    /// - seq == last_seq + 1：正常追加；
    /// - seq <= last_seq：**重复**（handoff 幂等，忽略不报错）；
    /// - seq > last_seq + 1：**缺口**（记录 TelemetryGap，不允许无声明跳变）。
    pub fn project(&mut self, event_seq: u64, event: &SessionEvent) {
        if event_seq <= self.last_seq {
            // 重复（handoff 允许）：幂等忽略。
            return;
        }
        if event_seq > self.last_seq + 1 {
            // 无声明缺口：必须记录。
            self.gaps.push(TelemetryGap {
                session_id: self.session_id,
                from_seq: self.last_seq,
                to_seq: event_seq,
                reason: "unexpected seq jump",
            });
        }
        self.last_seq = event_seq;
        self.projected_seq += 1;
        let counts = counts_for(event);
        self.records.push(TelemetryRecord {
            session_id: self.session_id,
            event_seq,
            projector_version: PROJECTOR_VERSION,
            event_type: event.type_name(),
            projected_seq: self.projected_seq,
            counts,
            sidecar_seq: None, // Standard 不含正文；Verbose 由调用方附 sidecar
        });
    }

    /// 全量重建（等价于逐条 project；incremental == replay 验收）。
    pub fn rebuild(session_id: SessionId, events: &[(u64, SessionEvent)]) -> Self {
        let mut p = Self::new(session_id);
        for (seq, event) in events {
            p.project(*seq, event);
        }
        p
    }

    /// 已投影记录。
    pub fn records(&self) -> &[TelemetryRecord] {
        &self.records
    }

    /// 声明的缺口（无声明跳变 → 必有一条 gap）。
    pub fn gaps(&self) -> &[TelemetryGap] {
        &self.gaps
    }

    /// 已投影最大 event_seq。
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// 输出记录数。
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 是否无记录。
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

fn counts_for(event: &SessionEvent) -> TelemetryCounts {
    let mut counts = TelemetryCounts::default();
    match event {
        SessionEvent::ToolRequested { .. } => counts.tool_calls = 1,
        SessionEvent::AssistantAttemptInterrupted { .. } => counts.interrupted = 1,
        _ => {}
    }
    counts
}
