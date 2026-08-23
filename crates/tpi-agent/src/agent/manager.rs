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

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;
use tpi_capabilities::tool::scheduler::GlobalEffectScheduler;
use tpi_core::ids::{AgentId, DelegationId, RequestId, SessionId};

/// Orchestration limits for the shared agent graph. These are policy limits,
/// not capability filters: every runtime still receives the same tool set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentLimits {
    pub max_depth: usize,
    pub max_concurrent_agents: usize,
    pub max_total_agents: usize,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_concurrent_agents: 64,
            max_total_agents: 64,
        }
    }
}

/// 被托管 agent 的状态机（ADR-007 §2.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Allocated in the graph but not scheduled yet.
    Created,
    /// 已注册（agent_id 已分配，worker 尚未进入 AgentLoop）。
    Starting,
    /// worker AgentLoop 执行中。
    Running,
    /// Waiting for a tool result.
    WaitingTool,
    /// Waiting for an interaction routed through the graph.
    WaitingInput,
    /// Normal terminal state.
    Completed,
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
            AgentState::Created => "created",
            AgentState::Starting => "starting",
            AgentState::Running => "running",
            AgentState::WaitingTool => "waiting_tool",
            AgentState::WaitingInput => "waiting_input",
            AgentState::Completed => "completed",
            AgentState::Stopped => "stopped",
            AgentState::Failed => "failed",
            AgentState::Cancelled => "cancelled",
        }
    }

    fn is_active(self) -> bool {
        matches!(
            self,
            Self::Created | Self::Starting | Self::Running | Self::WaitingTool | Self::WaitingInput
        )
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Stopped | Self::Completed | Self::Failed | Self::Cancelled
        )
    }
}

/// The context owned by one runtime. It deliberately contains no parent
/// conversation; agents exchange only delegation results/mailbox messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentContext {
    pub session_id: SessionId,
    pub instruction: String,
}

/// Runtime budget/policy snapshot. Limits are orchestration policy, not tool
/// capability filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentBudget {
    pub max_depth: usize,
    pub max_concurrent_agents: usize,
    pub max_total_agents: usize,
}

/// Ownership identity attached to process/terminal effects created by an
/// agent. Resource registries remain shared at session scope, but every effect
/// has an unambiguous graph owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessOwner {
    pub agent_id: AgentId,
}

/// The uniform runtime entity represented by every node in the graph.
///
/// The execution loop remains in `agent::run`; this value is the lifecycle and
/// ownership boundary around it. Parent/child is represented only by
/// `parent_agent_id`, never by a different capability type.
#[derive(Clone)]
pub struct AgentRuntime {
    pub agent_id: AgentId,
    pub parent_agent_id: Option<AgentId>,
    pub session_id: SessionId,
    pub depth: usize,
    pub context: AgentContext,
    pub tools: Arc<std::sync::Mutex<tpi_capabilities::tool::registry::ToolRegistry>>,
    pub budget: AgentBudget,
    pub process_owner: ProcessOwner,
    pub state: AgentState,
    pub cancellation: CancellationToken,
}

/// A graph-routed interaction request. The answer is delivered to the waiting
/// agent, never to the ordinary parent prompt by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionRequest {
    pub request_id: RequestId,
    pub agent_id: AgentId,
    pub parent_agent_id: Option<AgentId>,
    pub question: String,
}

/// A graph-routed mailbox message. Mailboxes are deliberately separate from
/// conversation history: delivery never splices a sibling's context into the
/// recipient's model input unless the recipient explicitly reads the mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessage {
    pub message_id: RequestId,
    pub from_agent_id: Option<AgentId>,
    pub to_agent_id: AgentId,
    pub body: String,
}

struct PendingInteraction {
    request: InteractionRequest,
    answer: tokio::sync::oneshot::Sender<String>,
}

/// InteractionRouter is intentionally small: it owns routing and cancellation
/// of pending answers, while UI/runtime adapters decide how to present them.
#[derive(Default)]
pub struct InteractionRouter {
    pending: HashMap<RequestId, PendingInteraction>,
    by_agent: HashMap<AgentId, RequestId>,
}

impl InteractionRouter {
    pub fn request(
        &mut self,
        agent_id: AgentId,
        parent_agent_id: Option<AgentId>,
        question: String,
    ) -> (InteractionRequest, tokio::sync::oneshot::Receiver<String>) {
        let request = InteractionRequest {
            request_id: RequestId::new_v7(),
            agent_id,
            parent_agent_id,
            question,
        };
        let (answer, receiver) = tokio::sync::oneshot::channel();
        self.by_agent.insert(agent_id, request.request_id);
        self.pending.insert(
            request.request_id,
            PendingInteraction {
                request: request.clone(),
                answer,
            },
        );
        (request, receiver)
    }

    pub fn answer(
        &mut self,
        request_id: RequestId,
        answer: String,
    ) -> Result<InteractionRequest, String> {
        let Some(pending) = self.pending.remove(&request_id) else {
            return Err("interaction request not found or already answered".into());
        };
        self.by_agent.remove(&pending.request.agent_id);
        pending
            .answer
            .send(answer)
            .map_err(|_| "agent is no longer waiting for input".to_string())?;
        Ok(pending.request)
    }

    pub fn answer_agent(
        &mut self,
        agent_id: AgentId,
        answer: String,
    ) -> Result<InteractionRequest, String> {
        let request_id = self
            .by_agent
            .get(&agent_id)
            .copied()
            .ok_or_else(|| "agent has no pending input request".to_string())?;
        self.answer(request_id, answer)
    }

    pub fn cancel_agent(&mut self, agent_id: AgentId) {
        if let Some(request_id) = self.by_agent.remove(&agent_id) {
            self.pending.remove(&request_id);
        }
    }

    pub fn pending_for(&self, agent_id: AgentId) -> Option<InteractionRequest> {
        self.by_agent
            .get(&agent_id)
            .and_then(|id| self.pending.get(id))
            .map(|p| p.request.clone())
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
    pub child_session: SessionId,
    pub summary: String,
    pub evidence: Vec<String>,
    /// false = progress 进度；true = 首个 final 报告。
    pub final_report: bool,
}

/// 某个 agent 的外部可查询视图（list/status 输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    pub agent_id: AgentId,
    pub parent_agent_id: Option<AgentId>,
    pub depth: usize,
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
    parent_agent_id: Option<AgentId>,
    depth: usize,
    child_session: SessionId,
    instruction: String,
    last_summary: Option<String>,
    updated_at: u64,
    /// child 取消 token：由 external worker 在 spawn 时登记，cancel 用。
    cancel: CancellationToken,
    /// **per-agent** 唤醒原语：settle/report 时 `notify_waiters`。wait 侧在
    /// 短暂锁内快照此 Arc，释放锁后再 `notified().await`——若唤醒发生在
    /// 快照之后、注册 waiter 之前，`notify_waiters()` 会丢失（无 waiter 可醒），
    /// 但共享单 Notify 同样丢失且会误醒其他 agent 的 waiter；per-agent 实例
    /// 至少保证不会因其他 agent 的 settle 而虚假返回，配合 wait 循环顶部重查
    /// 终态（check → await → re-check）消除丢失唤醒：只要状态变化发生在
    /// 快照前，循环顶部的终态检查就能直接返回；发生在快照后则 notify_waiters
    /// 必然命中已注册的 waiter（快照与注册之间无 await 点）。
    notify: Arc<tokio::sync::Notify>,
}

/// The graph is the manager's actual responsibility; this name is exported so
/// orchestration code does not have to model parent/child as separate classes.
pub type AgentGraph = AgentManager;

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
    children: HashMap<AgentId, Vec<AgentId>>,
    limits: AgentLimits,
    /// Shared by every runtime in the session/workspace.
    effect_scheduler: Arc<GlobalEffectScheduler>,
    /// Worker handles are owned by the graph, so a child cannot become a
    /// detached task. Completed workers are removed by `complete_worker`;
    /// panicking/cancelled workers remain joinable for session shutdown.
    workers: HashMap<AgentId, tokio::task::JoinHandle<()>>,
    tool_registry: Option<Arc<std::sync::Mutex<tpi_capabilities::tool::registry::ToolRegistry>>>,
    workspace_session: Option<Arc<tpi_capabilities::workspace::session::WorkspaceSession>>,
    interaction_router: InteractionRouter,
    mailboxes: HashMap<AgentId, VecDeque<AgentMessage>>,
    /// 已落盘 durable event、但尚未投影到任何 parent model context 的 report。
    /// 易失唤醒缓存：durable SessionEvent 才是真相，崩溃后从事件流重建。
    inbox: Vec<PendingReport>,
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 内存中保留的终态 agent 记录上限（防无界增长；参照 ProcessRegistry 的
/// MAX_RETAINED_PROCESSES）。active agent 永不淘汰。
pub const MAX_RETAINED_AGENTS: usize = 64;

impl AgentManager {
    pub fn new() -> Self {
        Self::with_limits(AgentLimits::default())
    }

    pub fn with_limits(limits: AgentLimits) -> Self {
        Self {
            order: Vec::new(),
            agents: HashMap::new(),
            delegations: HashMap::new(),
            children: HashMap::new(),
            limits,
            effect_scheduler: Arc::new(GlobalEffectScheduler::new()),
            workers: HashMap::new(),
            tool_registry: Some(Arc::new(std::sync::Mutex::new(
                tpi_capabilities::tool::registry::builtin_registry(),
            ))),
            workspace_session: None,
            interaction_router: InteractionRouter::default(),
            mailboxes: HashMap::new(),
            inbox: Vec::new(),
        }
    }

    pub fn limits(&self) -> AgentLimits {
        self.limits
    }

    pub fn effect_scheduler(&self) -> Arc<GlobalEffectScheduler> {
        self.effect_scheduler.clone()
    }

    pub fn workspace_session(
        &self,
    ) -> Option<Arc<tpi_capabilities::workspace::session::WorkspaceSession>> {
        self.workspace_session.clone()
    }

    /// Install the session's active tool directory. Root and child runtimes
    /// point at this same Arc; no child-specific capability registry exists.
    pub fn set_tool_registry(
        &mut self,
        registry: Arc<std::sync::Mutex<tpi_capabilities::tool::registry::ToolRegistry>>,
    ) {
        // `new()` has a builtin fallback so direct manager users remain useful,
        // but the first real runtime must replace that fallback with the active
        // session directory (including MCP/overlay tools).
        self.tool_registry = Some(registry);
    }

    pub fn tool_registry(
        &self,
    ) -> Option<Arc<std::sync::Mutex<tpi_capabilities::tool::registry::ToolRegistry>>> {
        self.tool_registry.clone()
    }

    /// Construct the current runtime projection for graph/status consumers.
    pub fn runtime(&self, agent_id: AgentId) -> Option<AgentRuntime> {
        let record = self.agents.get(&agent_id)?;
        let tools = self.tool_registry.clone().unwrap_or_else(|| {
            Arc::new(std::sync::Mutex::new(
                tpi_capabilities::tool::registry::builtin_registry(),
            ))
        });
        Some(AgentRuntime {
            agent_id,
            parent_agent_id: record.parent_agent_id,
            session_id: record.child_session,
            depth: record.depth,
            context: AgentContext {
                session_id: record.child_session,
                instruction: record.instruction.clone(),
            },
            tools,
            budget: AgentBudget {
                max_depth: self.limits.max_depth,
                max_concurrent_agents: self.limits.max_concurrent_agents,
                max_total_agents: self.limits.max_total_agents,
            },
            process_owner: ProcessOwner { agent_id },
            state: record.state,
            cancellation: record.cancel.clone(),
        })
    }

    pub fn request_input(
        &mut self,
        agent_id: AgentId,
        question: String,
    ) -> Result<(InteractionRequest, tokio::sync::oneshot::Receiver<String>), String> {
        let (parent_agent_id, state) = self
            .agents
            .get(&agent_id)
            .map(|record| (record.parent_agent_id, record.state))
            .ok_or_else(|| "agent not found".to_string())?;
        if !state.is_active() {
            return Err(format!("agent is not active: {}", state.as_str()));
        }
        if self.interaction_router.pending_for(agent_id).is_some() {
            return Err("agent already has a pending input request".into());
        }
        let request = self
            .interaction_router
            .request(agent_id, parent_agent_id, question);
        if let Some(record) = self.agents.get_mut(&agent_id) {
            record.state = AgentState::WaitingInput;
            record.updated_at = Self::now();
        }
        Ok(request)
    }

    pub fn answer_input(
        &mut self,
        request_id: RequestId,
        answer: String,
    ) -> Result<InteractionRequest, String> {
        let request = self.interaction_router.answer(request_id, answer)?;
        if let Some(record) = self.agents.get_mut(&request.agent_id)
            && record.state == AgentState::WaitingInput
        {
            record.state = AgentState::Running;
            record.updated_at = Self::now();
        }
        Ok(request)
    }

    pub fn answer_agent(
        &mut self,
        agent_id: AgentId,
        answer: String,
    ) -> Result<InteractionRequest, String> {
        let request = self.interaction_router.answer_agent(agent_id, answer)?;
        if let Some(record) = self.agents.get_mut(&agent_id)
            && record.state == AgentState::WaitingInput
        {
            record.state = AgentState::Running;
            record.updated_at = Self::now();
        }
        Ok(request)
    }

    pub fn pending_input(&self, agent_id: AgentId) -> Option<InteractionRequest> {
        self.interaction_router.pending_for(agent_id)
    }

    /// Queue a message for a graph node. The recipient must already exist;
    /// there is no implicit conversation sharing or detached mailbox target.
    pub fn send_message(
        &mut self,
        from_agent_id: Option<AgentId>,
        to_agent_id: AgentId,
        body: String,
    ) -> Result<AgentMessage, String> {
        if !self.agents.contains_key(&to_agent_id) {
            return Err(format!("agent not found: {to_agent_id}"));
        }
        if let Some(from) = from_agent_id
            && !self.agents.contains_key(&from)
        {
            return Err(format!("sender agent not found: {from}"));
        }
        if body.trim().is_empty() {
            return Err("message must not be empty".into());
        }
        let message = AgentMessage {
            message_id: RequestId::new_v7(),
            from_agent_id,
            to_agent_id,
            body,
        };
        self.mailboxes
            .entry(to_agent_id)
            .or_default()
            .push_back(message.clone());
        if let Some(record) = self.agents.get(&to_agent_id) {
            record.notify.notify_waiters();
        }
        Ok(message)
    }

    /// Drain only the addressed agent's mailbox. Messages are not projected
    /// into model context automatically, preserving context isolation.
    pub fn drain_mailbox(&mut self, agent_id: AgentId) -> Result<Vec<AgentMessage>, String> {
        if !self.agents.contains_key(&agent_id) {
            return Err(format!("agent not found: {agent_id}"));
        }
        Ok(self
            .mailboxes
            .remove(&agent_id)
            .map(|messages| messages.into_iter().collect())
            .unwrap_or_default())
    }

    pub fn set_workspace_session(
        &mut self,
        workspace_session: Arc<tpi_capabilities::workspace::session::WorkspaceSession>,
    ) {
        if self.workspace_session.is_none() {
            self.workspace_session = Some(workspace_session);
        }
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
        self.register_child(child_session, instruction, cancel, None)
    }

    /// Ensure the session's root is represented by the same runtime entity as
    /// every descendant. The root is stable across serialized runs, while its
    /// run-scoped cancellation token is refreshed for the current run.
    pub fn ensure_root(
        &mut self,
        session_id: SessionId,
        cancel: CancellationToken,
    ) -> Result<AgentId, String> {
        if let Some(agent_id) = self.agents.iter().find_map(|(id, record)| {
            (record.child_session == session_id && record.parent_agent_id.is_none()).then_some(*id)
        }) {
            let record = self
                .agents
                .get_mut(&agent_id)
                .expect("root found by the same map");
            record.cancel = cancel;
            record.state = AgentState::Running;
            record.updated_at = Self::now();
            return Ok(agent_id);
        }
        let agent_id = self.register(session_id, "root runtime".into(), cancel)?;
        self.mark_running(agent_id);
        Ok(agent_id)
    }

    /// Register any runtime in the graph. Parent and child use the same path;
    /// the edge is the only relationship-specific data.
    pub fn register_child(
        &mut self,
        child_session: SessionId,
        instruction: String,
        cancel: CancellationToken,
        parent_agent_id: Option<AgentId>,
    ) -> Result<AgentId, String> {
        let active = self.agents.values().filter(|a| a.state.is_active()).count();
        if active >= self.limits.max_concurrent_agents {
            return Err(format!(
                "agent_concurrency_limit: active agents limit {} reached",
                self.limits.max_concurrent_agents
            ));
        }
        if self.agents.len() >= self.limits.max_total_agents {
            return Err(format!(
                "agent_total_limit: total agents limit {} reached",
                self.limits.max_total_agents
            ));
        }
        let depth = if let Some(parent) = parent_agent_id {
            let parent_record = self
                .agents
                .get(&parent)
                .ok_or_else(|| format!("agent_parent_not_found: {parent}"))?;
            if !parent_record.state.is_active() {
                return Err(format!(
                    "agent_parent_not_active: {}",
                    parent_record.state.as_str()
                ));
            }
            parent_record.depth + 1
        } else {
            0
        };
        if depth > self.limits.max_depth {
            return Err(format!(
                "agent_depth_limit: max depth {} reached",
                self.limits.max_depth
            ));
        }
        let agent_id = AgentId::new_v7();
        self.agents.insert(
            agent_id,
            AgentRecord {
                state: AgentState::Starting,
                parent_agent_id,
                depth,
                child_session,
                instruction,
                last_summary: None,
                updated_at: Self::now(),
                cancel,
                notify: Arc::new(tokio::sync::Notify::new()),
            },
        );
        self.order.push(agent_id);
        if let Some(parent) = parent_agent_id {
            self.children.entry(parent).or_default().push(agent_id);
        }
        Ok(agent_id)
    }

    /// Resolve the graph node owning a runtime/session. The mapping keeps
    /// agent identity out of the model-visible tool protocol.
    pub fn agent_for_session(&self, session_id: SessionId) -> Option<AgentId> {
        self.agents
            .iter()
            .find_map(|(id, record)| (record.child_session == session_id).then_some(*id))
    }

    pub fn delegation_for_agent(&self, agent_id: AgentId) -> Option<DelegationId> {
        self.delegations
            .values()
            .find_map(|delegation| (delegation.child_agent == agent_id).then_some(delegation.id))
    }

    pub fn register_worker(
        &mut self,
        agent_id: AgentId,
        worker: tokio::task::JoinHandle<()>,
    ) -> Result<(), (String, tokio::task::JoinHandle<()>)> {
        if !self.agents.contains_key(&agent_id) {
            return Err(("agent not found".into(), worker));
        }
        self.workers.insert(agent_id, worker);
        Ok(())
    }

    /// Remove a worker handle after its task has completed its final state
    /// write. This is called by the worker itself only after no await points
    /// remain, so dropping the already-complete handle does not detach work.
    pub fn complete_worker(&mut self, agent_id: AgentId) {
        self.workers.remove(&agent_id);
    }

    /// Cancel all agents and join every owned worker. The caller must invoke
    /// this at session shutdown, outside any mutex guard.
    pub async fn shutdown_and_join(manager: Arc<std::sync::Mutex<AgentManager>>) {
        let workers = {
            let mut guard = manager.lock().unwrap_or_else(|p| p.into_inner());
            guard.shutdown();
            std::mem::take(&mut guard.workers)
        };
        for worker in workers.into_values() {
            let _ = worker.await;
        }
    }

    /// 登记一次委托（spawn 时创建）。
    pub fn add_delegation(&mut self, delegation: Delegation) {
        self.delegations.insert(delegation.id, delegation);
    }

    /// worker 已进入 AgentLoop（Starting → Running）。
    pub fn mark_running(&mut self, agent_id: AgentId) -> Option<()> {
        let r = self.agents.get_mut(&agent_id)?;
        if matches!(r.state, AgentState::Starting | AgentState::WaitingTool) {
            r.state = AgentState::Running;
            r.updated_at = Self::now();
        }
        Some(())
    }

    pub fn mark_waiting_tool(&mut self, agent_id: AgentId) -> Option<()> {
        let r = self.agents.get_mut(&agent_id)?;
        if r.state == AgentState::Running {
            r.state = AgentState::WaitingTool;
            r.updated_at = Self::now();
        }
        Some(())
    }

    /// 登记一次 report（已落盘 durable event 后调用）。写入 inbox 并唤醒 wait。
    pub fn report(&mut self, delegation_id: DelegationId, report: PendingReport) {
        let notify = self.notify_of(report.agent_id);
        if let Some(d) = self.delegations.get_mut(&delegation_id) {
            d.state = DelegationState::Reported;
        }
        if let Some(r) = self.agents.get_mut(&report.agent_id) {
            r.last_summary = Some(report.summary.clone());
            r.updated_at = Self::now();
        }
        self.inbox.push(report);
        // 持有锁时调用：notify_waiters 只唤醒已注册 waiter，无 waiter 时无害。
        // wait 侧在锁内快照同一 Arc 后才释放锁去 await，不会在此间隙错过。
        notify.notify_waiters();
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
        let notify = self.notify_of(agent_id);
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
        // 持锁调用（见 report）：唤醒该 agent 的 waiter，不误醒其他 agent。
        notify.notify_waiters();
    }

    /// 取指定 agent 的 per-agent Notify（agent 不存在时返回孤立实例，无害）。
    fn notify_of(&self, agent_id: AgentId) -> Arc<tokio::sync::Notify> {
        self.agents
            .get(&agent_id)
            .map(|r| r.notify.clone())
            .unwrap_or_default()
    }

    /// 取消一个 agent（cancel token；等待 worker 协作退出，非强行 kill）。
    pub fn cancel(&mut self, agent_id: AgentId) -> Result<(), String> {
        if !self.agents.contains_key(&agent_id) {
            return Err("agent not found".into());
        }
        let mut pending = vec![agent_id];
        while let Some(current) = pending.pop() {
            pending.extend(self.children.get(&current).cloned().unwrap_or_default());
            if let Some(record) = self.agents.get_mut(&current)
                && record.state.is_active()
            {
                record.cancel.cancel();
            }
            self.interaction_router.cancel_agent(current);
        }
        Ok(())
    }

    /// 清理一个终态 agent（cancel + 移除记录）。active agent 不能被提前
    /// 删除：worker 仍可能递归 spawn 或写回状态，静默移除会制造孤儿任务。
    pub fn close(&mut self, agent_id: AgentId) -> Result<(), String> {
        self.cancel(agent_id)?;
        if let Some(record) = self.agents.get(&agent_id)
            && record.state.is_active()
        {
            return Err(
                "agent still active; cancel/wait for its terminal state before close".into(),
            );
        }
        let mut to_remove = Vec::new();
        let mut pending = vec![agent_id];
        while let Some(current) = pending.pop() {
            to_remove.push(current);
            pending.extend(self.children.get(&current).cloned().unwrap_or_default());
        }
        if to_remove.iter().any(|id| {
            self.agents
                .get(id)
                .is_some_and(|record| record.state.is_active())
        }) {
            return Err("agent tree still active; wait for all descendants before close".into());
        }
        for id in &to_remove {
            self.agents.remove(id);
            self.children.remove(id);
            self.mailboxes.remove(id);
        }
        self.order.retain(|id| !to_remove.contains(id));
        for children in self.children.values_mut() {
            children.retain(|id| !to_remove.contains(id));
        }
        Ok(())
    }

    /// 列出全部 agent（按插入顺序）。active 永在；终态只保留最近
    /// [`MAX_RETAINED_AGENTS`] 个（防无限增长）。
    pub fn list(&self) -> Vec<AgentView> {
        // 先收集 active 数量，终态从后往前保留，直到凑满上限。
        let active_count = self.agents.values().filter(|a| a.state.is_active()).count();
        let mut retained = 0usize;
        let capacity = MAX_RETAINED_AGENTS.saturating_sub(active_count);
        let mut out = Vec::with_capacity(self.agents.len());
        for id in &self.order {
            let Some(a) = self.agents.get(id) else {
                continue;
            };
            let active = a.state.is_active();
            if !active && retained >= capacity {
                continue;
            }
            if !active {
                retained += 1;
            }
            out.push(AgentView {
                agent_id: *id,
                parent_agent_id: a.parent_agent_id,
                depth: a.depth,
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
            parent_agent_id: a.parent_agent_id,
            depth: a.depth,
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
            // 短暂锁内完成：终态检查 + per-agent notify 快照。释放锁后立即
            // 注册 waiter（无中间 await），settle/report 的持锁 notify_waiters
            // 要么发生在快照前（循环顶部终态检查已覆盖），要么命中本 waiter——
            // 丢失唤醒窗口不存在。
            let notify = {
                let guard = manager.lock().ok()?;
                match guard.agents.get(&agent_id) {
                    Some(a) if a.state.is_terminal() => {
                        return Some(());
                    }
                    None => return None,
                    _ => {}
                }
                guard.notify_of(agent_id)
            };
            tokio::select! {
                _ = notify.notified() => {}
                _ = cancel.cancelled() => return None,
            }
            // 醒来后回到循环顶部重新检查是否终态（report 唤醒 ≠ 终态）。
        }
    }

    /// 会话关闭：取消全部 active agent（SessionScoped 生命周期终结）。
    pub fn shutdown(&mut self) {
        // 先取消全部 active（worker 协作退出），再逐 agent 唤醒 waiter
        //（wait 侧循环顶部重查终态：cancel 后 settle(Cancelled) 也会 notify）。
        let notifies: Vec<Arc<tokio::sync::Notify>> = self
            .agents
            .values()
            .filter(|r| r.state.is_active())
            .map(|r| {
                r.cancel.cancel();
                r.notify.clone()
            })
            .collect();
        for notify in notifies {
            notify.notify_waiters();
        }
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
                child_session: SessionId::new_v7(),
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

    #[test]
    fn settlement_remains_terminal_after_failure_report() {
        let mut manager = AgentManager::new();
        let agent = manager
            .register(
                SessionId::new_v7(),
                "fails".into(),
                CancellationToken::new(),
            )
            .unwrap();
        let delegation = DelegationId::new_v7();
        manager.add_delegation(Delegation {
            id: delegation,
            child_agent: agent,
            child_session: SessionId::new_v7(),
            state: DelegationState::Running,
        });
        manager.report(
            delegation,
            PendingReport {
                delegation_id: delegation,
                agent_id: agent,
                child_session: SessionId::new_v7(),
                summary: "failed".into(),
                evidence: vec![],
                final_report: true,
            },
        );
        manager.settle(delegation, agent, AgentState::Failed, None);
        assert_eq!(
            manager.delegations.get(&delegation).unwrap().state,
            DelegationState::Settled
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

    /// 回归：wait 先进入（终态未发生）→ settle 在“释放锁后、注册 waiter 前”
    /// 的窗口完成 → wait 仍必须在有陘时间内返回（丢失唤醒竞态，修复前
    /// notify_waiters 蒸发后挂到超时）。并发压力下循环验证。
    #[tokio::test]
    async fn wait_wakes_even_when_settle_races_before_waiter_registration() {
        for _ in 0..50 {
            let mut m = AgentManager::new();
            let aid = m
                .register(SessionId::new_v7(), "竞态".into(), CancellationToken::new())
                .unwrap();
            let did = DelegationId::new_v7();
            m.add_delegation(Delegation {
                id: did,
                child_agent: aid,
                child_session: SessionId::new_v7(),
                state: DelegationState::Running,
            });
            let manager = Arc::new(std::sync::Mutex::new(m));
            let m2 = manager.clone();
            let worker = tokio::spawn(async move {
                // 立即 settle：大概率落在 wait 侧快照与注册 waiter 的窗口。
                m2.lock()
                    .unwrap()
                    .settle(did, aid, AgentState::Stopped, None);
            });
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                AgentManager::wait(&manager, aid, CancellationToken::new()),
            )
            .await
            .expect("wait 必须在 2s 内返回（丢失唤醒）");
            assert!(result.is_some());
            worker.await.unwrap();
        }
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
        m.settle(DelegationId::new_v7(), id, AgentState::Stopped, None);
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

    #[test]
    fn cancelling_parent_cancels_the_entire_descendant_tree() {
        let mut manager = AgentManager::with_limits(AgentLimits {
            max_depth: 4,
            max_concurrent_agents: 8,
            max_total_agents: 8,
        });
        let parent_token = CancellationToken::new();
        let parent = manager
            .register(SessionId::new_v7(), "parent".into(), parent_token.clone())
            .unwrap();
        let child_token = CancellationToken::new();
        let child = manager
            .register_child(
                SessionId::new_v7(),
                "child".into(),
                child_token.clone(),
                Some(parent),
            )
            .unwrap();
        let grandchild_token = CancellationToken::new();
        manager
            .register_child(
                SessionId::new_v7(),
                "grandchild".into(),
                grandchild_token.clone(),
                Some(child),
            )
            .unwrap();

        manager.cancel(parent).unwrap();
        assert!(parent_token.is_cancelled());
        assert!(child_token.is_cancelled());
        assert!(grandchild_token.is_cancelled());
    }

    #[test]
    fn depth_policy_is_runtime_error_not_capability_filter() {
        let mut manager = AgentManager::with_limits(AgentLimits {
            max_depth: 1,
            max_concurrent_agents: 8,
            max_total_agents: 8,
        });
        let root = manager
            .register(SessionId::new_v7(), "root".into(), CancellationToken::new())
            .unwrap();
        let child = manager
            .register_child(
                SessionId::new_v7(),
                "child".into(),
                CancellationToken::new(),
                Some(root),
            )
            .unwrap();
        let error = manager
            .register_child(
                SessionId::new_v7(),
                "grandchild".into(),
                CancellationToken::new(),
                Some(child),
            )
            .unwrap_err();
        assert!(error.contains("agent_depth_limit"));
    }

    #[test]
    fn runtime_projection_has_uniform_tools_and_budget() {
        let mut m = AgentManager::with_limits(AgentLimits {
            max_depth: 3,
            max_concurrent_agents: 4,
            max_total_agents: 5,
        });
        let id = m
            .register(SessionId::new_v7(), "task".into(), CancellationToken::new())
            .unwrap();
        let runtime = m.runtime(id).expect("runtime projection");
        assert_eq!(runtime.budget.max_depth, 3);
        assert_eq!(runtime.process_owner.agent_id, id);
        assert!(runtime.tools.lock().unwrap().get("edit").is_some());
    }

    #[test]
    fn active_registry_replaces_builtin_fallback_for_all_runtimes() {
        let mut manager = AgentManager::new();
        let active = Arc::new(std::sync::Mutex::new(
            tpi_capabilities::tool::registry::ToolRegistry::new(),
        ));
        manager.set_tool_registry(active.clone());
        let id = manager
            .register(SessionId::new_v7(), "task".into(), CancellationToken::new())
            .unwrap();
        assert!(Arc::ptr_eq(&manager.runtime(id).unwrap().tools, &active));
    }

    #[test]
    fn mailbox_routes_messages_without_sharing_context() {
        let mut manager = AgentManager::new();
        let sender = manager
            .register(
                SessionId::new_v7(),
                "sender".into(),
                CancellationToken::new(),
            )
            .unwrap();
        let receiver = manager
            .register_child(
                SessionId::new_v7(),
                "receiver".into(),
                CancellationToken::new(),
                Some(sender),
            )
            .unwrap();
        let message = manager
            .send_message(Some(sender), receiver, "keep config.rs unchanged".into())
            .unwrap();
        assert_eq!(message.to_agent_id, receiver);
        assert_eq!(manager.drain_mailbox(receiver).unwrap(), vec![message]);
        assert!(manager.drain_mailbox(receiver).unwrap().is_empty());
    }

    #[test]
    fn child_registration_rejects_unknown_parent() {
        let mut manager = AgentManager::new();
        let error = manager
            .register_child(
                SessionId::new_v7(),
                "orphan".into(),
                CancellationToken::new(),
                Some(AgentId::new_v7()),
            )
            .unwrap_err();
        assert!(error.contains("agent_parent_not_found"));
    }

    #[test]
    fn root_runtime_anchors_child_parent_edge() {
        let mut manager = AgentManager::new();
        let session = SessionId::new_v7();
        let root = manager
            .ensure_root(session, CancellationToken::new())
            .unwrap();
        let child = manager
            .register_child(
                SessionId::new_v7(),
                "child".into(),
                CancellationToken::new(),
                Some(root),
            )
            .unwrap();
        assert_eq!(manager.status(child).unwrap().parent_agent_id, Some(root));
        assert_eq!(manager.agent_for_session(session), Some(root));
    }

    #[tokio::test]
    async fn interaction_router_delivers_only_to_target_agent() {
        let mut m = AgentManager::new();
        let id = m
            .register(
                SessionId::new_v7(),
                "needs answer".into(),
                CancellationToken::new(),
            )
            .unwrap();
        let manager = Arc::new(std::sync::Mutex::new(m));
        let (request, receiver) = manager
            .lock()
            .unwrap()
            .request_input(id, "which option?".into())
            .unwrap();
        assert_eq!(
            manager.lock().unwrap().status(id).unwrap().state,
            AgentState::WaitingInput
        );
        manager
            .lock()
            .unwrap()
            .answer_input(request.request_id, "option-a".into())
            .unwrap();
        assert_eq!(receiver.await.unwrap(), "option-a");
        assert!(manager.lock().unwrap().pending_input(id).is_none());
    }
}
