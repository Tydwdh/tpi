//! Agent 状态机与模型—工具执行循环。
//!
//! §6.2 一轮的精确算法：接收用户消息 → append UserSubmitted → 构建 context →
//! 发起一次 provider request → 消费规范化 stream → 原子提交 assistant message →
//! 若有 tool calls：校验、调度执行、按原 index 回填 tool messages → 再次请求；
//! 若无 tool call 且 finish=stop，立即完成 run，绝不追加第二次模型请求（§3.2 不变量 2）。

use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;

pub mod limits;
pub mod scheduler;
mod tool_runtime;
use self::tool_runtime::{BatchEnd, ToolBatchExecutor, ToolRuntime};
use crate::ids::{EventId, RequestId, ToolCallId};
use crate::provider::{ChatMessage, ModelRequest, Provider, ProviderEvent};
use crate::session::{
    self, AssistantMessage, CompletionReason, ModelRef, RunLimits, SessionEvent, SessionLog, Usage,
};
use crate::tool::outcome::{StoredToolOutcome, ToolStatus};
use crate::tool::{self, BuiltinTool};

/// Tokio 的 JoinHandle 在 drop 时会脱离而不是取消；run 的 watchdog 必须随
/// 任意返回路径终止，避免失败后仍发送警告或取消已结束的 token。
struct AbortTaskOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 内建 system prompt（§23 草案）。
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

/// §4.3 第二阶段：text-only attempt 断联后的最大自动续写次数。
/// 只允许一次（防无限续写循环）；已出现 tool delta 的 attempt 不自动续。
pub const MAX_STREAM_RECOVERIES: u32 = 1;

/// §4.3 第三阶段：partial tool-call 后整个 model turn 重新生成的最大次数。
/// 只允许一次（防无限 restart）；tool-call 场景风险更大，恢复一次后仍失败
/// 就如实上报 ProviderInterrupted。
pub const MAX_TURN_RESTARTS: u32 = 1;

/// 续写请求注入的 recovery instruction（§4.3：harness control metadata，
/// 不进 durable conversation，不进 session）。
const STREAM_RECOVERY_INSTRUCTION: &str = "\
The previous assistant generation was interrupted by a transport failure.

Partial assistant output already shown to the user:
---
{partial}
---

Continue the same response from where it was interrupted.
Do not repeat the already emitted text.";

/// 单轮 run 的结果。
pub struct AgentOutcome {
    pub reason: CompletionReason,
    pub usage: Usage,
    /// 本轮新增的对话消息（供调用方保存/继续）。
    pub messages: Vec<ChatMessage>,
    /// 最终 assistant 文本（UI 展示）。
    pub assistant_text: String,
}

/// Inputs that describe one agent run independently of its provider, session,
/// and immutable configuration dependencies.
pub struct RunInput<'a> {
    pub history: &'a [ChatMessage],
    pub user_message: String,
    pub ui: mpsc::Sender<RuntimeEvent>,
    pub cancel: CancellationToken,
    pub interactive: bool,
    /// P1-10：手动 `/compact`——无条件在第一个完整边界执行一次压缩。
    pub force_compaction: bool,
}

/// 不可恢复的 run 失败（§19.1）。
#[derive(Debug, thiserror::Error)]
pub enum RunFailure {
    #[error("provider failure: {0}")]
    Provider(String),
    #[error("tool infrastructure failure: {0}")]
    ToolInfrastructure(String),
    #[error("session failure: {0}")]
    Session(String),
    #[error("budget exceeded: {0}")]
    BudgetExceeded(BudgetKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    MaxTurns,
    MaxToolCalls,
}

impl std::fmt::Display for BudgetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BudgetKind::MaxTurns => write!(f, "max_turns"),
            BudgetKind::MaxToolCalls => write!(f, "max_tool_calls"),
        }
    }
}

/// ephemeral 运行时事件（§4.3：不逐 token 写盘）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// 每次实际 provider 请求前发送，供 TUI 更新运行状态。
    TurnStarted { turn: u32 },
    AssistantDelta {
        request_id: RequestId,
        kind: DeltaKind,
        text: String,
    },
    /// 工具真正启动前发送（TUI 创建运行中的工具卡片）。
    ToolStarted {
        call_id: ToolCallId,
        name: String,
        /// 主行 target 摘要（整改 A3：压缩后的命令或 `name path`）。
        target: String,
        /// 完整命令（详情 overlay 用；非 bash 为 None）。
        command: Option<String>,
    },
    /// 工具终态（TUI 更新卡片状态/耗时；失败时附带关键输出 tail）。
    ToolCompleted {
        call_id: ToolCallId,
        name: String,
        status: ToolStatus,
        duration_ms: u64,
        exit_code: Option<i32>,
        tail: String,
        /// edit/write 的 unified diff（§用户诉求：默认展开红绿 diff）。
        diff: Option<String>,
    },
    /// 工具执行中的实时输出增量（bash stdout/stderr；TUI 卡片运行中可见）。
    ToolOutputDelta {
        call_id: ToolCallId,
        stream: u8,
        text: String,
    },
    /// 上下文占用投影（每次请求前发送；TUI 绘制用量条）。
    ContextUsage { projected: u64, usable: u64 },
    /// 接近 wall-time 预算（P1-3：TUI 状态栏/系统行提示，此前只写日志）。
    BudgetWarning,
    /// `update_plan` 提交后的独立 UI 状态；不作为聊天流水的一部分。
    PlanUpdated { plan: crate::tool::plan::Plan },
    /// 流中断后正在自动续写（第二阶段 §4.3：text-only attempt 恢复）。
    StreamRecovering { attempt: u32 },
    /// partial tool-call 后整个 model turn 重新生成（第三阶段 §4.3）。
    /// UI 应丢弃当前 attempt 的 partial 展示（不进 transcript）。
    TurnRestarting { attempt: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// 执行一次完整 run（§6.2）。
pub async fn run<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    input: RunInput<'_>,
) -> Result<AgentOutcome, RunFailure> {
    let RunInput {
        history,
        user_message,
        ui,
        cancel,
        interactive,
        force_compaction,
    } = input;
    let run_id = session.begin_run();
    let span = tracing::info_span!("agent.run", %run_id);
    let _enter = span.enter();
    // §44：run 级耗时基线。
    let run_started = std::time::Instant::now();

    // 1. 用户提交（durable boundary）。
    // 空 user_message = retry 语义（`/retry`）：复用已有 history（上次失败的 turn
    // 不重复记录 UserSubmitted，也不追加新 User 消息）；仍记录 RunStarted 作为新 attempt。
    // app 层已拦截空输入，正常对话不会传空消息。
    if !user_message.is_empty() {
        session
            .append_event(&SessionEvent::UserSubmitted {
                content: user_message.clone(),
            })
            .and_then(|_| session.sync_data())
            .map_err(|e| RunFailure::Session(e.to_string()))?;
    }
    session
        .append_event(&SessionEvent::RunStarted {
            model: ModelRef {
                name: config.model.name.clone(),
                provider: config.model.provider.clone(),
            },
            limits: RunLimits {
                max_turns: config.limits.max_model_turns,
                max_tool_calls: config.limits.max_tool_calls,
            },
        })
        .and_then(|_| session.sync_data())
        .map_err(|e| RunFailure::Session(e.to_string()))?;

    // watchdog 必须覆盖手动 compaction 在内的整个模型工作阶段，并通过
    // AbortTaskOnDrop 保证所有 `?`/early return 路径都停止后台任务。
    let cancel_cause = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
        crate::agent::limits::CANCEL_CAUSE_USER,
    ));
    let warn_ui = ui.clone();
    let cause_for_watchdog = cancel_cause.clone();
    let (watchdog, _wall) = crate::agent::limits::spawn_watchdog(
        &config.limits,
        cancel.clone(),
        move || {
            cause_for_watchdog.store(
                crate::agent::limits::CANCEL_CAUSE_WALL_TIME,
                std::sync::atomic::Ordering::SeqCst,
            );
        },
        move || {
            tracing::info!("run approaching wall-time budget");
            let _ = warn_ui.try_send(RuntimeEvent::BudgetWarning);
        },
    );
    let watchdog = AbortTaskOnDrop(watchdog);

    let mut usage_total = Usage::default();
    let mut messages: Vec<ChatMessage> = history.to_vec();
    if !user_message.is_empty() {
        messages.push(ChatMessage::User(user_message.clone()));
    }
    // P1-10：手动 /compact——在第一个完整边界无条件执行一次压缩。
    // 失败（历史不足/不显著）不中断 run，只记录日志。
    if force_compaction && messages.len() > 1 {
        match compact_turn(
            provider,
            &mut messages,
            session,
            config,
            &cancel,
            &mut usage_total,
        )
        .await
        {
            Ok(()) => {}
            Err(error @ RunFailure::Session(_)) => return Err(error),
            Err(error) => tracing::warn!(error = %error, "manual compaction failed"),
        }
    }
    // 工具共享状态与 ToolContext 构造由 run-scoped runtime 统一管理。
    let tool_runtime = ToolRuntime::new(
        config,
        session.session_id().to_string(),
        cancel.clone(),
        interactive,
    );

    let mut turn = 0u32;
    let mut tool_calls_total = 0u32;
    let tools = tool::implemented_tools();
    let tool_defs: Vec<crate::provider::ToolDef> = tools.iter().map(BuiltinTool::schema).collect();

    let mut assistant_text = String::new();
    // §12.3：确定性无进展检测（不调用额外模型）。
    let mut progress = crate::agent::scheduler::ProgressTracker::default();
    // §15.4：同一阈值区间内 compaction 失败后不反复调用模型。
    let mut compaction_failed = false;
    let final_reason: CompletionReason = 'run_loop: loop {
        if turn >= config.limits.max_model_turns {
            session
                .append_event(&SessionEvent::RunCompleted {
                    reason: CompletionReason::MaxTurns,
                    usage: usage_total,
                })
                .and_then(|_| session.sync_data())
                .map_err(|e| RunFailure::Session(e.to_string()))?;
            break CompletionReason::MaxTurns;
        }
        turn += 1;
        // 一个 request_id 标识一个逻辑 model turn；自动续写/restart 沿用它，
        // 下一轮（通常在工具结果之后）必须分配新 id。
        let request_id = RequestId::new_v7();
        let _ = ui.send(RuntimeEvent::TurnStarted { turn }).await;

        // §15.4：compaction 检查（只在下一次请求前；完整 boundary 之后）。
        // P0-9：投影用请求级估算（system prompt + 计划快照 + 工具 schema），
        // 只算 messages 会低估实际请求，导致 compaction 触发过晚。
        if let Some(context_window) = config.model.context_window {
            let usable = crate::context::usable_input(
                context_window,
                config.model.max_output_tokens.unwrap_or(0) as u64,
                config.safety_reserve_tokens,
            );
            let plan = tool_runtime.plan_snapshot();
            let plan_text = crate::tool::plan::plan_snapshot(plan.as_ref());
            let system_prompt = system_prompt_text(config, None);
            let projected = crate::context::estimate_request(
                &system_prompt,
                &messages,
                &tool_defs,
                Some(&plan_text),
            );
            if crate::context::should_compact(projected, usable) && !compaction_failed {
                match compact_turn(
                    provider,
                    &mut messages,
                    session,
                    config,
                    &cancel,
                    &mut usage_total,
                )
                .await
                {
                    Ok(()) => {
                        // P1-4：compaction 成功后若仍无法容纳（窗口过小），
                        // 不再发起普通请求（必然 length error），明确结束并提示用户。
                        let plan = tool_runtime.plan_snapshot();
                        let plan_text = crate::tool::plan::plan_snapshot(plan.as_ref());
                        let system_prompt = system_prompt_text(config, None);
                        let after = crate::context::estimate_request(
                            &system_prompt,
                            &messages,
                            &tool_defs,
                            Some(&plan_text),
                        );
                        if after > usable {
                            session
                                .append_event(&SessionEvent::RunCompleted {
                                    reason: CompletionReason::ContextOverflow,
                                    usage: usage_total,
                                })
                                .and_then(|_| session.sync_data())
                                .map_err(|e| RunFailure::Session(e.to_string()))?;
                            break 'run_loop CompletionReason::ContextOverflow;
                        }
                    }
                    Err(error @ RunFailure::Session(_)) => return Err(error),
                    Err(error) => {
                        // §15.4 第 6 条：失败不循环；继续确定性 prune，仍无法容纳则明确停止。
                        compaction_failed = true;
                        tracing::warn!(error = %error, "compaction failed; not retrying in this band");
                        // 确定性 prune 兜底（§15.3：只影响投影）。
                        messages = crate::context::prune_messages(messages);
                        // P1-4：prune 后仍超窗口（如 user 消息本身巨大）→ 明确结束。
                        let plan = tool_runtime.plan_snapshot();
                        let plan_text = crate::tool::plan::plan_snapshot(plan.as_ref());
                        let system_prompt = system_prompt_text(config, None);
                        let after = crate::context::estimate_request(
                            &system_prompt,
                            &messages,
                            &tool_defs,
                            Some(&plan_text),
                        );
                        if after > usable {
                            session
                                .append_event(&SessionEvent::RunCompleted {
                                    reason: CompletionReason::ContextOverflow,
                                    usage: usage_total,
                                })
                                .and_then(|_| session.sync_data())
                                .map_err(|e| RunFailure::Session(e.to_string()))?;
                            break 'run_loop CompletionReason::ContextOverflow;
                        }
                    }
                }
            }
        }

        // 3. 构建 context projection 并发起请求（§6.2 第 3-4 步）。
        // §4.3 attempt 模型：text-only 断联 → 续写（stream_recoveries）；
        // partial tool-call 断联 → 整个 turn 重新生成（turn_restarts）。
        // `content`/`saw_any_semantic`/`saw_tool_calls` 跨 attempt 累积
        // （整个 model turn 的事实）；restart 会清空 content 重新发起原始请求。
        let mut stream_recoveries: u32 = 0;
        let mut turn_restarts: u32 = 0;
        let mut content = String::new();
        let mut saw_any_semantic = false;
        let mut saw_tool_calls = false;

        let response = 'attempt: loop {
            // 续写 attempt 的 recovery instruction 是 harness control metadata：
            // 作为 ephemeral system instruction 注入 build_context（不进 session、
            // 不进对话投影），而不是伪装成 User 消息。
            let ephemeral_system = if stream_recoveries == 0 {
                None
            } else {
                Some(STREAM_RECOVERY_INSTRUCTION.replace("{partial}", &content))
            };
            let plan = tool_runtime.plan_snapshot();
            let request = ModelRequest {
                model: config.model.name.clone(),
                messages: build_context(
                    config,
                    &messages,
                    plan.as_ref(),
                    ephemeral_system.as_deref(),
                ),
                tools: tool_defs.clone(),
                max_output_tokens: config.model.max_output_tokens,
                reasoning: config.model.reasoning.clone(),
                context_window: config.model.context_window,
            };
            // 上下文占用投影（TUI 用量条；请求前发送）。
            if let Some(window) = config.model.context_window {
                let usable = crate::context::usable_input(
                    window,
                    config.model.max_output_tokens.unwrap_or(0) as u64,
                    config.safety_reserve_tokens,
                );
                let plan = tool_runtime.plan_snapshot();
                let plan_text = crate::tool::plan::plan_snapshot(plan.as_ref());
                let system_prompt = system_prompt_text(config, ephemeral_system.as_deref());
                let projected = crate::context::estimate_request(
                    &system_prompt,
                    &messages,
                    &tool_defs,
                    Some(&plan_text),
                );
                let _ = ui
                    .send(RuntimeEvent::ContextUsage { projected, usable })
                    .await;
            }
            let (event_tx, mut event_rx) = mpsc::channel(crate::provider::EVENT_CHANNEL_CAPACITY);

            // 必须在 provider 请求进行时消费 channel：等请求结束后再 drain 不但不是真正
            // streaming，达到 channel 容量时还会让 provider 与 agent 相互等待。
            let stream = provider.stream(request, event_tx, cancel.clone());
            tokio::pin!(stream);
            // §4.3：流中断分类需要知道是否已产生语义内容（文本/工具调用）。
            // 统一消费入口：主 select 分支与所有 drain 分支共用，保证标志与 UI 同步。
            loop {
                tokio::select! {
                    Some(event) = event_rx.recv() => {
                        consume_stream_event(
                            event,
                            request_id,
                            &mut content,
                            &mut saw_any_semantic,
                            &mut saw_tool_calls,
                            &ui,
                        )
                        .await;
                    }
                    result = &mut stream => {
                        let response = match result {
                            Ok(response) => response,
                            Err(crate::provider::ProviderError::Cancelled) => {
                                // 先收完 provider 返回前已入队的残余 delta（与 Ok 分支一致），
                                // 否则取消时已到达的文本可能丢失（P1-1 测试场景）。
                                while let Ok(event) = event_rx.try_recv() {
                                    consume_stream_event(
                                        event,
                                        request_id,
                                        &mut content,
                                        &mut saw_any_semantic,
                                        &mut saw_tool_calls,
                                        &ui,
                                    )
                                    .await;
                                }
                                // §6.2/§11.5：取消（Esc/Ctrl-C）是正常结束——提交已到达的内容，
                                // 记录 Cancelled 原因并保留 session，而不是让 run 以错误退出。
                                if !content.is_empty() {
                                    assistant_text = content.clone();
                                    session
                                        .append_event(&SessionEvent::AssistantMessageCommitted {
                                            message: AssistantMessage {
                                                content: content.clone(),
                                                tool_calls: Vec::new(),
                                            },
                                        })
                                        .and_then(|_| session.sync_data())
                                        .map_err(|e| RunFailure::Session(e.to_string()))?;
                                    // P1-1：已提交 session 的内容必须同步进 outcome.messages，
                                    // 否则继续对话时模型上下文与 session 事实不一致。
                                    messages.push(ChatMessage::Assistant {
                                        content: content.clone(),
                                        tool_calls: Vec::new(),
                                    });
                                // §16：区分取消来源——watchdog 超时不是用户取消。
                                }
                                let cause = cancel_cause.load(std::sync::atomic::Ordering::SeqCst);
                                let cancel_reason =
                                    crate::agent::limits::cancel_reason_for_cause(cause);
                                session
                                    .append_event(&SessionEvent::RunCompleted {
                                        reason: cancel_reason,
                                        usage: usage_total,
                                    })
                                    .and_then(|_| session.sync_data())
                                    .map_err(|e| RunFailure::Session(e.to_string()))?;
                                break 'run_loop cancel_reason;
                            }
                            Err(e) => {
                                // 错误详情记录到日志（§19.2：provider 错误可诊断）。
                                tracing::error!(%request_id, error = %e, "provider request failed");
                                // 先收完已入队的残余 delta（与 Cancelled 分支一致）。
                                while let Ok(event) = event_rx.try_recv() {
                                    consume_stream_event(
                                        event,
                                        request_id,
                                        &mut content,
                                        &mut saw_any_semantic,
                                        &mut saw_tool_calls,
                                        &ui,
                                    )
                                    .await;
                                }
                            // §4.3 第二阶段：text-only attempt 断联 → 自动续写一次。
                            // 条件：已产生文本（非连接不可用）、未出现 tool delta（partial
                            // JSON 恢复风险大）、还有续写额度。
                            if saw_any_semantic
                                && !saw_tool_calls
                                && recoverable_stream_interrupt(&e)
                                && stream_recoveries < MAX_STREAM_RECOVERIES
                            {
                                let cause = interrupt_cause(&e);
                                session
                                    .append_event(&SessionEvent::AssistantAttemptInterrupted {
                                        request_id,
                                        content: content.clone(),
                                        cause,
                                        saw_tool_calls: false,
                                    })
                                    .and_then(|_| session.sync_data())
                                    .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                let _ = ui
                                    .send(RuntimeEvent::StreamRecovering {
                                        attempt: stream_recoveries + 1,
                                    })
                                    .await;
                                stream_recoveries += 1;
                                // 续写请求（content 已累积 partial，recovery instruction
                                // 在下一个 attempt 循环开头注入 request）。
                                continue 'attempt;
                            }
                            // §4.3 第三阶段：partial tool-call 后断联 → 整个 turn 重新生成。
                            // 条件：已出现 tool delta（saw_tool_calls=true）、还有 restart 额度。
                            // 不尝试续接 partial JSON（风险大）；重新发起原始请求。
                            if saw_tool_calls
                                && recoverable_stream_interrupt(&e)
                                && turn_restarts < MAX_TURN_RESTARTS
                            {
                                let cause = interrupt_cause(&e);
                                session
                                    .append_event(&SessionEvent::AssistantAttemptInterrupted {
                                        request_id,
                                        content: content.clone(),
                                        cause,
                                        saw_tool_calls: true,
                                    })
                                    .and_then(|_| session.sync_data())
                                    .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                let _ = ui
                                    .send(RuntimeEvent::TurnRestarting {
                                        attempt: turn_restarts + 1,
                                    })
                                    .await;
                                turn_restarts += 1;
                                // 清空本 attempt 的 partial（UI 已 discard；content 重置），
                                // 下一个 attempt 以原始 request 重新生成整个 turn。
                                content.clear();
                                saw_any_semantic = false;
                                saw_tool_calls = false;
                                continue 'attempt;
                            }
                                // §4.3：区分"未收到任何语义事件"（连接不可用）与
                                // "已收到部分内容后断联"（记录 interrupted attempt）。
                                // partial content 已发给 UI 但不是一个完整 turn：
                                // 不提交为 AssistantMessageCommitted，写入记录型事件。
                                if saw_any_semantic {
                                    let cause = interrupt_cause(&e);
                                    session
                                        .append_event(&SessionEvent::AssistantAttemptInterrupted {
                                            request_id,
                                            content: content.clone(),
                                            cause,
                                            saw_tool_calls,
                                        })
                                        .and_then(|_| session.sync_data())
                                        .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                    session
                                        .append_event(&SessionEvent::RunCompleted {
                                            reason: CompletionReason::ProviderInterrupted,
                                            usage: usage_total,
                                        })
                                        .and_then(|_| session.sync_data())
                                        .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                    // 保留 partial 供 UI/outcome 展示，但不动 messages（不是已提交事实）。
                                    assistant_text = content.clone();
                                    break 'run_loop CompletionReason::ProviderInterrupted;
                                }
                                session
                                    .append_event(&SessionEvent::RunCompleted {
                                        reason: CompletionReason::ProviderUnavailable,
                                        usage: usage_total,
                                    })
                                    .and_then(|_| session.sync_data())
                                    .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                return Err(RunFailure::Provider(e.to_string()));
                            }
                        };
                        // provider 返回后仍可能有已经入队的末尾 delta；这些发送都已完成，
                        // 因而非阻塞 drain 即可，也不会依赖 future 内部 Sender 的析构时机。
                        while let Ok(event) = event_rx.try_recv() {
                            consume_stream_event(
                                event,
                                request_id,
                                &mut content,
                                &mut saw_any_semantic,
                                &mut saw_tool_calls,
                                &ui,
                            )
                            .await;
                        }
                        break 'attempt response;
                    }
                }
            }
        };
        // provider 返回的 usage 是本次请求的用量；跨轮累加（§2.2/§12.4）。
        accumulate_usage(&mut usage_total, response.usage);
        // §27：每个 model turn 至少记录可回答“哪一轮、什么模型、多少工具、
        // 什么 finish reason、多少 token”的诊断行。
        tracing::debug!(
            turn,
            model = %config.model.name,
            tool_count = response.tool_calls.len(),
            finish_reason = ?response.finish_reason,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "model turn completed"
        );

        // 5. 原子提交 assistant turn（durable boundary，§14.2）。
        // P0-2：即使 content 为空（纯 tool-call 轮）也必须提交——assistant turn
        // 是 provider 协议的最小原子单元，缺失会导致 resume 重建出非法消息序列。
        // P0-3：runtime context 与 session 事实一致——assistant 消息同时携带
        // content 与 tool_calls（此前纯文本轮完全没有 assistant 消息，
        // text+tool 轮 push 的 content 为空，live projection != resume projection）。
        assistant_text = content.clone();
        session
            .append_event(&SessionEvent::AssistantMessageCommitted {
                message: AssistantMessage {
                    content: content.clone(),
                    tool_calls: response.tool_calls.clone(),
                },
            })
            .and_then(|_| session.sync_data())
            .map_err(|e| RunFailure::Session(e.to_string()))?;
        messages.push(ChatMessage::Assistant {
            content: content.clone(),
            tool_calls: response.tool_calls.clone(),
        });

        // 6. 工具调用（§12.2 batch 调度）。
        if !response.tool_calls.is_empty() {
            let batch = ToolBatchExecutor::new(
                config,
                session,
                &mut messages,
                &mut progress,
                &tool_runtime,
                &ui,
            )
            .execute(response.tool_calls, &mut tool_calls_total)
            .await?;
            if batch == BatchEnd::BudgetExceeded {
                // P1-2：工具预算超限用独立 reason（此前归为 Error，
                // 用户/模型会误以为是协议错误）。
                session
                    .append_event(&SessionEvent::RunCompleted {
                        reason: CompletionReason::MaxToolCalls,
                        usage: usage_total,
                    })
                    .and_then(|_| session.sync_data())
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
                break 'run_loop CompletionReason::MaxToolCalls;
            }
            continue;
        }

        // 7. 无 tool call：按 finish reason 结束（§6.2 第 7-8 步）。
        let reason = match response.finish_reason {
            crate::provider::FinishReason::Stop => CompletionReason::Stop,
            crate::provider::FinishReason::ToolCalls => CompletionReason::Error,
            crate::provider::FinishReason::Length
            | crate::provider::FinishReason::ContentFilter
            | crate::provider::FinishReason::Error => CompletionReason::Error,
        };
        session
            .append_event(&SessionEvent::RunCompleted {
                reason,
                usage: usage_total,
            })
            .and_then(|_| session.sync_data())
            .map_err(|e| RunFailure::Session(e.to_string()))?;
        break reason;
    };

    watchdog.0.abort();
    // §27/§44：run 级汇总（turn/tool 计数、总 token、总耗时）——性能基线的最小记录。
    tracing::info!(
        run_id = %run_id,
        reason = ?final_reason,
        turns = turn,
        tool_calls = tool_calls_total,
        input_tokens = usage_total.input_tokens,
        output_tokens = usage_total.output_tokens,
        elapsed_ms = run_started.elapsed().as_millis() as u64,
        "agent run completed"
    );
    Ok(AgentOutcome {
        reason: final_reason,
        usage: usage_total,
        messages,
        assistant_text,
    })
}

/// 消费一个 provider 流事件：更新语义内容标志并投影到 UI。
///
/// 主 select 分支与所有 drain 分支共用，保证 `saw_any_semantic`/`saw_tool_calls`
/// 与实际已消费事件一致（§4.3：断联分类依赖该标志）。
async fn consume_stream_event(
    event: ProviderEvent,
    request_id: RequestId,
    content: &mut String,
    saw_any_semantic: &mut bool,
    saw_tool_calls: &mut bool,
    ui: &mpsc::Sender<RuntimeEvent>,
) {
    match &event {
        ProviderEvent::TextDelta(_) => *saw_any_semantic = true,
        ProviderEvent::ToolCallStarted { .. } | ProviderEvent::ToolArgumentsDelta { .. } => {
            *saw_any_semantic = true;
            *saw_tool_calls = true;
        }
        ProviderEvent::ReasoningDelta(_) | ProviderEvent::Usage(_) => {}
    }
    forward_provider_event(event, request_id, content, ui).await;
}

/// 把 provider 的单个增量同时投影到 session 待提交内容和 TUI。
async fn forward_provider_event(
    event: ProviderEvent,
    request_id: RequestId,
    content: &mut String,
    ui: &mpsc::Sender<RuntimeEvent>,
) {
    match event {
        ProviderEvent::TextDelta(text) => {
            content.push_str(&text);
            let _ = ui
                .send(RuntimeEvent::AssistantDelta {
                    request_id,
                    kind: DeltaKind::Text,
                    text,
                })
                .await;
        }
        ProviderEvent::ReasoningDelta(text) => {
            // §15.5：reasoning 只在 UI 展示，不进入 durable facts。
            let _ = ui
                .send(RuntimeEvent::AssistantDelta {
                    request_id,
                    kind: DeltaKind::Reasoning,
                    text,
                })
                .await;
        }
        ProviderEvent::ToolCallStarted { .. }
        | ProviderEvent::ToolArgumentsDelta { .. }
        | ProviderEvent::Usage(_) => {}
    }
}

/// 把 provider 错误分类为会话中断原因（§4.3）。
fn interrupt_cause(error: &crate::provider::ProviderError) -> crate::session::InterruptCause {
    use crate::provider::ProviderError;
    use crate::session::InterruptCause;
    match error {
        ProviderError::Connection(_) => InterruptCause::Connection,
        ProviderError::Http(_) => InterruptCause::Connection,
        ProviderError::Protocol(_) => InterruptCause::Protocol,
        ProviderError::RateLimited(_) => InterruptCause::RateLimited,
        ProviderError::Auth(_) => InterruptCause::Auth,
        ProviderError::Cancelled => InterruptCause::Other,
    }
}

fn recoverable_stream_interrupt(error: &crate::provider::ProviderError) -> bool {
    matches!(error, crate::provider::ProviderError::Connection(_))
}

/// compaction 一轮（§15.4）。
async fn compact_turn<P: Provider>(
    provider: &mut P,
    messages: &mut Vec<ChatMessage>,
    session: &mut SessionLog,
    config: &Config,
    cancel: &CancellationToken,
    usage_total: &mut Usage,
) -> Result<(), RunFailure> {
    // 1. 先 prune 大 tool output（§15.3）。
    // P0-3：以 runtime messages 计算压缩内容（与调用方看到的上下文一致）；
    // 用 session 投影计算 covered 边界——正常路径下两者消息数一致，
    // covered 只覆盖真正被压缩的事件（recent 在 replay 端也保留）；
    // 调用方注入的 history 未落 session 时（如 force 压缩测试场景）
    // 消息数不一致，fallback 覆盖全部压缩前事件（旧语义）。
    let pruned = crate::context::prune_messages(messages.clone());
    // 2. 保留最近约 25% 的完整原始上下文，且不小于两个完整 turns（§15.4 第 3 条）。
    let keep_count = (pruned.len() as f64 * crate::context::KEEP_RECENT_RATIO)
        .ceil()
        .max(crate::context::MIN_KEEP_TURNS as f64) as usize;
    let mut split = pruned.len().saturating_sub(keep_count);
    // P0-3/§18.2：消息单元必须原子——recent 不能以 Tool 消息开头（其
    // Assistant(tool_calls) 载体若被压缩，replay 重建出的 Tool 无协议载体）。
    // 向回调整 split，直到 recent 以 User/Assistant（新单元起点）开头。
    while split > 0 && matches!(pruned[split], ChatMessage::Tool { .. }) {
        split -= 1;
    }
    let history = pruned[..split].to_vec();
    let recent = pruned[split..].to_vec();
    // 覆盖范围：只覆盖 recent 之前的事件（P0-3）。recent 第一条真实消息的
    // 起始 seq 即覆盖边界（exclusive）；summary 前缀消息无事件关联（start=0）
    // 需要跳过；recent 内没有真实消息或投影与 runtime 不一致时
    // fallback 覆盖全部压缩前事件。
    let seq = session.seq();
    let next_seq = seq
        .checked_add(1)
        .ok_or_else(|| RunFailure::Session("session seq 已耗尽".into()))?;
    let events = crate::session::read_events_with_seq(session.path())
        .map_err(|e| RunFailure::Session(e.to_string()))?;
    let projected = crate::session::project_messages_with_ranges(&events);
    let recent_start_seq = if projected.len() == pruned.len() {
        projected[split..]
            .iter()
            .find(|(_, start, _)| *start > 0)
            .map(|(_, start, _)| *start)
            .unwrap_or(next_seq)
    } else {
        next_seq
    };
    let covered = crate::session::EventRange {
        start: EventId::from_u128(1),
        end: EventId::from_u128(recent_start_seq as u128),
    };

    // 3. 用 compaction 角色（默认 primary，§7.2）生成结构化 summary。
    // P0-2：stream 与事件消费必须并发——provider 在同一 task 内 `send().await`，
    // 若先等 stream 返回再收事件，事件数超过 channel 容量时双方互相等待（死锁）。
    let (event_tx, mut event_rx) = mpsc::channel(crate::provider::EVENT_CHANNEL_CAPACITY);
    let request = ModelRequest {
        model: config.model.name.clone(),
        messages: crate::context::compaction_request_messages(&history),
        tools: Vec::new(), // §15.4：compaction request 不提供任何工具 schema。
        max_output_tokens: Some(1024),
        reasoning: config.model.reasoning.clone(),
        context_window: config.model.context_window,
    };
    let stream = provider.stream(request, event_tx, cancel.clone());
    tokio::pin!(stream);
    let mut summary_text = String::new();
    let response = loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                if let ProviderEvent::TextDelta(text) = event {
                    summary_text.push_str(&text);
                }
            }
            result = &mut stream => {
                break result.map_err(|e| RunFailure::Provider(e.to_string()))?;
            }
        }
    };
    // stream 返回后 channel 中可能还有已入队的尾部 delta，全部收完。
    while let Ok(event) = event_rx.try_recv() {
        if let ProviderEvent::TextDelta(text) = event {
            summary_text.push_str(&text);
        }
    }
    accumulate_usage(usage_total, response.usage);
    if response.finish_reason != crate::provider::FinishReason::Stop
        || !response.tool_calls.is_empty()
    {
        return Err(RunFailure::Provider(format!(
            "invalid compaction response: finish={:?}, tool_calls={}",
            response.finish_reason,
            response.tool_calls.len()
        )));
    }
    let summary_text = crate::context::parse_summary(&summary_text);
    let summary_tokens = crate::context::estimate_tokens(&summary_text);
    let original_tokens = crate::context::estimate_messages(&history);
    tracing::debug!(
        original_tokens,
        summary_tokens,
        text_len = summary_text.len(),
        messages = messages.len(),
        "compaction summary generated"
    );

    // 4. 校验必填字段和压缩后估算；只有明显缩小才提交（§15.4 第 5 条）。
    if summary_text.is_empty()
        || !crate::context::is_significant_shrink(original_tokens, summary_tokens)
    {
        return Err(RunFailure::ToolInfrastructure(
            "compaction summary invalid or not significant".into(),
        ));
    }
    session
        .append_event(&SessionEvent::CompactionCommitted {
            covered,
            summary: crate::session::CompactSummary {
                text: summary_text.clone(),
            },
        })
        .and_then(|_| session.sync_data())
        .map_err(|e| RunFailure::Session(e.to_string()))?;

    // 5. 重建投影：最新 summary + 保留的最近 turns（§15.4：旧 raw 不重复注入）。
    *messages = Vec::with_capacity(recent.len() + 1);
    messages.push(ChatMessage::User(format!(
        "（此前会话的压缩摘要，见 CompactionCommitted）
{summary_text}"
    )));
    messages.extend(recent);
    Ok(())
}

fn accumulate_usage(total: &mut Usage, additional: Usage) {
    total.input_tokens = total.input_tokens.saturating_add(additional.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(additional.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(additional.cache_read_tokens);
}

/// 拼接 system prompt 文本（P0-9 提取：预算估算与请求构造共用同一来源）。
///
/// `ephemeral_system` 是 harness control metadata（如续写 recovery instruction）：
/// 只在本次 request 出现，不进 session、不进对话投影，也不伪装成 User 消息。
fn system_prompt_text(config: &Config, ephemeral_system: Option<&str>) -> String {
    let mut system = DEFAULT_SYSTEM_PROMPT.to_string();
    if let Some(extra) = &config.system_prompt_extra {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    if let Some(ephemeral) = ephemeral_system {
        system.push_str("\n\n");
        system.push_str(ephemeral);
    }
    system
}

/// 构造上下文 projection（§15.1 顺序：system → 用户目标 → 历史 turns → 当前输入在尾部）。
///
/// §13：每次 model request 的 runtime snapshot 都包含规范化计划（compaction 或
/// 长对话不会让模型只靠记忆遵循 Todo）。plan 快照不进 system prompt——
/// 它随每次 update 变化，放 system 会破坏 system 前缀缓存（长上下文下
/// 每次请求都要重算 system 部分）；改为以 user 消息追加轨迹尾部。
///
/// `ephemeral_system`：本次 request 的 harness control metadata（§4.3 续写指令），
/// 以 system 指令注入，不进入对话投影。
fn build_context(
    config: &Config,
    messages: &[ChatMessage],
    plan: Option<&crate::tool::plan::Plan>,
    ephemeral_system: Option<&str>,
) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len() + 3);
    out.push(ChatMessage::System(system_prompt_text(
        config,
        ephemeral_system,
    )));
    out.extend_from_slice(messages);
    if let Some(plan) = plan {
        let snapshot = crate::tool::plan::plan_snapshot(Some(plan));
        if !snapshot.is_empty() {
            out.push(ChatMessage::User(snapshot));
        }
    }
    out
}

/// 恢复一个中断 session 后继续：把 Interrupted outcome 注入历史（§4.3）。
pub fn interrupted_as_messages(
    interrupted: &[(String, String, StoredToolOutcome)],
) -> Vec<ChatMessage> {
    interrupted
        .iter()
        .map(|(_call_id, provider_id, outcome)| ChatMessage::Tool {
            tool_call_id: provider_id.clone(),
            name: outcome.session_metadata.tool.clone(),
            content: outcome.model_payload.output.clone(),
        })
        .collect()
}

/// 该 session 的 JSONL 路径（§14.1）。
pub fn session_path(
    sessions_root: &std::path::Path,
    workspace_root: &camino::Utf8PathBuf,
    session_id: crate::ids::SessionId,
) -> PathBuf {
    sessions_root
        .join(session::workspace_id_for(workspace_root.as_std_path()))
        .join(format!("{session_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::AbortTaskOnDrop;

    struct DropProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn abort_task_on_drop_does_not_detach_background_task() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_dropped = dropped.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _probe = DropProbe(task_dropped);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();

        drop(AbortTaskOnDrop(handle));
        for _ in 0..10 {
            if dropped.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "guard drop 必须 abort 并销毁后台 task"
        );
    }
}
