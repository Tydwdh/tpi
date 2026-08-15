//! 纯投影器（P2-03）：从事件序列重建会话视图，不依赖文件路径。
//!
//! [`ConversationProjector`] 是纯 apply/rebuild 状态机：
//! - [`Self::new`] 空投影；
//! - [`Self::apply`] 增量记录一条事件（O(1)，只追加到事件缓冲）；
//! - [`Self::rebuild`] 从全量事件序列重建（= `apply` 逐条 + 惰性投影）；
//! - [`Self::history`] / [`Self::plan`] 读取时惰性投影（等价于全量 rebuild）。
//!
//! 不变量：`history()/plan()` 与 `rebuild(events)` 完全等价（增量累积的事件
//! 与全量序列一致时）。属性测试 `incremental == rebuild` 验证任意前缀。
//!
//! 设计：incremental apply 不重复实现投影逻辑（避免 ToolRequested 关联、
//! compaction prune 等复杂度的两处实现漂移）——它只**累积事件**，读取时
//! 一次全量投影。这是"apply 记录 + rebuild 投影"的纯 projector：apply 是
//! O(1) 追加，投影只在读时发生（与当前 `refresh_from_log` 的语义等价，
//! 但纯函数、无文件 IO）。
//!
//! [`Conversation`]（conversation.rs）是 facade：持有 `SessionLog` 与本
//! 投影器；恢复/刷新经 `events_with_seq()`（P2-02 port）喂给 `rebuild`。

use crate::protocol::{Plan, SessionEvent};
use crate::store::compacted_range;
use tpi_core::message::ChatMessage;

/// 会话的纯投影状态（事件缓冲 + 惰性投影结果）。
#[derive(Debug, Clone, Default)]
pub struct ConversationProjector {
    /// 已应用事件（按 seq 有序；apply 只追加，保证单调）。
    events: Vec<(u64, SessionEvent)>,
    history: Vec<ChatMessage>,
    plan: Option<Plan>,
    /// 投影是否与 events 同步（apply 后置脏，读时重投影）。
    dirty: bool,
}

impl ConversationProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 全量重建（等价于逐条 apply；属性测试 incremental == rebuild）。
    pub fn rebuild(events: &[(u64, SessionEvent)]) -> Self {
        let history = crate::store::project_messages(events);
        let plan = plan_from_events(events);
        Self {
            events: events.to_vec(),
            history,
            plan,
            dirty: false,
        }
    }

    /// 增量应用一条事件（O(1) 追加；标记投影脏，读时重投影）。
    pub fn apply(&mut self, seq: u64, event: SessionEvent) {
        self.events.push((seq, event));
        self.dirty = true;
    }

    /// 当前历史（读时惰性投影；与 rebuild 等价）。
    pub fn history(&mut self) -> &[ChatMessage] {
        self.refresh_if_dirty();
        &self.history
    }

    /// 当前 plan（读时惰性投影）。
    pub fn plan(&mut self) -> Option<&Plan> {
        self.refresh_if_dirty();
        self.plan.as_ref()
    }

    /// 已应用事件数。
    pub fn applied(&self) -> usize {
        self.events.len()
    }

    /// 从外部注入的完整 context 构造投影（`Conversation::accept_context`
    /// 用：AgentOutcome.messages 是完整 context，非增量）。事件缓冲为空
    /// （外部 context 无法反向重建事件），下次 refresh_from_log 会覆盖。
    pub fn from_history(history: Vec<ChatMessage>, plan: Option<Plan>) -> Self {
        Self {
            events: Vec::new(),
            history,
            plan,
            dirty: false,
        }
    }

    /// 惰性重投影：仅当 apply 后第一次读取时执行（等价全量 rebuild）。
    fn refresh_if_dirty(&mut self) {
        if self.dirty {
            let events = std::mem::take(&mut self.events);
            let rebuilt = Self::rebuild(&events);
            self.events = events;
            self.history = rebuilt.history;
            self.plan = rebuilt.plan;
            self.dirty = false;
        }
    }
}

/// 从事件序列重建 plan（纯函数）。
pub fn plan_from_events(events: &[(u64, SessionEvent)]) -> Option<Plan> {
    events.iter().rev().find_map(|(_, event)| match event {
        SessionEvent::PlanReplaced { plan } => Some(plan.clone()),
        _ => None,
    })
}

/// 兼容引用（compacted_range 供 project_domain_messages 内部使用，此处避免
/// 直接 import 造成未用告警；实际投影逻辑在 store 模块）。
#[allow(unused)]
fn _compaction_reference(events: &[(u64, SessionEvent)]) {
    let _ = compacted_range(events);
}
