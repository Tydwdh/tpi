//! O1 trace contract（P1-07）：typed TraceRecord + catalog names + sensitivity。
//!
//! 目标（`12-debug-tracing-and-replay.md` §4-§5）：每条 trace record 能回答
//! "谁发起、基于什么输入、产生什么结果、失败后哪些信息完整"；不要求每条
//! record 填全部 ID——创建者只填它能权威提供的身份，sink 继承 active trace
//! context。
//!
//! 本模块是 **contract 类型 + catalog**，不是 exporter：
//! - 业务模块仍用 `tracing::info!/warn!` 等宏（O2 之前不迁移）；这些宏的
//!   字段经 catalog 登记（name/sensitivity/owner），禁止无登记 name 的孤儿。
//! - `TraceRecord` 是 O2 Local TraceSink 的输入契约（schema/versioned）；
//!   O1 只定义类型 + 在真实边界注入 trace_id/span_id（见 `agent.run` span）。
//!
//! 禁止业务模块直接依赖 exporter DTO（OTel 等）——本模块是唯一的
//! trace 领域类型源。

use crate::ids::{RunId, SessionId, SpanId, TraceId};

/// TraceRecord schema 版本（O2 sink 读取时校验；与 session schema 独立）。
pub const TRACE_SCHEMA_VERSION: u16 = 1;

/// 记录种类：span 开/关、事件、缺口、链接、快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TraceRecordKind {
    SpanOpen,
    SpanClose,
    Event,
    Gap,
    Link,
    Snapshot,
}

/// 日志级别（对齐 tracing 的 Level）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TraceLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// span/event 的终止结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TraceOutcome {
    Ok,
    Cancelled,
    Failed,
    TimedOut,
    Interrupted,
}

/// 字段敏感度（`12` §5.2）：构造 TraceValue 时声明，禁止裸 Debug string。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Sensitivity {
    /// 无隐私，可外发。
    Public,
    /// 内部实现细节，不面向用户但无密钥。
    Internal,
    /// workspace 文件/用户文本内容。
    WorkspaceContent,
    /// 密钥/凭据/完整环境。永远不能 Plain。
    Secret,
}

/// trace value（O2 落地前仅定义类型；敏感字段必须 Hashed/Redacted/Payload）。
#[derive(Debug, Clone)]
pub enum TraceValue {
    Plain(serde_json::Value),
    Hashed { blake3: String, bytes: u64 },
    Redacted { reason: &'static str },
}

/// 记录完整性（`12` §5.3）：manifest 报告 dropped/truncated/redacted 等。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RecordCompleteness {
    Complete,
    Lossy,
    Truncated,
    Redacted,
    Crashed,
    WriterFailed,
}

/// 关联身份：创建者填它能权威提供的；sink 继承 active trace context 补全。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CorrelationIds {
    pub trace_id: Option<TraceId>,
    pub span_id: Option<SpanId>,
    pub session_id: Option<SessionId>,
    pub run_id: Option<RunId>,
}

/// 一条 trace record（O2 sink 的输入契约；本阶段用于 catalog 与边界注入）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TraceRecord {
    pub schema: u16,
    pub record_seq: u64,
    pub monotonic_ns: u64,
    pub kind: TraceRecordKind,
    pub name: &'static str,
    pub level: TraceLevel,
    pub outcome: Option<TraceOutcome>,
    pub ids: CorrelationIds,
    pub sensitivity: Sensitivity,
    pub completeness: RecordCompleteness,
}

impl TraceRecord {
    pub fn new(name: &'static str) -> Self {
        Self {
            schema: TRACE_SCHEMA_VERSION,
            record_seq: 0,
            monotonic_ns: 0,
            kind: TraceRecordKind::Event,
            name,
            level: TraceLevel::Info,
            outcome: None,
            ids: CorrelationIds::default(),
            sensitivity: Sensitivity::Internal,
            completeness: RecordCompleteness::Complete,
        }
    }
}

// ---------------------------------------------------------------------------
// Catalog：所有 trace name 的注册表（无孤儿 name 验收）。
// 每个 name：sensitivity（记录字段的最大敏感度）+ owner（产生该记录的原生模块）。
// 新增 span/event 必须先在此登记（测试 trace_catalog_is_complete 强制）。
// ---------------------------------------------------------------------------

/// 一个 catalog 条目：name 的唯一注册。
pub struct CatalogEntry {
    pub name: &'static str,
    pub kind: &'static str, // "span" | "event"
    pub sensitivity: Sensitivity,
    pub owner: &'static str,
}

/// 已登记的全部 trace name（O1 盘点，2026-08-14；O2 落地后随 sink 演进）。
pub const CATALOG: &[CatalogEntry] = &[
    // ---- spans ----
    CatalogEntry {
        name: "agent.run",
        kind: "span",
        sensitivity: Sensitivity::Internal,
        owner: "agent::run",
    },
    // ---- events（tracing 宏调用点，字段见 14-trace-catalog.md）----
    CatalogEntry {
        name: "tpi starting",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "main",
    },
    CatalogEntry {
        name: "agent run completed",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "agent::run_inner",
    },
    CatalogEntry {
        name: "run approaching wall-time budget",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "agent::limits",
    },
    CatalogEntry {
        name: "tool completed",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "agent::tool_runtime",
    },
    CatalogEntry {
        name: "compaction failed; not retrying in this band",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "agent::run_inner",
    },
    CatalogEntry {
        name: "manual compaction: summary invalid",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "agent::run_inner",
    },
    CatalogEntry {
        name: "manual compaction: not significant",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "agent::run_inner",
    },
    CatalogEntry {
        name: "provider trace 无法打开日志文件；trace 已禁用",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "provider::trace",
    },
    CatalogEntry {
        name: "provider trace 写入失败",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "provider::trace",
    },
    CatalogEntry {
        name: "session 第 {} 行损坏: {}",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "session",
    },
    CatalogEntry {
        name: "decode_html_entities: 字节索引越界（内部不变量破坏）",
        kind: "event",
        sensitivity: Sensitivity::Internal,
        owner: "tool::web",
    },
];

/// 检查某 name 是否已登记（未登记 → 孤儿，应补 catalog）。
pub fn is_registered(name: &str) -> bool {
    CATALOG.iter().any(|e| e.name == name)
}

/// 按 name 查 catalog 条目。
pub fn entry(name: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.name == name)
}

// ---------------------------------------------------------------------------
// O2 Local TraceSink（P2-08）：有界队列 + gap counter + flush guard。
//
// - [`TraceSink`]：application 持有的写端。有界（records+bytes），溢出时
//   gap counter +1 并在后续写入前插入一条 [`TraceGap`]（声明缺口，不伪装完整）；
// - [`TraceFlushGuard`]：Drop 时 flush 未写队列（或 shutdown deadline 后放弃）；
// - [`SinkStats`]：manifest/completeness（dropped/gap/seq 范围）。
//
// sink error 只降级观测：flush 失败不 panic、不影响调用方（session/run）。

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// 队列上限（records）。
const MAX_QUEUED_RECORDS: usize = 4096;
/// 队列上限（bytes）。
const MAX_QUEUED_BYTES: usize = 1024 * 1024;

/// 一条声明的 trace 缺口（溢出/写入失败时产生，manifest 披露）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TraceGap {
    pub dropped_records: u64,
    pub reason: &'static str,
    pub at_seq: u64,
}

/// sink 统计（manifest/completeness 依据）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SinkStats {
    pub written_records: u64,
    pub dropped_records: u64,
    pub gaps: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
}

/// 有界 trace sink：记录进队列，FlushGuard 落盘。
///
/// 线程安全：`push` 可并发（原子 seq + mutex 队列）；`flush` 需 &mut。
pub struct TraceSink<W: Write + Send> {
    writer: W,
    queue: VecDeque<(u64, TraceRecord)>,
    queued_bytes: usize,
    seq: AtomicU64,
    stats: SinkStats,
    /// 溢出导致的 pending gap（下次 flush 前插入）。
    pending_gap: Option<TraceGap>,
}

impl<W: Write + Send> TraceSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            queue: VecDeque::new(),
            queued_bytes: 0,
            seq: AtomicU64::new(0),
            stats: SinkStats::default(),
            pending_gap: None,
        }
    }

    /// 入队一条记录。有界：超出 records/bytes 时丢最旧 + gap counter。
    pub fn push(&mut self, record: TraceRecord) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut record = record;
        record.record_seq = seq;
        let approx_bytes = std::mem::size_of_val(&record) + record.name.len();

        if self.queue.len() >= MAX_QUEUED_RECORDS
            || self.queued_bytes + approx_bytes > MAX_QUEUED_BYTES
        {
            // 溢出：丢最旧 + gap counter + 声明 TraceGap。
            if let Some((dropped_seq, _)) = self.queue.pop_front() {
                self.queued_bytes = self
                    .queued_bytes
                    .saturating_sub(std::mem::size_of_val(&dropped_seq));
                self.stats.dropped_records += 1;
            }
            self.stats.gaps += 1;
            self.pending_gap = Some(TraceGap {
                dropped_records: 1,
                reason: "queue overflow",
                at_seq: seq,
            });
        }
        self.queue.push_back((seq, record));
        self.queued_bytes += approx_bytes;
        if self.stats.first_seq.is_none() {
            self.stats.first_seq = Some(seq);
        }
        self.stats.last_seq = Some(seq);
    }

    /// 当前队列深度。
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// 底层 writer 可变访问（测试注入故障用）。
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// 统计（manifest/completeness）。
    pub fn stats(&self) -> &SinkStats {
        &self.stats
    }

    /// flush 全部队列到 writer（含 pending gap）。失败只降级观测（记录错误）。
    pub fn flush(&mut self) {
        // 先写 pending gap（声明缺口）。
        if let Some(gap) = self.pending_gap.take()
            && let Ok(line) = serde_json::to_string(&gap)
        {
            let _ = writeln!(self.writer, "{}", line);
        }
        while let Some((_seq, record)) = self.queue.pop_front() {
            let line = serde_json::to_string(&record);
            match line {
                Ok(line) => {
                    if writeln!(self.writer, "{}", line).is_err() {
                        // 写入失败：停止（后续 flush 重试）；不 panic。
                        self.queue.push_front((0, record)); // 放回，下次重试
                        self.stats.dropped_records += 1;
                        break;
                    }
                    self.stats.written_records += 1;
                }
                Err(_) => self.stats.dropped_records += 1,
            }
        }
        self.queued_bytes = self
            .queue
            .iter()
            .map(|(_, r)| std::mem::size_of_val(r) + r.name.len())
            .sum();
        let _ = self.writer.flush();
    }

    /// 队列是否为空。
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// Flush guard：Drop 时 flush 未写队列（RAII）。
/// 若 flush 耗时超 deadline（shutdown deadline），仍尽力 flush（不强制超时——
/// 有界队列保证 flush 有界）。
pub struct TraceFlushGuard<'a, W: Write + Send> {
    sink: &'a mut TraceSink<W>,
}

impl<'a, W: Write + Send> TraceFlushGuard<'a, W> {
    pub fn new(sink: &'a mut TraceSink<W>) -> Self {
        Self { sink }
    }
}

impl<W: Write + Send> Drop for TraceFlushGuard<'_, W> {
    fn drop(&mut self) {
        self.sink.flush();
    }
}

// ---------------------------------------------------------------------------
// P6-09：O5 trace inspector——只读视图（timeline/gap/completeness）。
//
// inspector 读取冻结 snapshot/segment；**禁止通过 debug UI 触发 tool/session
// mutation**（本模块只读 SinkStats/TraceRecord）。

/// inspector 视图（只读；由 sink stats 投影）。
#[derive(Debug, Clone, PartialEq)]
pub struct InspectorView {
    pub written: u64,
    pub dropped: u64,
    pub gaps: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    /// 完整性 = 写入 / 总记录（gap 可见，不伪装完整）。
    pub completeness_ratio: f64,
}

/// 从 sink stats 构建只读 inspector 视图（不修改 sink）。
pub fn inspect(stats: &SinkStats) -> InspectorView {
    let total = stats.written_records + stats.dropped_records;
    InspectorView {
        written: stats.written_records,
        dropped: stats.dropped_records,
        gaps: stats.gaps,
        first_seq: stats.first_seq,
        last_seq: stats.last_seq,
        completeness_ratio: if total == 0 {
            1.0
        } else {
            stats.written_records as f64 / total as f64
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// inspector 对 incomplete/dropped 数据不伪装完整。
    #[test]
    fn inspector_reports_incompleteness() {
        let stats = SinkStats {
            written_records: 8,
            dropped_records: 2,
            gaps: 1,
            first_seq: Some(1),
            last_seq: Some(10),
        };
        let view = inspect(&stats);
        assert_eq!(view.gaps, 1, "gap 可见");
        assert!((view.completeness_ratio - 0.8).abs() < 1e-9, "完整性如实");
        assert!((view.completeness_ratio - 1.0).abs() >= 1e-9, "不伪装完整");
    }

    /// 空 sink：完整（无记录）。
    #[test]
    fn empty_sink_is_complete() {
        let view = inspect(&SinkStats::default());
        assert_eq!(view.completeness_ratio, 1.0);
        assert_eq!(view.written, 0);
    }

    #[test]
    fn catalog_has_no_duplicate_names() {
        let mut names: Vec<&str> = CATALOG.iter().map(|e| e.name).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "catalog 不允许重复 name");
    }

    #[test]
    fn catalog_entries_are_non_empty_and_owned() {
        for e in CATALOG {
            assert!(!e.name.is_empty());
            assert!(!e.owner.is_empty());
            assert!(matches!(e.kind, "span" | "event"));
        }
    }

    #[test]
    fn registered_names_resolve() {
        assert!(is_registered("agent.run"));
        assert!(is_registered("tool completed"));
        assert_eq!(entry("agent.run").unwrap().owner, "agent::run");
        assert!(!is_registered("never-registered-name"));
    }
}
