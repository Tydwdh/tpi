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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceRecordKind {
    SpanOpen,
    SpanClose,
    Event,
    Gap,
    Link,
    Snapshot,
}

/// 日志级别（对齐 tracing 的 Level）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// span/event 的终止结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceOutcome {
    Ok,
    Cancelled,
    Failed,
    TimedOut,
    Interrupted,
}

/// 字段敏感度（`12` §5.2）：构造 TraceValue 时声明，禁止裸 Debug string。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordCompleteness {
    Complete,
    Lossy,
    Truncated,
    Redacted,
    Crashed,
    WriterFailed,
}

/// 关联身份：创建者填它能权威提供的；sink 继承 active trace context 补全。
#[derive(Debug, Clone, Default)]
pub struct CorrelationIds {
    pub trace_id: Option<TraceId>,
    pub span_id: Option<SpanId>,
    pub session_id: Option<SessionId>,
    pub run_id: Option<RunId>,
}

/// 一条 trace record（O2 sink 的输入契约；本阶段用于 catalog 与边界注入）。
#[derive(Debug, Clone)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
