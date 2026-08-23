//! ADR-007 Commit 3/4：非阻塞 subagent 工具集 + 后台 worker。
//!
//! - [`SpawnAgentTool`]：`spawn_agent` --注册 + 启动 worker，**毫秒级返回**
//!   agent_id（不等待 child 完成；主代理 turn 不阻塞）。
//! - [`AgentControlTool`]：`agent` 控制面（list/status/wait/cancel/message/mailbox），
//!   镜像 `process` 工具的多 action 模式。
//!
//! worker（child AgentLoop）由后台 task 执行，复用
//! [`InProcessChildProvider`]（独立 child session + shared tool registry + Fresh context）。
//! 完成时把 report/settlement 写回 [`AgentManager`]（inbox + notify）。
//!
//! durable 语义（ADR-007 §4.2）：worker **不直接写 parent SessionLog**（parent
//! session 是单写者，run 期间被独占）。durable event 落盘发生在 **parent run 的
//! deterministic boundary**（context projection 消费 inbox 前，先写
//! SubagentReported/Settled 事件再构建 context）--同临界区内"先 durable 再
//! model-visible"，且不破坏 JSONL 单写者不变量。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use tpi_capabilities::tool::ToolContext;
use tpi_capabilities::tool::registry::{Tool, ToolOrigin};
use tpi_core::ids::{AgentId, DelegationId, SessionId};
use tpi_core::outcome::ToolOutcome;
use tpi_session::{SessionEvent, SubagentFinishedReason};

use crate::agent::manager::{AgentManager, AgentState, Delegation, DelegationState, PendingReport};
use crate::provider::Provider;
use crate::subagent::child::InProcessChildProvider;
use crate::subagent::{SubagentProvider, SubagentRequest};

/// session 级共享 AgentManager 句柄（composition root 创建，跨 run 存活）。
pub type SharedAgentManager = Arc<Mutex<AgentManager>>;

/// `spawn_agent`：发起一次非阻塞 agent delegation（ADR-007）。
pub struct SpawnAgentTool<P>
where
    P: Provider + Send + 'static,
{
    config: Arc<tpi_config::config::Config>,
    make_provider: Arc<dyn Fn() -> P + Send + Sync>,
    manager: SharedAgentManager,
    /// P8-06：child 完成时发 SubagentReported 的通道（parent TUI summary card）。
    report_tx: Option<tokio::sync::mpsc::Sender<crate::agent::LiveEvent>>,
    _provider: std::marker::PhantomData<fn() -> P>,
}

impl<P> SpawnAgentTool<P>
where
    P: Provider + Send + 'static,
{
    pub fn new<F>(
        config: Arc<tpi_config::config::Config>,
        make_provider: F,
        manager: SharedAgentManager,
        report_tx: Option<tokio::sync::mpsc::Sender<crate::agent::LiveEvent>>,
    ) -> Self
    where
        F: Fn() -> P + Send + Sync + 'static,
    {
        Self {
            config,
            make_provider: Arc::new(make_provider),
            manager,
            report_tx,
            _provider: std::marker::PhantomData,
        }
    }
}

/// `spawn_agent` 参数。
#[derive(Debug, serde::Deserialize)]
struct SpawnAgentArgs {
    #[serde(default)]
    instruction: String,
}

/// 组装 spawn_agent 的立即返回文本（模型可见）。
fn spawn_reply(agent_id: AgentId, delegation_id: DelegationId, instruction: &str) -> String {
    format!(
        "status: spawned\nagent_id: {agent_id}\ndelegation_id: {delegation_id}\n\
instruction: {instruction}\n\
后台调查已启动（非阻塞）：继续你的其他工作，之后用 `agent` 工具 \
（action=status/list/wait）查询结果；完成的报告也会在下一次模型输入边界自动注入。"
    )
}

/// worker：后台执行 child 调查，终态写回 manager（inbox + notify）。
///
/// 不持有 parent session 写权；durable 落盘由 parent run boundary 完成。
#[allow(clippy::too_many_arguments)]
fn spawn_worker<P>(
    config: Arc<tpi_config::config::Config>,
    make_provider: Arc<dyn Fn() -> P + Send + Sync>,
    manager: SharedAgentManager,
    agent_id: AgentId,
    delegation_id: DelegationId,
    request: SubagentRequest,
    cancel: CancellationToken,
    registry: Arc<Mutex<tpi_capabilities::tool::registry::ToolRegistry>>,
    resources: Arc<tpi_capabilities::resource::ResourceManager>,
    output_tx: Option<tokio::sync::mpsc::Sender<tpi_capabilities::tool::ToolStreamEvent>>,
    parent_call_id: Option<tpi_core::ids::ToolCallId>,
    report_tx: Option<tokio::sync::mpsc::Sender<crate::agent::LiveEvent>>,
) -> tokio::task::JoinHandle<()>
where
    P: Provider + Send + 'static,
{
    let child_session = request.child_session;
    tokio::spawn(async move {
        let child_workspace = tpi_capabilities::workspace::ActiveWorkspace::local(
            tpi_capabilities::workspace::LocalWorkspace::new(
                config.workspace_root.clone(),
                config.allow_outside_workspace,
            ),
        );
        let mut child = InProcessChildProvider::<P, _>::new(
            {
                let make = make_provider.clone();
                move || (make)()
            },
            config.clone(),
            child_workspace,
        )
        .with_registry(registry)
        .with_agent_manager(manager.clone())
        .with_resource_manager(resources.clone());
        // P8-06：绑定实时观察通道（child 活动经此通道转发到 parent TUI 卡片）。
        child = child.with_report_tx(report_tx);
        if let Some(call_id) = parent_call_id {
            child = child.with_output_tx(output_tx, call_id);
        }
        let outcome = child.run_investigation(request, cancel).await;
        if let Err(error) = resources.cleanup_delegation(delegation_id).await {
            tracing::error!(%error, %delegation_id, "delegation resource cleanup was not fully confirmed");
        }
        // 终态写回：report（成功）或 Failed/Cancelled。
        let mut guard = manager.lock().unwrap_or_else(|p| p.into_inner());
        match outcome {
            Ok(report) => {
                guard.settle(
                    delegation_id,
                    agent_id,
                    // Keep the stable external settlement spelling for the
                    // existing protocol; `Completed` remains available in
                    // the unified runtime state machine for newer callers.
                    AgentState::Stopped,
                    Some(PendingReport {
                        delegation_id,
                        agent_id,
                        child_session,
                        summary: report.summary,
                        evidence: report.evidence,
                        final_report: true,
                    }),
                );
            }
            Err(message) => {
                let cancelled = message.contains("cancelled");
                // 失败也留一条 pending notice，让 parent boundary 可见失败原因。
                guard.report(
                    delegation_id,
                    PendingReport {
                        delegation_id,
                        agent_id,
                        child_session,
                        summary: format!("子代理失败: {message}"),
                        evidence: vec![],
                        final_report: true,
                    },
                );
                // Report must precede settlement: report() marks a delegation
                // as Reported, while the runtime terminal state is Settled.
                guard.settle(
                    delegation_id,
                    agent_id,
                    if cancelled {
                        AgentState::Cancelled
                    } else {
                        AgentState::Failed
                    },
                    None,
                );
            }
        }
        guard.complete_worker(agent_id);
    })
}

#[async_trait]
impl<P> Tool for SpawnAgentTool<P>
where
    P: Provider + Send + 'static,
{
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "发起一次**非阻塞** agent：立即返回 agent_id（不等待完成），child 拥有独立 \
         session/trace 和隔离上下文，但使用与当前 agent 相同的工具目录。主代理应继续其他工作，之后用 \
         `agent` 工具查询/等待结果；已完成的报告会在下一次模型输入边界自动注入。需要并行 \
         工作时连续发起多个 spawn_agent（每个独立 agent）；递归深度由 runtime policy 控制。"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "instruction": {
                    "type": "string",
                    "description": "单条 agent 指令；child 使用与 parent 相同的工具目录"
                }
            },
            "required": ["instruction"]
        })
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    /// spawn 只修改 agent graph，不访问 workspace。
    fn access_class(&self) -> tpi_capabilities::tool::registry::ToolAccessClass {
        tpi_capabilities::tool::registry::ToolAccessClass::Pure
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        let parsed: SpawnAgentArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return args_error(format!("spawn_agent 参数解析失败: {e}")),
        };
        if parsed.instruction.trim().is_empty() {
            return args_error("spawn_agent 需要非空 instruction".into());
        }
        let delegation_id = DelegationId::new_v7();
        let child_session = SessionId::new_v7();
        // A child token is linked to the parent run and is also the exact
        // token stored in AgentManager. Cancelling through either path now
        // reaches the worker (the previous code registered a different token).
        let child_cancel = ctx.cancel.child_token();

        let parent_agent_id = SessionId::parse_str(&ctx.session_id)
            .ok()
            .and_then(|session| {
                self.manager
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .agent_for_session(session)
            });

        let instruction = parsed.instruction.clone();
        // 注册（Starting）+ 委托记录。
        let agent_id = {
            let mut manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
            // The graph records the exact active directory used by the parent
            // runtime, so descendants receive the same tool surface.
            manager.set_tool_registry(ctx.registry.clone());
            match manager.register_child(
                child_session,
                instruction.clone(),
                child_cancel.clone(),
                parent_agent_id,
            ) {
                Ok(id) => {
                    manager.add_delegation(Delegation {
                        id: delegation_id,
                        child_agent: id,
                        child_session,
                        state: DelegationState::Running,
                    });
                    id
                }
                Err(e) => return args_error(e),
            }
        };

        // 启动 worker（后台；不 await）。
        let worker = spawn_worker(
            self.config.clone(),
            self.make_provider.clone(),
            self.manager.clone(),
            agent_id,
            delegation_id,
            SubagentRequest {
                instruction,
                child_session,
                parent: None,
            },
            child_cancel.clone(),
            ctx.registry.clone(),
            ctx.resource_manager(),
            ctx.output_tx.clone(),
            Some(ctx.call_id),
            self.report_tx.clone(),
        );
        // worker 已 spawn：标记 Running。
        let unowned_worker = {
            let mut manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
            manager.mark_running(agent_id);
            if let Err((error, worker)) = manager.register_worker(agent_id, worker) {
                tracing::error!(%error, %agent_id, "agent worker could not be registered");
                // The task has already started; cancellation is the safe
                // fallback when ownership registration unexpectedly fails.
                let _ = manager.cancel(agent_id);
                Some(worker)
            } else {
                None
            }
        };
        if let Some(worker) = unowned_worker {
            child_cancel.cancel();
            // The graph was closed between registration and ownership handoff.
            // Keep a join owner until the task observes cancellation; never
            // silently detach a worker whose process tree may still be alive.
            tokio::spawn(async move {
                let _ = worker.await;
            });
        }

        ToolOutcome::succeeded(
            self.name(),
            spawn_reply(agent_id, delegation_id, &parsed.instruction),
        )
    }
}

/// `agent` 控制面工具（list/status/wait/cancel；镜像 process 工具模式）。
pub struct AgentControlTool<P>
where
    P: Provider + Send + 'static,
{
    manager: SharedAgentManager,
    _provider: std::marker::PhantomData<fn() -> P>,
}

impl<P> AgentControlTool<P>
where
    P: Provider + Send + 'static,
{
    pub fn new(manager: SharedAgentManager) -> Self {
        Self {
            manager,
            _provider: std::marker::PhantomData,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct AgentControlArgs {
    /// list / status / wait / cancel / close / message / mailbox。
    pub action: String,
    pub agent_id: Option<String>,
    /// Answer for a graph-routed request_input or a mailbox message.
    pub message: Option<String>,
    /// wait 的超时毫秒（默认 120_000；0 = 不限时）。
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn agent_not_found(id: &str) -> ToolOutcome {
    ToolOutcome::failed(
        "agent",
        tpi_core::outcome::ModelPayload {
            status: tpi_core::outcome::ToolStatus::Rejected,
            program: Some("agent".into()),
            exit_code: None,
            duration_ms: 0,
            output: format!("status: rejected\nerror: agent not found: {id}"),
            effect: None,
            artifact: None,
        },
    )
}

fn parse_agent_id(raw: &str) -> Result<AgentId, String> {
    AgentId::parse_str(raw).map_err(|e| format!("invalid agent_id {raw:?}: {e}"))
}

#[async_trait]
impl<P> Tool for AgentControlTool<P>
where
    P: Provider + Send + 'static,
{
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "控制/查询 agent graph 中的后台 agent。action: \
         list=列出全部agent与状态；status=查询单个agent（含最近报告）；wait=等待 \
         agent 终态（**仅在下一步必须依赖该结果时使用**，可带 timeout_ms；正常应 \
         继续其他工作）；cancel=取消；close=在终态后清理记录；message=向 agent \
         投递 mailbox 或回答 request_input；mailbox=读取指定 agent 的 mailbox。示例：\
         agent action=list / agent action=status agent_id=\"...\""
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "status", "wait", "cancel", "close", "message", "mailbox"] },
                "agent_id": { "type": "string", "description": "目标 agent id（status/wait/cancel/close/message 必填；mailbox 省略时读取当前 agent）" },
                "message": { "type": "string", "description": "message action 的投递内容" },
                "timeout_ms": { "type": "integer", "description": "wait 超时毫秒（默认 120000；0=不限时）" }
            },
            "required": ["action"]
        })
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    fn access_class(&self) -> tpi_capabilities::tool::registry::ToolAccessClass {
        tpi_capabilities::tool::registry::ToolAccessClass::Pure
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        let parsed: AgentControlArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return args_error(format!("agent 参数解析失败: {e}")),
        };
        let started = std::time::Instant::now();
        let outcome = match parsed.action.as_str() {
            "list" => {
                let manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
                let views = manager.list();
                let mut body = String::from("agents:\n");
                for v in &views {
                    body.push_str(&format!(
                        "  {} {:<9} {}\n",
                        v.agent_id,
                        v.state.as_str(),
                        truncate(&v.instruction, 60)
                    ));
                }
                if views.is_empty() {
                    body.push_str("  (none)\n");
                }
                ok_text("list", body)
            }
            "status" => {
                let Some(raw) = &parsed.agent_id else {
                    return args_error("agent status 需要 agent_id".into());
                };
                let Ok(id) = parse_agent_id(raw) else {
                    return agent_not_found(raw);
                };
                let manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
                match manager.status(id) {
                    Some(v) => {
                        let mut body = format!(
                            "agent_id: {}\nstate: {}\nchild_session: {}\ninstruction: {}\n",
                            v.agent_id,
                            v.state.as_str(),
                            v.child_session,
                            v.instruction
                        );
                        if let Some(s) = &v.last_summary {
                            body.push_str(&format!("last_summary: {}\n", truncate(s, 2000)));
                        }
                        ok_text("status", body)
                    }
                    None => return agent_not_found(raw),
                }
            }
            "message" => {
                let Some(raw) = &parsed.agent_id else {
                    return args_error("agent message 需要 agent_id".into());
                };
                let Ok(id) = parse_agent_id(raw) else {
                    return agent_not_found(raw);
                };
                let Some(message) = parsed.message.clone() else {
                    return args_error("agent message 需要 message".into());
                };
                if message.trim().is_empty() {
                    return args_error("agent message 需要非空 message".into());
                }
                let mut manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
                if manager.pending_input(id).is_some() {
                    match manager.answer_agent(id, message) {
                        Ok(request) => ok_text(
                            "message",
                            format!(
                                "agent_id: {}\nrequest_id: {}\nstatus: input_answered\n",
                                request.agent_id, request.request_id
                            ),
                        ),
                        Err(error) => args_error(error),
                    }
                } else {
                    let from_agent_id = tpi_core::ids::SessionId::parse_str(&ctx.session_id)
                        .ok()
                        .and_then(|session| manager.agent_for_session(session));
                    match manager.send_message(from_agent_id, id, message) {
                        Ok(message) => ok_text(
                            "message",
                            format!(
                                "agent_id: {}\nmessage_id: {}\nstatus: queued\n",
                                message.to_agent_id, message.message_id
                            ),
                        ),
                        Err(error) => args_error(error),
                    }
                }
            }
            "mailbox" => {
                let target = match &parsed.agent_id {
                    Some(raw) => match parse_agent_id(raw) {
                        Ok(id) => id,
                        Err(_) => return agent_not_found(raw),
                    },
                    None => {
                        let manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
                        let Some(session) =
                            tpi_core::ids::SessionId::parse_str(&ctx.session_id).ok()
                        else {
                            return args_error("当前 runtime 没有关联 agent".into());
                        };
                        let Some(id) = manager.agent_for_session(session) else {
                            return args_error("当前 runtime 没有关联 agent".into());
                        };
                        id
                    }
                };
                let mut manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
                match manager.drain_mailbox(target) {
                    Ok(messages) => {
                        let mut body = String::from("messages:\n");
                        if messages.is_empty() {
                            body.push_str("  (none)\n");
                        } else {
                            for message in messages {
                                let sender = message
                                    .from_agent_id
                                    .map(|id| id.to_string())
                                    .unwrap_or_else(|| "root".into());
                                body.push_str(&format!(
                                    "  {} from={} {}\n",
                                    message.message_id, sender, message.body
                                ));
                            }
                        }
                        ok_text("mailbox", body)
                    }
                    Err(error) => args_error(error),
                }
            }
            "wait" => {
                let Some(raw) = &parsed.agent_id else {
                    return args_error("agent wait 需要 agent_id".into());
                };
                let Ok(id) = parse_agent_id(raw) else {
                    return agent_not_found(raw);
                };
                let timeout =
                    std::time::Duration::from_millis(parsed.timeout_ms.unwrap_or(120_000));
                let wait_cancel = CancellationToken::new();
                // run 取消传播到 wait。
                {
                    let run_cancel = ctx.cancel.clone();
                    let wc = wait_cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = run_cancel.cancelled() => wc.cancel(),
                            _ = wc.cancelled() => {}
                        }
                    });
                }
                let manager = self.manager.clone();
                let result = if timeout.is_zero() {
                    AgentManager::wait(&manager, id, wait_cancel).await
                } else {
                    tokio::select! {
                        r = AgentManager::wait(&manager, id, wait_cancel) => r,
                        _ = tokio::time::sleep(timeout) => None,
                    }
                };
                match result {
                    Some(()) => {
                        let guard = manager.lock().unwrap_or_else(|p| p.into_inner());
                        match guard.status(id) {
                            Some(v) => ok_text(
                                "wait",
                                format!(
                                    "agent_id: {}\nstate: {}\nlast_summary: {}\n",
                                    v.agent_id,
                                    v.state.as_str(),
                                    v.last_summary.as_deref().unwrap_or("(无报告)")
                                ),
                            ),
                            None => agent_not_found(raw),
                        }
                    }
                    None => {
                        let guard = manager.lock().unwrap_or_else(|p| p.into_inner());
                        let state = guard
                            .status(id)
                            .map(|v| v.state.as_str().to_string())
                            .unwrap_or_else(|| "unknown".into());
                        ok_text(
                            "wait",
                            format!(
                                "status: timeout_or_cancelled\nagent_id: {}\nstate: {state}\n\
                                 提示：等待超时/被取消。agent 仍在后台运行时，继续其他工作，稍后查询。",
                                raw
                            ),
                        )
                    }
                }
            }
            "cancel" | "close" => {
                let Some(raw) = &parsed.agent_id else {
                    return args_error(format!("agent {} 需要 agent_id", parsed.action));
                };
                let Ok(id) = parse_agent_id(raw) else {
                    return agent_not_found(raw);
                };
                let mut manager = self.manager.lock().unwrap_or_else(|p| p.into_inner());
                let result = if parsed.action == "cancel" {
                    manager.cancel(id).map(|_| "cancelled".to_string())
                } else {
                    manager.close(id).map(|_| "closed".to_string())
                };
                match result {
                    Ok(status) => ok_text(
                        &parsed.action,
                        format!("agent_id: {id}\nstatus: {status}\n"),
                    ),
                    Err(e) => agent_not_found(&e),
                }
            }
            other => args_error(format!(
                "未知 action: {other:?}（可用: list/status/wait/cancel/close/message/mailbox）"
            )),
        };
        outcome.with_timing(started.elapsed().as_millis() as u64)
    }
}

fn ok_text(action: &str, body: String) -> ToolOutcome {
    ToolOutcome::succeeded(
        "agent",
        format!("status: succeeded\naction: {action}\n{body}"),
    )
}

fn args_error(message: String) -> ToolOutcome {
    ToolOutcome::failed(
        "agent",
        tpi_core::outcome::ModelPayload {
            status: tpi_core::outcome::ToolStatus::Rejected,
            program: Some("agent".into()),
            exit_code: Some(2),
            duration_ms: 0,
            output: format!("status: rejected\nerror: {message}"),
            effect: None,
            artifact: None,
        },
    )
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// composition root 注册（spawn_agent + agent 一起）。
pub fn register_async_subagent_tools<P, F>(
    registry: &Arc<Mutex<tpi_capabilities::tool::registry::ToolRegistry>>,
    config: Arc<tpi_config::config::Config>,
    make_provider: F,
    manager: SharedAgentManager,
    report_tx: Option<tokio::sync::mpsc::Sender<crate::agent::LiveEvent>>,
) where
    P: Provider + Send + 'static,
    F: Fn() -> P + Send + Sync + 'static,
{
    let spawn: Arc<dyn Tool> = Arc::new(SpawnAgentTool::<P>::new(
        config,
        make_provider,
        manager.clone(),
        report_tx,
    ));
    registry
        .lock()
        .unwrap()
        .register_validated(spawn)
        .expect("spawn_agent 工具定义必须合法");
    let control: Arc<dyn Tool> = Arc::new(AgentControlTool::<P>::new(manager));
    registry
        .lock()
        .unwrap()
        .register_validated(control)
        .expect("agent 工具定义必须合法");
}

/// 把 inbox 中待投影的 report 转为 durable 事件（parent run boundary 调用；
/// 单写者安全：仅在 parent session owner 临界区内执行）。
pub fn pending_reports_as_events(pending: &[PendingReport]) -> Vec<SessionEvent> {
    pending
        .iter()
        .map(|p| SessionEvent::SubagentReported {
            delegation_id: p.delegation_id,
            agent_id: p.agent_id,
            child_session: p.child_session,
            summary: p.summary.clone(),
            evidence: p.evidence.clone(),
            final_report: p.final_report,
        })
        .collect()
}

/// 生成终态事件（settlement；parent run boundary 调用）。
pub fn settled_event(
    delegation_id: DelegationId,
    agent_id: AgentId,
    child_session: SessionId,
    reason: SubagentFinishedReason,
    summary: Option<String>,
    evidence: Vec<String>,
) -> SessionEvent {
    SessionEvent::SubagentSettled {
        delegation_id,
        agent_id,
        child_session,
        reason,
        summary,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_reply_mentions_nonblocking() {
        let reply = spawn_reply(AgentId::new_v7(), DelegationId::new_v7(), "调查 parser");
        assert!(reply.contains("status: spawned"));
        assert!(reply.contains("非阻塞"));
    }
}
