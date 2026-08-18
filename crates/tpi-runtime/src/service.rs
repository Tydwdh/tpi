//! ApplicationService：runtime 核心任务（web_desktop.md §七 / Phase 2）。
//!
//! 单任务循环消费 `ClientCommand`；**run 执行在独立 tokio task**，主循环
//! 不阻塞——这是 CancelRun / AnswerInput 在 run 进行中仍能及时处理的必要条件
//! （web_desktop.md §二十九：Cancel idempotent，TUI 必须能在 run 中取消）。
//!
//! ## 状态模型
//!
//! - `sessions: HashMap<SessionId, Arc<Mutex<Option<SessionRuntime>>>>`
//!   - `Some(session)` = 空闲/挂起（可提交）；`None` = 正在 run（session 被
//!     run task 独占）。
//! - `runs: HashMap<SessionId, CancellationToken>` = 进行中的 run 取消表。
//!   主循环单线程持有，run 期间其他命令（Cancel/Submit busy 检查）查它，
//!   **不碰 session 锁**——因此 run 的 execute 持锁长 await 不阻塞取消。
//!
//! ## 并发安全
//!
//! - 一个 session 同一时刻最多一个 run（runs 表唯一键）。
//! - Cancel 幂等：无 run 时 no-op 成功；有 run 时 cancel token 生效。
//! - AnswerInput 重复回答：第一个 Accepted，其余 Rejected（run 结束后
//!   status=AwaitingInput 已被清除）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use camino::Utf8PathBuf;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use tpi_agent::agent::{self, AgentOutcome, LiveEvent};
use tpi_agent::provider::Provider;
use tpi_capabilities::tool::registry::ToolRegistry;
use tpi_config::config::{Config, ModelConfig};
use tpi_core::ids::{RequestId, RunId, SessionId};
use tpi_core::outcome::ToolStatus;
use tpi_session::conversation::Conversation;
use tpi_session::{CompletionReason, SessionEvent, SessionLog};

use tpi_protocol::{
    AppError, ClientCommand, CommandAck, CompletionReasonDto, DeltaKind, ErrorCode, EventEnvelope,
    QuestionDto, QuestionOptionDto, RuntimeEvent, SessionStatus, SessionView, ToolState,
};

use crate::handle::PendingCommand;

/// runtime 内部的会话状态（`None` 包装 = 正在 run）。
pub struct SessionRuntime<P: Provider> {
    pub conversation: Conversation,
    pub provider: P,
    pub registry: Arc<StdMutex<ToolRegistry>>,
    /// session 级共享 ManagedProcess registry（任务书 §8：跨 run 存活；
    /// background bash 注册，`process` 工具读取/取消）。
    pub processes: Arc<StdMutex<tpi_capabilities::process::managed::ProcessRegistry>>,
    /// session 级共享 Persistent PTY terminal registry（跨 run 存活）。
    pub terminals: Arc<StdMutex<tpi_capabilities::terminal::TerminalRegistry>>,
    /// ADR-007：session 级共享 AgentManager（跨 run 存活；后台调查注册/查询/取消）。
    pub agents: Arc<StdMutex<tpi_agent::agent::manager::AgentManager>>,
    pub status: SessionStatus,
}

/// 事件发射器：分配全局单调 seq、构造信封、更新 last_seq、广播。
#[derive(Clone)]
pub struct Emitter {
    tx: broadcast::Sender<EventEnvelope>,
    seq: Arc<AtomicU64>,
    last_seq: Arc<StdMutex<u64>>,
}

impl Emitter {
    fn new(tx: broadcast::Sender<EventEnvelope>, last_seq: Arc<StdMutex<u64>>) -> Self {
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
            last_seq,
        }
    }

    /// 分配 seq、构造信封并广播。无订阅者时 no-op（不视为错误）。
    pub fn emit(&self, event: RuntimeEvent) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let envelope = EventEnvelope::new(seq, event);
        if let Ok(mut last) = self.last_seq.lock()
            && envelope.seq > *last
        {
            *last = envelope.seq;
        }
        let _ = self.tx.send(envelope);
        seq
    }
}

/// provider 重建闭包类型：给定模型配置，构造 provider 实例。
pub type ProviderFactory<P> = Box<dyn FnMut(&ModelConfig) -> Result<P, String> + Send>;

/// runtime 任务本体（通过 `RuntimeHandle::new` 启动）。
pub struct RuntimeTask<P: Provider + 'static> {
    config: Arc<Config>,
    sessions_root: std::path::PathBuf,
    workspace_root: Utf8PathBuf,
    /// None = 正在 run（session 被 run task 独占）。
    sessions: HashMap<SessionId, Arc<StdMutex<Option<SessionRuntime<P>>>>>,
    /// 进行中的 run 取消表（主循环独占）。
    runs: HashMap<SessionId, CancellationToken>,
    /// 进行中的 run task JoinHandle（shutdown 时等待结束）。
    run_handles: HashMap<SessionId, tokio::task::JoinHandle<()>>,
    build_provider: ProviderFactory<P>,
    registry: Arc<StdMutex<ToolRegistry>>,
}

impl<P: Provider + 'static> RuntimeTask<P> {
    /// 构造 runtime 任务（调用方通过 `RuntimeHandle::new` 启动它）。
    pub fn new(
        config: Arc<Config>,
        build_provider: ProviderFactory<P>,
        registry: Arc<StdMutex<ToolRegistry>>,
    ) -> Self {
        let sessions_root = config.sessions_root.clone();
        let workspace_root = config.workspace_root.clone();
        Self {
            config,
            sessions_root,
            workspace_root,
            sessions: HashMap::new(),
            runs: HashMap::new(),
            run_handles: HashMap::new(),
            build_provider,
            registry,
        }
    }

    /// runtime 主循环（由 `RuntimeHandle::new` 调用并 spawn）。
    pub(crate) async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<PendingCommand>,
        event_tx: broadcast::Sender<EventEnvelope>,
        shutdown: CancellationToken,
        last_seq: Arc<StdMutex<u64>>,
    ) {
        let emitter = Emitter::new(event_tx, last_seq);
        let (done_tx, mut done_rx) = mpsc::channel::<SessionId>(16);
        info!("tpi-runtime 启动");

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("tpi-runtime 收到关闭信号，退出主循环");
                    break;
                }
                maybe = cmd_rx.recv() => {
                    let Some(pending) = maybe else {
                        info!("命令通道已关闭，退出 runtime 主循环");
                        break;
                    };
                    self.handle_command(pending, &emitter, &done_tx).await;
                }
                finished = done_rx.recv() => {
                    if let Some(session_id) = finished {
                        self.runs.remove(&session_id);
                        self.run_handles.remove(&session_id);
                    }
                }
            }
        }

        // 退出前取消所有 run，等它们结束（最多 5 秒）。
        for (_, cancel) in self.runs.drain() {
            cancel.cancel();
        }
        for (sid, handle) in self.run_handles.drain() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!(?sid, "run task panicked: {e}");
                }
                Err(_) => {
                    warn!(?sid, "run task did not finish within 5s after cancel");
                }
            }
        }
        info!("tpi-runtime 已停止");
    }

    async fn handle_command(
        &mut self,
        pending: PendingCommand,
        emitter: &Emitter,
        done_tx: &mpsc::Sender<SessionId>,
    ) {
        let PendingCommand {
            command,
            request_id,
            reply,
        } = pending;

        let result = match &command {
            ClientCommand::CreateSession { title } => {
                self.cmd_create_session(title.clone(), emitter).await
            }
            ClientCommand::ListSessions => self.cmd_list_sessions(emitter).await,
            ClientCommand::ResumeSession { session_id } => {
                self.cmd_resume_session(session_id.clone(), emitter).await
            }
            ClientCommand::SubmitMessage {
                session_id,
                content,
            } => {
                self.cmd_start_run(*session_id, RunKind::New(content.clone()), emitter, done_tx)
                    .await
            }
            ClientCommand::CancelRun { session_id } => self.cmd_cancel_run(*session_id),
            ClientCommand::RetryRun { session_id } => {
                self.cmd_start_run(*session_id, RunKind::Retry, emitter, done_tx)
                    .await
            }
            ClientCommand::AnswerInput {
                session_id,
                request_id: req,
                answer,
            } => {
                self.cmd_start_run(
                    *session_id,
                    RunKind::Answer {
                        request_id: *req,
                        answer: answer.clone(),
                    },
                    emitter,
                    done_tx,
                )
                .await
            }
            ClientCommand::Undo {
                session_id,
                all,
                force,
            } => self.cmd_undo(*session_id, *all, *force, emitter).await,
            ClientCommand::Redo {
                session_id,
                all,
                force,
            } => self.cmd_redo(*session_id, *all, *force, emitter).await,
            ClientCommand::Shutdown => Ok(()),
        };

        let ack = match result {
            Ok(()) => CommandAck::accepted(request_id),
            Err(err) => CommandAck::rejected(request_id, err),
        };
        let _ = reply.send(ack);
    }

    // ===== 命令实现 =====

    async fn cmd_create_session(
        &mut self,
        _title: Option<String>,
        emitter: &Emitter,
    ) -> Result<(), AppError> {
        let provider = (self.build_provider)(&self.config.model)
            .map_err(|e| AppError::new(ErrorCode::InternalError, e))?;
        let conversation = Conversation::new();
        let sid = SessionId::new_v7();
        let session = SessionRuntime {
            conversation,
            provider,
            registry: self.registry.clone(),
            processes: Arc::new(StdMutex::new(
                tpi_capabilities::process::managed::ProcessRegistry::new(),
            )),
            terminals: Arc::new(StdMutex::new(
                tpi_capabilities::terminal::TerminalRegistry::default(),
            )),
            agents: Arc::new(StdMutex::new(
                tpi_agent::agent::manager::AgentManager::new(),
            )),
            status: SessionStatus::Idle,
        };
        let ws = self.workspace_name();
        let mut session_for_view = session;
        let view = session_view(&ws, &mut session_for_view, sid);
        self.sessions
            .insert(sid, Arc::new(StdMutex::new(Some(session_for_view))));
        emitter.emit(RuntimeEvent::SessionCreated { session: view });
        Ok(())
    }

    async fn cmd_list_sessions(&mut self, emitter: &Emitter) -> Result<(), AppError> {
        let sessions = self.list_session_views();
        emitter.emit(RuntimeEvent::SessionList { sessions });
        Ok(())
    }

    async fn cmd_resume_session(
        &mut self,
        id_str: String,
        emitter: &Emitter,
    ) -> Result<(), AppError> {
        let session_id = self.parse_session_id_prefix(&id_str)?;

        if let Some(arc) = self.sessions.get(&session_id) {
            let mut guard = arc
                .lock()
                .map_err(|e| AppError::new(ErrorCode::InternalError, e.to_string()))?;
            if let Some(s) = guard.as_mut() {
                let ws = self.workspace_name();
                let view = session_view(&ws, s, session_id);
                emitter.emit(RuntimeEvent::SessionResumed { session: view });
                drop(guard);
                emit_session_history(&self.sessions, session_id, emitter);
                return Ok(());
            }
            // run 进行中：仍返回 SessionResumed（视图由事件流持续更新）。
            let ws = self.workspace_name();
            let view = SessionView {
                id: session_id,
                title: String::new(),
                workspace: ws,
                status: SessionStatus::Running,
                created_at_ms: 0,
                updated_at_ms: 0,
            };
            emitter.emit(RuntimeEvent::SessionResumed { session: view });
            return Ok(());
        }

        let provider = (self.build_provider)(&self.config.model)
            .map_err(|e| AppError::new(ErrorCode::InternalError, e))?;
        let mut conversation =
            Conversation::resume(&self.sessions_root, &self.workspace_root, session_id)
                .map_err(|e| AppError::new(ErrorCode::SessionNotFound, e))?;
        let status = if conversation.history().is_empty() {
            SessionStatus::Idle
        } else {
            let awaiting = conversation
                .log()
                .map(last_event_is_awaiting_input)
                .unwrap_or(false);
            if awaiting {
                SessionStatus::AwaitingInput
            } else {
                SessionStatus::Idle
            }
        };
        let session = SessionRuntime {
            conversation,
            provider,
            registry: self.registry.clone(),
            processes: Arc::new(StdMutex::new(
                tpi_capabilities::process::managed::ProcessRegistry::new(),
            )),
            terminals: Arc::new(StdMutex::new(
                tpi_capabilities::terminal::TerminalRegistry::default(),
            )),
            agents: Arc::new(StdMutex::new(
                tpi_agent::agent::manager::AgentManager::new(),
            )),
            status,
        };
        let ws = self.workspace_name();
        let mut session_for_view = session;
        let view = session_view(&ws, &mut session_for_view, session_id);
        self.sessions
            .insert(session_id, Arc::new(StdMutex::new(Some(session_for_view))));
        emitter.emit(RuntimeEvent::SessionResumed { session: view });
        // 断线重连 / 页面刷新后的历史重建：广播会话历史快照。
        emit_session_history(&self.sessions, session_id, emitter);
        Ok(())
    }

    /// 启动一次 run（New / Retry / Answer）。session 从 map take 出来交给
    /// run task 独占；主循环不阻塞（runs 表负责 busy 检查与 cancel）。
    async fn cmd_start_run(
        &mut self,
        session_id: SessionId,
        kind: RunKind,
        emitter: &Emitter,
        done_tx: &mpsc::Sender<SessionId>,
    ) -> Result<(), AppError> {
        // 参数校验。
        match &kind {
            RunKind::New(content) if content.trim().is_empty() => {
                return Err(AppError::invalid("消息不能为空"));
            }
            RunKind::Answer { answer, .. } if answer.trim().is_empty() => {
                return Err(AppError::invalid("回答不能为空"));
            }
            _ => {}
        }

        // Busy 检查：runs 表有该 session = 正在 run。
        if self.runs.contains_key(&session_id) {
            return Err(AppError::new(ErrorCode::Busy, "会话正在运行中，请稍候"));
        }

        let arc = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| AppError::new(ErrorCode::SessionNotFound, "会话不存在"))?
            .clone();

        // Answer 需要 session 处于 AwaitingInput。
        if let RunKind::Answer { .. } = &kind {
            let guard = arc
                .lock()
                .map_err(|e| AppError::new(ErrorCode::InternalError, e.to_string()))?;
            let is_awaiting = guard
                .as_ref()
                .map(|s| matches!(s.status, SessionStatus::AwaitingInput))
                .unwrap_or(false);
            if !is_awaiting {
                return Err(AppError::new(
                    ErrorCode::InvalidCommand,
                    "会话当前没有挂起的输入请求",
                ));
            }
        }

        let cancel = CancellationToken::new();
        self.runs.insert(session_id, cancel.clone());

        let run_id = RunId::new_v7();
        emitter.emit(RuntimeEvent::SessionStatusChanged {
            session_id,
            status: SessionStatus::Running,
        });
        emitter.emit(RuntimeEvent::RunStarted { session_id, run_id });

        // 取出 session 交给 run task（锁内置 None）。
        {
            let guard = arc
                .lock()
                .map_err(|e| AppError::new(ErrorCode::InternalError, e.to_string()))?;
            if guard.is_none() {
                // 理论上不可能（runs 表已挡）；防御。
                self.runs.remove(&session_id);
                return Err(AppError::new(ErrorCode::Busy, "会话正在运行中"));
            }
        }

        let session_arc = arc.clone();
        let run_emitter = emitter.clone();
        let run_done = done_tx.clone();
        let cfg = self.config.clone();
        let sessions_root = self.sessions_root.clone();
        let workspace_root = self.workspace_root.clone();
        let message = match &kind {
            RunKind::New(content) => content.clone(),
            RunKind::Retry => String::new(),
            RunKind::Answer { answer, .. } => answer.clone(),
        };
        let answer_request = match &kind {
            RunKind::Answer { request_id, .. } => Some(*request_id),
            _ => None,
        };

        // 在 run task 启动前记录 UserInputReceived（Answer 语义，与 app 层一致）。
        // 在 task 内做：先 take session，写 log，再跑 agent。
        let handle = tokio::spawn(async move {
            // take session（独占）。
            let session = {
                let mut guard = session_arc.lock().unwrap_or_else(|p| p.into_inner());
                guard.take()
            };
            let Some(mut session) = session else {
                warn!("run task: session 已被其他 run 占用");
                return;
            };

            // Answer：先记录 durable UserInputReceived。
            if let Some(req) = answer_request {
                match session
                    .conversation
                    .parts_for_run()
                    .map_err(|e| e.to_string())
                {
                    Ok((session_log, _history)) => {
                        use tpi_session::store::SessionStore;
                        let _ = session_log
                            .commit(&SessionEvent::UserInputReceived {
                                content: message.clone(),
                            })
                            .map_err(|e| format!("UserInputReceived 写盘失败: {e}"));
                        let _ = run_emitter.emit(RuntimeEvent::InputAnswered {
                            session_id,
                            request_id: req,
                        });
                    }
                    Err(e) => {
                        warn!("answer: parts_for_run 失败: {e}");
                    }
                }
            }

            // New：广播用户消息已持久化。
            if let RunKind::New(_) = &kind {
                run_emitter.emit(RuntimeEvent::UserMessageAdded {
                    session_id,
                    content: message.clone(),
                });
            }

            // 执行 agent run。
            let ctx = RunContext {
                run_id,
                message,
                cancel: cancel.clone(),
                config: &cfg,
                emitter: run_emitter.clone(),
                sessions_root: &sessions_root,
                workspace_root: &workspace_root,
            };
            let outcome = execute_agent_run(&mut session, ctx).await;

            // 计算结束后状态并放回 session。
            let status_after = match &outcome {
                Ok(out) if out.reason == CompletionReason::AwaitingUserInput => {
                    if let Some(awaiting) = &out.awaiting_input {
                        let questions = awaiting
                            .questions
                            .iter()
                            .map(project_question)
                            .collect::<Vec<_>>();
                        run_emitter.emit(RuntimeEvent::InputRequested {
                            session_id,
                            run_id,
                            request_id: RequestId::new_v7(),
                            text: awaiting.text.clone(),
                            questions,
                        });
                    }
                    SessionStatus::AwaitingInput
                }
                Ok(_) => SessionStatus::Idle,
                Err(_) => SessionStatus::Idle,
            };
            session.status = status_after;
            {
                let mut guard = session_arc.lock().unwrap_or_else(|p| p.into_inner());
                *guard = Some(session);
            }

            // 广播 run 终态。
            match outcome {
                Ok(out) => {
                    run_emitter.emit(RuntimeEvent::RunCompleted {
                        session_id,
                        run_id,
                        reason: completion_reason_to_dto(out.reason),
                        assistant_text: out.assistant_text,
                    });
                }
                Err(e) => {
                    run_emitter.emit(RuntimeEvent::RunFailed {
                        session_id,
                        run_id,
                        error: AppError::new(ErrorCode::InternalError, e),
                    });
                }
            }
            run_emitter.emit(RuntimeEvent::SessionStatusChanged {
                session_id,
                status: status_after,
            });

            // 通知主循环清理 runs 表。
            let _ = run_done.send(session_id).await;
        });
        self.run_handles.insert(session_id, handle);

        Ok(())
    }

    fn cmd_cancel_run(&mut self, session_id: SessionId) -> Result<(), AppError> {
        // 幂等：无 run 时 no-op 成功。
        if let Some(cancel) = self.runs.get(&session_id) {
            cancel.cancel();
        }
        Ok(())
    }

    async fn cmd_undo(
        &mut self,
        session_id: SessionId,
        all: bool,
        force: bool,
        emitter: &Emitter,
    ) -> Result<(), AppError> {
        // Mutation Journal 撤销（逻辑在 tpi-session::journal，与 CLI `tpi undo` 同一实现）。
        let summary = self.journal_apply(session_id, all, force, JournalAction::Undo)?;
        emitter.emit(RuntimeEvent::UndoCompleted {
            session_id,
            summary,
        });
        Ok(())
    }

    async fn cmd_redo(
        &mut self,
        session_id: SessionId,
        all: bool,
        force: bool,
        emitter: &Emitter,
    ) -> Result<(), AppError> {
        let summary = self.journal_apply(session_id, all, force, JournalAction::Redo)?;
        emitter.emit(RuntimeEvent::RedoCompleted {
            session_id,
            summary,
        });
        Ok(())
    }

    /// 对指定 session 的 Mutation Journal 执行 undo/redo，返回人读摘要。
    fn journal_apply(
        &self,
        session_id: SessionId,
        all: bool,
        force: bool,
        action: JournalAction,
    ) -> Result<String, AppError> {
        use tpi_session::journal::{CasVerdict, assert_can_mutate, journal_path, load_journal};
        let artifacts_root = self.config.artifacts_root.clone();
        let jpath = journal_path(&artifacts_root, &session_id.to_string());
        let state = load_journal(&jpath)
            .map_err(|e| AppError::new(ErrorCode::InternalError, e.to_string()))?;
        assert_can_mutate(&state, force)
            .map_err(|e| AppError::new(ErrorCode::InternalError, e.to_string()))?;
        let mutations = &state.mutations;
        if mutations.is_empty() {
            return Ok("会话没有已记录的文件变更".to_string());
        }
        let ws = self.workspace_root.as_std_path();
        let verdicts = match (action, all) {
            (JournalAction::Undo, true) => tpi_session::journal::undo_all(mutations, ws),
            (JournalAction::Undo, false) => tpi_session::journal::undo_last(mutations, ws),
            (JournalAction::Redo, true) => tpi_session::journal::redo_all(mutations, ws),
            (JournalAction::Redo, false) => tpi_session::journal::redo_last(mutations, ws),
        }
        .map_err(|e| AppError::new(ErrorCode::InternalError, format!("journal 操作失败: {e}")))?;

        let applied = verdicts
            .iter()
            .filter(|(_, v)| *v == CasVerdict::Applied)
            .count();
        let already = verdicts
            .iter()
            .filter(|(_, v)| *v == CasVerdict::AlreadyDone)
            .count();
        let conflicts: Vec<&String> = verdicts
            .iter()
            .filter(|(_, v)| *v == CasVerdict::Conflict)
            .map(|(p, _)| p)
            .collect();
        let action_name = match action {
            JournalAction::Undo => "撤销",
            JournalAction::Redo => "重做",
        };
        if conflicts.is_empty() {
            Ok(format!(
                "{action_name}完成：应用 {applied} 个，已处于目标状态 {already} 个"
            ))
        } else {
            let list = conflicts
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!(
                "{action_name}冲突（{} 个文件被外部修改，CAS 拒绝）：{list}；已应用 {applied} 个，已处于目标状态 {already} 个",
                conflicts.len()
            ))
        }
    }

    // ===== 视图辅助 =====

    /// 当前 workspace 展示名。
    fn workspace_name(&self) -> String {
        self.workspace_root
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    fn list_session_views(&self) -> Vec<SessionView> {
        let workspace_id = tpi_session::store::workspace_id_for(self.workspace_root.as_std_path());
        let Ok(entries) = std::fs::read_dir(self.sessions_root.join(&workspace_id)) else {
            return Vec::new();
        };
        let mut views = Vec::new();
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(id_str) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let Ok(id) = SessionId::parse_str(id_str) else {
                continue;
            };
            let title = read_session_title(&entry.path());
            let created = entry
                .metadata()
                .map(|m| m.created().map(unix_ms).unwrap_or(0))
                .unwrap_or(0);
            let updated = entry
                .metadata()
                .map(|m| m.modified().map(unix_ms).unwrap_or(0))
                .unwrap_or(0);
            views.push(SessionView {
                id,
                title,
                workspace: self.workspace_name(),
                status: SessionStatus::Idle,
                created_at_ms: created,
                updated_at_ms: updated,
            });
        }
        views.sort_by_key(|v| std::cmp::Reverse(v.updated_at_ms));
        views
    }

    fn parse_session_id_prefix(&self, id_str: &str) -> Result<SessionId, AppError> {
        if let Ok(id) = SessionId::parse_str(id_str) {
            return Ok(id);
        }
        let workspace_id = tpi_session::store::workspace_id_for(self.workspace_root.as_std_path());
        let dir = self.sessions_root.join(&workspace_id);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Err(AppError::new(
                ErrorCode::SessionNotFound,
                "找不到匹配的 session",
            ));
        };
        let mut matches = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(file_id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if file_id.starts_with(id_str) {
                matches.push(file_id.to_string());
            }
        }
        match matches.len() {
            1 => SessionId::parse_str(&matches[0])
                .map_err(|_| AppError::new(ErrorCode::SessionNotFound, "无效 session id")),
            0 => Err(AppError::new(
                ErrorCode::SessionNotFound,
                "找不到匹配的 session",
            )),
            _ => Err(AppError::new(
                ErrorCode::InvalidCommand,
                format!("session id 前缀不唯一：{id_str}"),
            )),
        }
    }
}

/// run 启动方式。
enum RunKind {
    New(String),
    Retry,
    Answer {
        request_id: RequestId,
        answer: String,
    },
}

/// 会话视图投影（自由函数：避免与 `self.sessions` 的借用冲突）。
fn session_view<P: Provider>(
    workspace: &str,
    session: &mut SessionRuntime<P>,
    id: SessionId,
) -> SessionView {
    let title = session
        .conversation
        .history()
        .iter()
        .find_map(|m| match m {
            tpi_core::message::ChatMessage::User(text) => {
                let t = text.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.chars().take(60).collect::<String>())
                }
            }
            _ => None,
        })
        .unwrap_or_default();
    SessionView {
        id,
        title,
        workspace: workspace.to_string(),
        status: session.status,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

/// 广播会话历史快照（ResumeSession 后前端重建 transcript）。
fn emit_session_history<P: Provider + 'static>(
    sessions: &HashMap<SessionId, Arc<StdMutex<Option<SessionRuntime<P>>>>>,
    session_id: SessionId,
    emitter: &Emitter,
) {
    let Some(arc) = sessions.get(&session_id) else {
        return;
    };
    let Ok(mut guard) = arc.lock() else { return };
    let Some(s) = guard.as_mut() else { return };
    let messages = s
        .conversation
        .history()
        .iter()
        .map(|m| match m {
            tpi_core::message::ChatMessage::User(text) => tpi_protocol::ChatMessageDto {
                role: tpi_protocol::MessageRoleDto::User,
                content: text.clone(),
            },
            tpi_core::message::ChatMessage::Assistant { content, .. } => {
                tpi_protocol::ChatMessageDto {
                    role: tpi_protocol::MessageRoleDto::Assistant,
                    content: content.clone(),
                }
            }
            tpi_core::message::ChatMessage::System(text) => tpi_protocol::ChatMessageDto {
                role: tpi_protocol::MessageRoleDto::System,
                content: text.clone(),
            },
            tpi_core::message::ChatMessage::Tool { name, content, .. } => {
                tpi_protocol::ChatMessageDto {
                    role: tpi_protocol::MessageRoleDto::Tool,
                    content: format!("[{name}] {content}"),
                }
            }
        })
        .collect();
    emitter.emit(RuntimeEvent::SessionHistory {
        session_id,
        messages,
    });
}

// ===== 执行一次 agent run =====

/// 一次 run 执行的上下文（打包 execute_agent_run 的参数）。
struct RunContext<'a> {
    run_id: RunId,
    message: String,
    cancel: CancellationToken,
    config: &'a Config,
    emitter: Emitter,
    sessions_root: &'a std::path::Path,
    workspace_root: &'a Utf8PathBuf,
}

async fn execute_agent_run<P: Provider + 'static>(
    session: &mut SessionRuntime<P>,
    ctx: RunContext<'_>,
) -> Result<AgentOutcome, String> {
    let RunContext {
        run_id,
        message,
        cancel,
        config,
        emitter,
        sessions_root,
        workspace_root,
    } = ctx;
    // 确保 durable session 已创建（CreateSession 只建内存态；首次提交才落盘）。
    session
        .conversation
        .ensure_started(sessions_root, workspace_root)
        .map_err(|e| format!("会话创建失败: {e}"))?;
    let (session_log, history) = session
        .conversation
        .parts_for_run()
        .map_err(|e| format!("conversation 准备失败: {e}"))?;
    let session_id = session_log.session_id();

    let (live_tx, live_rx) = mpsc::channel(256);
    let forward_emitter = emitter.clone();
    let forwarder = tokio::spawn(forward_live_events(
        live_rx,
        forward_emitter,
        session_id,
        run_id,
    ));

    let registry = session.registry.clone();
    let processes = session.processes.clone();
    let terminals = session.terminals.clone();
    let result = agent::run(
        &mut session.provider,
        session_log,
        config,
        tpi_agent::agent::RunInput {
            history,
            user_message: message,
            ui: live_tx,
            cancel,
            interactive: true,
            force_compaction: false,
            workspace: None,
            registry,
            processes,
            terminals,
        },
    )
    .await
    .map_err(|f| f.to_string());

    // run 结束后从 durable log 刷新投影。
    session
        .conversation
        .refresh_from_log()
        .map_err(|e| format!("refresh_from_log 失败: {e}"))?;

    // 收净尾部事件。
    let _ = forwarder.await;
    result
}

/// 转发循环：消费 agent LiveEvent 并转写为协议 RuntimeEvent。
async fn forward_live_events(
    mut live_rx: mpsc::Receiver<LiveEvent>,
    emitter: Emitter,
    session_id: SessionId,
    run_id: RunId,
) {
    while let Some(ev) = live_rx.recv().await {
        forward_live_event(ev, &emitter, session_id, run_id);
    }
}

fn forward_live_event(ev: LiveEvent, emitter: &Emitter, session_id: SessionId, run_id: RunId) {
    let proto = match ev {
        LiveEvent::StepStarted { .. } => None,
        LiveEvent::AssistantDelta {
            request_id,
            kind,
            text,
        } => Some(RuntimeEvent::AssistantDelta {
            session_id,
            run_id,
            request_id,
            kind: match kind {
                tpi_agent::agent::DeltaKind::Text => DeltaKind::Text,
                tpi_agent::agent::DeltaKind::Reasoning => DeltaKind::Reasoning,
            },
            text,
        }),
        LiveEvent::ToolStarted {
            call_id,
            name,
            arguments,
        } => Some(RuntimeEvent::ToolStarted {
            session_id,
            run_id,
            call_id,
            name,
            arguments,
        }),
        LiveEvent::ToolCompleted {
            call_id,
            name,
            status,
            duration_ms,
            exit_code,
            output,
            diff,
        } => Some(RuntimeEvent::ToolCompleted {
            session_id,
            run_id,
            call_id,
            name,
            status: tool_status_to_proto(status),
            duration_ms,
            exit_code,
            output,
            diff,
        }),
        LiveEvent::ToolOutputDelta {
            call_id,
            stream,
            text,
        } => Some(RuntimeEvent::ToolOutputDelta {
            session_id,
            run_id,
            call_id,
            stream,
            text,
        }),
        LiveEvent::ContextUsage { projected, usable } => Some(RuntimeEvent::ContextUsage {
            session_id,
            projected,
            usable,
        }),
        LiveEvent::UsageUpdated {
            input_tokens,
            output_tokens,
            cache_read_tokens,
        } => Some(RuntimeEvent::UsageUpdated {
            session_id,
            input_tokens,
            output_tokens,
            cache_read_tokens,
        }),
        LiveEvent::BudgetWarning => Some(RuntimeEvent::BudgetWarning { session_id }),
        LiveEvent::PlanUpdated { plan } => Some(RuntimeEvent::PlanUpdated {
            session_id,
            plan: serde_json::to_value(plan).unwrap_or(serde_json::Value::Null),
        }),
        LiveEvent::StreamRecovering { attempt } => Some(RuntimeEvent::StreamRecovering {
            session_id,
            attempt,
        }),
        LiveEvent::TurnRestarting { attempt } => Some(RuntimeEvent::TurnRestarting {
            session_id,
            attempt,
        }),
        LiveEvent::CompactionNotice { message } => Some(RuntimeEvent::CompactionNotice {
            session_id,
            message,
        }),
        LiveEvent::SubagentReported {
            child_session,
            summary,
            evidence,
        } => Some(RuntimeEvent::SubagentReported {
            child_session,
            summary,
            evidence,
        }),
    };
    if let Some(ev) = proto {
        emitter.emit(ev);
    }
}

fn tool_status_to_proto(status: ToolStatus) -> ToolState {
    match status {
        ToolStatus::Succeeded => ToolState::Success,
        ToolStatus::Failed | ToolStatus::TimedOut => ToolState::Failed,
        ToolStatus::Cancelled | ToolStatus::Interrupted => ToolState::Cancelled,
        ToolStatus::Rejected => ToolState::Skipped,
    }
}

// ===== 辅助函数 =====

fn last_event_is_awaiting_input(log: &SessionLog) -> bool {
    use tpi_session::store::SessionStore;
    let events = match log.events_with_seq() {
        Ok(v) => v,
        Err(_) => return false,
    };
    events.iter().rev().any(|(_, e)| {
        matches!(
            e,
            SessionEvent::RunCompleted {
                reason: CompletionReason::AwaitingUserInput,
                ..
            }
        )
    })
}

fn project_question(
    q: &tpi_capabilities::tool::request_input::RequestInputQuestion,
) -> QuestionDto {
    QuestionDto {
        question: q.question.clone(),
        header: q.header.clone(),
        options: q
            .options
            .iter()
            .map(|o| QuestionOptionDto {
                label: o.label().to_string(),
                description: {
                    let d = o.description();
                    if d.is_empty() {
                        None
                    } else {
                        Some(d.to_string())
                    }
                },
            })
            .collect(),
        multiple: q.multiple,
        custom: q.custom || q.options.is_empty(),
    }
}

fn completion_reason_to_dto(reason: CompletionReason) -> CompletionReasonDto {
    match reason {
        CompletionReason::Stop => CompletionReasonDto::Stop,
        CompletionReason::Cancelled => CompletionReasonDto::Cancelled,
        CompletionReason::Error => CompletionReasonDto::Error,
        CompletionReason::ProviderInterrupted => CompletionReasonDto::ProviderInterrupted,
        CompletionReason::ProviderUnavailable => CompletionReasonDto::ProviderUnavailable,
        CompletionReason::ContextOverflow => CompletionReasonDto::ContextOverflow,
        CompletionReason::MaxTurns => CompletionReasonDto::MaxTurns,
        CompletionReason::MaxToolCalls => CompletionReasonDto::MaxToolCalls,
        CompletionReason::WallTimeExceeded => CompletionReasonDto::WallTimeExceeded,
        CompletionReason::AwaitingUserInput => CompletionReasonDto::AwaitingUserInput,
    }
}

fn read_session_title(path: &std::path::Path) -> String {
    use tpi_session::store::read_events;
    let events = match read_events(path) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    events
        .iter()
        .find_map(|e| match e {
            SessionEvent::UserSubmitted { content } => {
                let t = content.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.chars().take(60).collect())
                }
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn unix_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ===== Mutation Journal undo/redo =====

/// undo/redo 方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalAction {
    Undo,
    Redo,
}
