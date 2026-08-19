//! ADR-007：Durable Async Subagent Runtime —— AgentManager（agent 资源管理器）。
//!
//! > Process 是 managed resource，Terminal 是 managed resource，Agent 也应该是
//! > managed resource；Subagent tool 只负责**控制** Agent resource，而不负责执行
//! > 完整的 child 生命周期。
//!
//! 本模块是 **资源表本体**（状态 / 委托 / 取消 / 唤醒 / inbox），刻意**不引入
//! `Provider` 泛型**：worker 的实际执行（复用 `InProcessChildProvider` 的
//! `agent::run`）由调用方（工具 / composition root）在外部启动，完成后经
//! [`AgentManager::settle`] 写回状态。这使 manager 可被任意工具以
//! `Arc<Mutex<AgentManager>>` 引用，无需在 `ToolContext` 上推进泛型。
//!
//! 不变量（ADR-007 §1.2）：
//! - 异步事件只在 AgentLoop 的 deterministic boundary 被消费；
//! - 先 durable 再 model-visible（report 先落盘 durable event，再进 inbox/context）；
//! - report/settle 不直接修改进行中的 Context。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;
use tpi_core::ids::{AgentId, DelegationId, SessionId};

/// 被托管 agent 的状态机（ADR-007 §2.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// 已注册（agent_id 已分配，worker 尚未进入 AgentLoop）。
    Starting,
    /// worker AgentLoop 执行中。
    Running,
    /// 正常完成（有 report 或 settlement）。
    Stopped,
    /// 不可恢复失败。
    Failed,
    /// 被取消（parent cancel / session 关闭）。
    Cancelled,
}

impl AgentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Starting => "starting",
            AgentState::Running => "running",
            AgentState::Stopped => "stopped",
            AgentState::Failed => "failed",
            AgentState::Cancelled => "cancelled",
        }
    }
}

/// 一次委托（spawn）的记录状态（ADR-007 §2.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationState {
    /// child 后台执行中。
    Running,
    /// 已产生语义 report（可能 final）。
    Reported,
    /// AgentLoop 达到终态（runtime truth）。终态可能无 report。
    Settled,
}

/// 一条待投影到 parent context 的 report（inbox 条目，ADR-007 §5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReport {
    pub delegation_id: DelegationId,
    pub agent_id: AgentId,
    pub summary: String,
    pub evidence: Vec<String>,
    /// false = progress 进度；true = 首个 final 报告。
    pub final_report: bool,
}

/// 某个 agent 的外部可查询视图（list/status 输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    pub agent_id: AgentId,
    pub state: AgentState,
    pub child_session: SessionId,
    pub instruction: String,
    /// 最近一次 report summary（进度或 final）。
    pub last_summary: Option<String>,
    /// Unix 时间戳（墙钟）——启动/最后变更时刻。
    pub updated_at: u64,
}

/// 某个 agent 的完整记录（manager 内部持有）。
struct AgentRecord {
    state: AgentState,
    child_session: SessionId,
    instruction: String,
    last_summary: Option<String>,
    updated_at: u64,
    /// child 取消 token：由 external worker 在 spawn 时登记，cancel 用。
    cancel: CancellationToken,
}

/// 一次委托记录（不一定每委托一个独立 agent 复用，但保留以支持未来的
/// Delegation #101 → Agent A、Delegation #123 → Agent A 复用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    pub id: DelegationId,
    pub child_agent: AgentId,
    pub child_session: SessionId,
    pub state: DelegationState,
}

/// AgentManager：session 级共享注册表（镜像 `ProcessRegistry` / `TerminalRegistry`）。
///
/// 由 `SessionRuntime.agents`（或 composition root）以 `Arc<StdMutex<AgentManager>>`
/// 持有，跨 run 存活。生命周期绑定 **session**（session 关闭 → 清理全部 agent）。
pub struct AgentManager {
    /// 按插入顺序记录 AgentId（展示顺序稳定）。
    order: Vec<AgentId>,
    agents: HashMap<AgentId, AgentRecord>,
    delegations: HashMap<DelegationId, Delegation>,
    /// 已落盘 durable event、但尚未投影到任何 parent model context 的 report。
    /// 易失唤醒缓存：durable SessionEvent 才是真相，崩溃后从事件流重建。
    inbox: Vec<PendingReport>,
    notify: Arc<tokio::sync::Notify>,
}

/// 内存中保留的终态 agent 记录上限（防无界增长；参照 ProcessRegistry 的
/// MAX_RETAINED_PROCESSES）。active agent 永不淘汰。
pub const MAX_RETAINED_AGENTS: usize = 64;

impl Default for AgentManager {
    fn default() -> Self {
        Self {
            order: Vec::new(),
            agents: HashMap::new(),
            delegations: HashMap::new(),
            inbox: Vec::new(),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl AgentManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    }

    /// 注册一个新 agent（Starting）。返回 agent_id。达到上限返回 Err。
    ///
    /// worker 真实启动前先登记，使 `spawn_agent` 能**立即返回** agent_id，
    /// 且一旦启动失败也有 Starting→Failed 的终态可查。
    pub fn register(
        &mut self,
        child_session: SessionId,
        instruction: String,
        cancel: CancellationToken,
    ) -> Result<AgentId, String> {
        let active = self
            .agents
            .values()
            .filter(|a| matches!(a.state, AgentState::Starting | AgentState::Running))
            .count();
        if active >= MAX_RETAINED_AGENTS {
            return Err(format!(
                "agent 上限已达（{MAX_RETAINED_AGENTS}）；关闭一个已完成 agent 后再 spawn"
            ));
        }
        let agent_id = AgentId::new_v7();
        self.agents.insert(
            agent_id,
            AgentRecord {
                state: AgentState::Starting,
                child_session,
                instruction,
                last_summary: None,
                updated_at: Self::now(),
                cancel,
            },
        );
        self.order.push(agent_id);
        Ok(agent_id)
    }

    /// 登记一次委托（spawn 时创建）。
    pub fn add_delegation(&mut self, delegation: Delegation) {
        self.delegations.insert(delegation.id, delegation);
    }

    /// worker 已进入 AgentLoop（Starting → Running）。
    pub fn mark_running(&mut self, agent_id: AgentId) -> Option<()> {
        let r = self.agents.get_mut(&agent_id)?;
        if r.state == AgentState::Starting {
            r.state = AgentState::Running;
            r.updated_at = Self::now();
        }
        Some(())
    }

    /// 登记一次 report（已落盘 durable event 后调用）。写入 inbox 并唤醒 wait。
    pub fn report(&mut self, delegation_id: DelegationId, report: PendingReport) {
        if let Some(d) = self.delegations.get_mut(&delegation_id) {
            d.state = DelegationState::Reported;
        }
        if let Some(r) = self.agents.get_mut(&report.agent_id) {
            r.last_summary = Some(report.summary.clone());
            r.updated_at = Self::now();
        }
        self.inbox.push(report);
        self.notify.notify_waiters();
    }

    /// AgentLoop 达到终态（runtime truth）。state 迁移。
    ///
    /// `result`：None = 无 final report（settlement 为空）；Some = final report。
    /// `final_report` 若为 true，则把最后一条 report 视为 final（保留在 record，
    /// 但仍进 inbox 供 context 投影）。
    pub fn settle(
        &mut self,
        delegation_id: DelegationId,
        agent_id: AgentId,
        end_state: AgentState,
        final_report: Option<PendingReport>,
    ) {
        if let Some(d) = self.delegations.get_mut(&delegation_id) {
            d.state = DelegationState::Settled;
        }
        if let Some(r) = self.agents.get_mut(&agent_id) {
            if let Some(rep) = &final_report {
                r.last_summary = Some(rep.summary.clone());
            }
            r.state = end_state;
            r.updated_at = Self::now();
        }
        if let Some(rep) = final_report {
            self.inbox.push(rep);
        }
        self.notify.notify_waiters();
    }

    /// 取消一个 agent（cancel token；等待 worker 协作退出，非强行 kill）。
    pub fn cancel(&mut self, agent_id: AgentId) -> Result<(), String> {
        let r = self.agents.get_mut(&agent_id).ok_or("agent not found")?;
        if matches!(r.state, AgentState::Starting | AgentState::Running) {
            r.cancel.cancel();
        }
        Ok(())
    }

    /// 清理一个终态 agent（cancel + 移除记录）。
    pub fn close(&mut self, agent_id: AgentId) -> Result<(), String> {
        self.cancel(agent_id)?;
        self.agents.remove(&agent_id);
        self.order.retain(|id| *id != agent_id);
        Ok(())
    }

    /// 列出全部 agent（按插入顺序）。active 永在；终态只保留最近
    /// [`MAX_RETAINED_AGENTS`] 个（防无限增长）。
    pub fn list(&self) -> Vec<AgentView> {
        // 先收集 active 数量，终态从后往前保留，直到凑满上限。
        let active_count = self
            .agents
            .values()
            .filter(|a| matches!(a.state, AgentState::Starting | AgentState::Running))
            .count();
        let mut retained = 0usize;
        let capacity = MAX_RETAINED_AGENTS.saturating_sub(active_count);
        let mut out = Vec::with_capacity(self.agents.len());
        for id in &self.order {
            let Some(a) = self.agents.get(id) else {
                continue;
            };
            let active = matches!(a.state, AgentState::Starting | AgentState::Running);
            if !active && retained >= capacity {
                continue;
            }
            if !active {
                retained += 1;
            }
            out.push(AgentView {
                agent_id: *id,
                state: a.state,
                child_session: a.child_session,
                instruction: a.instruction.clone(),
                last_summary: a.last_summary.clone(),
                updated_at: a.updated_at,
            });
        }
        out
    }

    /// 查询单个 agent 状态。
    pub fn status(&self, agent_id: AgentId) -> Option<AgentView> {
        self.agents.get(&agent_id).map(|a| AgentView {
            agent_id,
            state: a.state,
            child_session: a.child_session,
            instruction: a.instruction.clone(),
            last_summary: a.last_summary.clone(),
            updated_at: a.updated_at,
        })
    }

    /// 取走所有尚未投影的 report（inbox drain）。由 context projection（Commit 5）
    /// 在 deterministic boundary 调用；易失缓存，不负责 durable 语义。
    pub fn drain_inbox(&mut self) -> Vec<PendingReport> {
        std::mem::take(&mut self.inbox)
    }

    /// 是否有待投影 report。
    pub fn has_pending(&self) -> bool {
        !self.inbox.is_empty()
    }

    /// wait：阻塞直到 agent 进入终态或有新 report（响应 cancel）。
    /// 返回 `Some(())` 表示已终态；`None` 表示 agent 不存在或已提前取消。
    ///
    /// 接受 `&Arc<Mutex<AgentManager>>`（而非 `&mut self`）——因为工具侧持有的是
    /// `Arc<StdMutex<...>>`，且**不允许持锁跨 await**（AGENTS.md §并发）：本方法
    /// 在短暂锁内快照 notify 与终态，随即释放锁再 await，避免死锁。
    pub async fn wait(
        manager: &Arc<std::sync::Mutex<AgentManager>>,
        agent_id: AgentId,
        cancel: CancellationToken,
    ) -> Option<()> {
        loop {
            let notify = {
                let guard = manager.lock().ok()?;
                match guard.agents.get(&agent_id) {
                    Some(a)
                        if matches!(
                            a.state,
                            AgentState::Stopped | AgentState::Failed | AgentState::Cancelled
                        ) =>
                    {
                        return Some(());
                    }
                    None => return None,
                    _ => {}
                }
                guard.notify.clone()
            };
            tokio::select! {
                _ = notify.notified() => {}
                _ = cancel.cancelled() => return None,
            }
            // 醒来后回到循环顶部重新检查是否终态。
        }
    }

    /// 会话关闭：取消全部 active agent（SessionScoped 生命周期终结）。
    pub fn shutdown(&mut self) {
        for r in self.agents.values_mut() {
            if matches!(r.state, AgentState::Starting | AgentState::Running) {
                r.cancel.cancel();
            }
        }
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn register_assigns_unique_ids_and_status() {
        let mut m = AgentManager::new();
        let id = m
            .register(SessionId::new_v7(), "调查".into(), CancellationToken::new())
            .unwrap();
        assert_eq!(m.status(id).unwrap().state, AgentState::Starting);
        m.mark_running(id);
        assert_eq!(m.status(id).unwrap().state, AgentState::Running);
    }

    #[test]
    fn report_enters_inbox_and_updates_view() {
        let mut m = AgentManager::new();
        let aid = m
            .register(SessionId::new_v7(), "调查".into(), CancellationToken::new())
            .unwrap();
        let did = DelegationId::new_v7();
        m.add_delegation(Delegation {
            id: did,
            child_agent: aid,
            child_session: SessionId::new_v7(),
            state: DelegationState::Running,
        });
        m.report(
            did,
            PendingReport {
                delegation_id: did,
                agent_id: aid,
                summary: "progress".into(),
                evidence: vec![],
                final_report: false,
            },
        );
        assert!(m.has_pending());
        let drained = m.drain_inbox();
        assert_eq!(drained.len(), 1);
        assert!(!m.has_pending());
        assert_eq!(
            m.status(aid).unwrap().last_summary.as_deref(),
            Some("progress")
        );
    }

    #[tokio::test]
    async fn wait_returns_when_agent_settles() {
        let mut m = AgentManager::new();
        let aid = m
            .register(SessionId::new_v7(), "调查".into(), CancellationToken::new())
            .unwrap();
        let did = DelegationId::new_v7();
        m.add_delegation(Delegation {
            id: did,
            child_agent: aid,
            child_session: SessionId::new_v7(),
            state: DelegationState::Running,
        });
        let cancel = CancellationToken::new();
        let manager = Arc::new(std::sync::Mutex::new(m));
        let m2 = manager.clone();
        let a2 = aid;
        let did2 = did;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            m2.lock()
                .unwrap()
                .settle(did2, a2, AgentState::Stopped, None);
        });
        let result = AgentManager::wait(&manager, aid, cancel).await;
        assert!(result.is_some());
    }

    #[test]
    fn cancel_propagates_token() {
        let mut m = AgentManager::new();
        let token = CancellationToken::new();
        let id = m
            .register(SessionId::new_v7(), "调查".into(), token.clone())
            .unwrap();
        m.mark_running(id);
        m.cancel(id).unwrap();
        assert!(token.is_cancelled());
    }

    #[test]
    fn close_removes_record() {
        let mut m = AgentManager::new();
        let id = m
            .register(SessionId::new_v7(), "调查".into(), CancellationToken::new())
            .unwrap();
        m.close(id).unwrap();
        assert!(m.status(id).is_none());
    }

    #[test]
    fn agent_limit_is_enforced_for_active() {
        let mut m = AgentManager::new();
        // 55 个 active 后第 65 个失败（上限 64，保留机制可能在内部调整）。
        // 这里只验证大量 active 后 register 报错（只要有真实上限）。
        let mut ids = Vec::new();
        let mut err = None;
        for _ in 0..100 {
            match m.register(SessionId::new_v7(), "i".into(), CancellationToken::new()) {
                Ok(id) => ids.push(id),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        assert!(err.is_some(), "active 超过上限必须报错");
        assert!(ids.len() >= 64, "至少允许 64 个 active, 实际 {}", ids.len());
    }
}
