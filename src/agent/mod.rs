//! Agent 状态机与执行循环（文档 §6）。
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
use crate::ids::{EventId, RequestId, ToolCallId};
use crate::provider::{ChatMessage, ModelRequest, Provider, ProviderEvent, ToolCall};
use crate::session::{
    self, AssistantMessage, CompletionReason, ModelRef, RecoveryMetadata, RunLimits, SessionEvent,
    SessionLog, Usage,
};
use crate::tool::outcome::{StoredToolOutcome, ToolOutcome, ToolStatus};
use crate::tool::{self, BuiltinTool};
use camino::Utf8PathBuf;

/// 内建 system prompt（§23 草案）。
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

/// §4.3 第二阶段：text-only attempt 断联后的最大自动续写次数。
/// 只允许一次（防无限续写循环）；已出现 tool delta 的 attempt 不自动续。
pub const MAX_STREAM_RECOVERIES: u32 = 1;

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
#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// 执行一次完整 run（§6.2）。
#[allow(clippy::too_many_arguments)]
pub async fn run<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    history: &[ChatMessage],
    user_message: String,
    ui: mpsc::Sender<RuntimeEvent>,
    cancel: CancellationToken,
    interactive: bool,
    // P1-10：手动 `/compact`——无条件在第一个完整边界执行一次压缩。
    force_compaction: bool,
) -> Result<AgentOutcome, RunFailure> {
    let run_id = session.run_id();
    let request_id = RequestId::new_v7();
    let span = tracing::info_span!("agent.run", %run_id, %request_id);
    let _enter = span.enter();
    // §44：run 级耗时基线。
    let run_started = std::time::Instant::now();

    // 1. 用户提交（durable boundary）。
    session
        .append_event(&SessionEvent::UserSubmitted {
            content: user_message.clone(),
        })
        .and_then(|_| session.sync_data())
        .map_err(|e| RunFailure::Session(e.to_string()))?;
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

    let mut messages: Vec<ChatMessage> = history.to_vec();
    messages.push(ChatMessage::User(user_message.clone()));
    // P1-10：手动 /compact——在第一个完整边界无条件执行一次压缩。
    // 失败（历史不足/不显著）不中断 run，只记录日志。
    if force_compaction
        && messages.len() > 1
        && let Err(error) = compact_turn(provider, &mut messages, session, config, &cancel).await
    {
        tracing::warn!(error = %error, "manual compaction failed");
    }
    // list/search 分页 snapshot（session 作用域，§8.4）。
    let scan_snapshots: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, crate::tool::search::ScanSnapshot>>,
    > = Default::default();
    // §10.1：session-local bounded SnapshotStore。
    let snapshot_store: std::sync::Arc<std::sync::Mutex<crate::tool::edit::SnapshotStore>> =
        Default::default();
    // §13：原子短计划（update_plan 原子替换；每次请求注入 snapshot）。
    let current_plan: std::sync::Arc<std::sync::Mutex<Option<crate::tool::plan::Plan>>> =
        Default::default();

    let mut usage_total = Usage::default();
    let mut turn = 0u32;
    let mut tool_calls_total = 0u32;
    let tools = tool::implemented_tools();
    let tool_defs: Vec<crate::provider::ToolDef> = tools.iter().map(BuiltinTool::schema).collect();

    let mut assistant_text = String::new();
    // §12.3：确定性无进展检测（不调用额外模型）。
    let mut progress = crate::agent::scheduler::ProgressTracker::default();
    // §12.4：wall-clock watchdog（实时主动取消）。
    // P1-3：接近预算时向 TUI 发送 BudgetWarning（此前只写日志）。
    // §16：取消来源（用户 vs wall-time）——watchdog 到期前写入 WALL_TIME，
    // 否则 UI/session 会把系统超时显示成用户取消。
    let cancel_cause = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
        crate::agent::limits::CANCEL_CAUSE_USER,
    ));
    let warn_ui = ui.clone();

    let cause_for_watchdog = cancel_cause.clone();
    let (watchdog, _wall) = crate::agent::limits::spawn_watchdog(
        &config.limits,
        cancel.clone(),
        move || {
            // §16：硬限制到期前标记来源，run 结束时据此记录 WallTimeExceeded。
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
            let system_prompt = system_prompt_text(
                config,
                crate::util::lock_mutex(&current_plan, "current_plan").as_ref(),
            );
            let projected = crate::context::estimate_request(&system_prompt, &messages, &tool_defs);
            if crate::context::should_compact(projected, usable) && !compaction_failed {
                match compact_turn(provider, &mut messages, session, config, &cancel).await {
                    Ok(()) => {
                        // P1-4：compaction 成功后若仍无法容纳（窗口过小），
                        // 不再发起普通请求（必然 length error），明确结束并提示用户。
                        let system_prompt = system_prompt_text(
                            config,
                            crate::util::lock_mutex(&current_plan, "current_plan").as_ref(),
                        );
                        let after =
                            crate::context::estimate_request(&system_prompt, &messages, &tool_defs);
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
                    Err(error) => {
                        // §15.4 第 6 条：失败不循环；继续确定性 prune，仍无法容纳则明确停止。
                        compaction_failed = true;
                        tracing::warn!(error = %error, "compaction failed; not retrying in this band");
                        // 确定性 prune 兜底（§15.3：只影响投影）。
                        messages = crate::context::prune_messages(messages);
                        // P1-4：prune 后仍超窗口（如 user 消息本身巨大）→ 明确结束。
                        let system_prompt = system_prompt_text(
                            config,
                            crate::util::lock_mutex(&current_plan, "current_plan").as_ref(),
                        );
                        let after =
                            crate::context::estimate_request(&system_prompt, &messages, &tool_defs);
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
        // §4.3 第二阶段：text-only attempt 断联后自动续写（最多 1 次）。
        // `content`/`saw_any_semantic`/`saw_tool_calls` 跨 attempt 累积
        // （整个 model turn 的事实）；`stream_recoveries` 是已续写次数。
        let mut stream_recoveries: u32 = 0;
        let mut content = String::new();
        let mut saw_any_semantic = false;
        let mut saw_tool_calls = false;

        let response = 'attempt: loop {
            // 构建 request：续写 attempt 在 messages 尾部注入 recovery instruction
            //（harness control metadata：不进 session、不进对话投影）。
            let request_messages = if stream_recoveries == 0 {
                messages.clone()
            } else {
                let mut recovery = messages.clone();
                let instruction = STREAM_RECOVERY_INSTRUCTION.replace("{partial}", &content);
                recovery.push(ChatMessage::User(instruction));
                recovery
            };
            let request = ModelRequest {
                model: config.model.name.clone(),
                messages: build_context(
                    config,
                    &request_messages,
                    crate::util::lock_mutex(&current_plan, "current_plan").as_ref(),
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
                let system_prompt = system_prompt_text(
                    config,
                    crate::util::lock_mutex(&current_plan, "current_plan").as_ref(),
                );
                let projected =
                    crate::context::estimate_request(&system_prompt, &request_messages, &tool_defs);
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
            let response = loop {
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
                                if saw_any_semantic && !saw_tool_calls && stream_recoveries < MAX_STREAM_RECOVERIES
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
                                    .ok();
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
            };
        };
        // provider 返回的 usage 是本次请求的用量；跨轮累加（§2.2/§12.4）。
        usage_total.input_tokens += response.usage.input_tokens;
        usage_total.output_tokens += response.usage.output_tokens;
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
            let batch = execute_batch(
                response.tool_calls,
                &mut tool_calls_total,
                config,
                session,
                &mut messages,
                &mut progress,
                &scan_snapshots,
                &snapshot_store,
                &current_plan,
                &cancel,
                &ui,
                interactive,
            )
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

    watchdog.abort();
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

/// batch 执行结果（§12.4：预算超限时明确结束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchEnd {
    Continue,
    BudgetExceeded,
}

/// 执行一次 tool-call batch（§12.2）。
///
/// 1. 全部参数预检（invalid → `rejected`，不启动）；
/// 2. 按资源冲突构建 waves；
/// 3. 同 wave 无冲突 Pure/Read 并行（受 `max_parallel_tools` 限制）；
///    Write / WorkspaceUnknown 按源顺序；
/// 4. 结果无论完成先后都按原 call index 送回 provider（§12.2 第 6 条）。
#[allow(clippy::too_many_arguments)]
async fn execute_batch(
    calls: Vec<ToolCall>,
    tool_calls_total: &mut u32,
    config: &Config,
    session: &mut SessionLog,
    messages: &mut Vec<ChatMessage>,
    progress: &mut crate::agent::scheduler::ProgressTracker,
    scan_snapshots: &std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, crate::tool::search::ScanSnapshot>>,
    >,
    snapshot_store: &std::sync::Arc<std::sync::Mutex<crate::tool::edit::SnapshotStore>>,
    current_plan: &std::sync::Arc<std::sync::Mutex<Option<crate::tool::plan::Plan>>>,
    cancel: &CancellationToken,
    ui: &mpsc::Sender<RuntimeEvent>,
    interactive: bool,
) -> Result<BatchEnd, RunFailure> {
    use crate::agent::scheduler::{
        PreparedCall, action_key, build_waves, stable_observation, state_stamp_from_ctx,
    };
    use futures_util::future::join_all;
    use std::collections::HashMap;

    let max_parallel = config.limits.max_parallel_tools as usize;

    // 1. 预检全部参数（§12.2 第 1 条）。
    let mut prepared: Vec<PreparedCall> = Vec::with_capacity(calls.len());
    let mut rejected: HashMap<usize, StoredToolOutcome> = HashMap::new();
    for (index, call) in calls.iter().enumerate() {
        if *tool_calls_total >= config.limits.max_tool_calls {
            return Ok(BatchEnd::BudgetExceeded);
        }
        *tool_calls_total += 1;
        let Some(tool) = BuiltinTool::from_name(&call.name) else {
            // §30 第 9 条：rejected 的 observation 必须持久化——runtime messages
            // 有 Tool 消息而 session 无 ToolRequested/ToolCompleted 时，restart 后
            // observation 丢失（P0-3 不变量违反）。
            let outcome = unknown_tool_outcome(&call.name);
            session
                .append_event(&SessionEvent::ToolRequested { call: call.clone() })
                .and_then(|_| session.complete_tool(call.call_id, &outcome))
                .map_err(|e| RunFailure::Session(e.to_string()))?;
            rejected.insert(index, outcome);
            continue;
        };
        match tool.parse_args(&call.arguments) {
            Ok(args) => {
                let access = crate::agent::scheduler::tool_access(
                    tool,
                    &args,
                    &config.workspace_root,
                    config.allow_outside_workspace,
                );
                prepared.push(PreparedCall {
                    source_index: index,
                    tool,
                    args,
                    access,
                    action_key: action_key(tool, &call.arguments),
                });
            }
            Err(message) => {
                let outcome = ToolOutcome::failed(
                    tool.name(),
                    crate::tool::outcome::ModelPayload {
                        status: ToolStatus::Rejected,
                        program: None,
                        exit_code: None,
                        duration_ms: 0,
                        output: format!(
                            "status: rejected
tool: {}
error: invalid_arguments

{message}",
                            tool.name()
                        ),
                        effect: None,
                        artifact: None,
                    },
                )
                .into_stored();
                // §30 第 9 条：rejected 的 observation 必须持久化（同未知工具）。
                session
                    .append_event(&SessionEvent::ToolRequested { call: call.clone() })
                    .and_then(|_| session.complete_tool(call.call_id, &outcome))
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
                rejected.insert(index, outcome);
            }
        }
    }

    // 2-3. waves（§12.2 第 3-4 条）。
    let waves = build_waves(prepared, max_parallel);
    let mut results: HashMap<usize, StoredToolOutcome> = rejected;

    for wave in waves {
        // session 事件按原 index 顺序（§12.2 第 6 条）。
        for call in wave.iter().map(|p| &calls[p.source_index]) {
            session
                .append_event(&SessionEvent::ToolRequested { call: call.clone() })
                .and_then(|_| session.sync_data())
                .map_err(|e| RunFailure::Session(e.to_string()))?;
        }

        // write-ahead（§14.2）：wave 内写工具先持久化 ToolStarted。
        for call in &wave {
            let source = &calls[call.source_index];
            let plan = write_tool_plan(
                call.tool,
                source,
                &config.workspace_root,
                config.allow_outside_workspace,
            );
            let recovery = recovery_metadata(
                call.tool,
                source,
                plan.as_ref(),
                &config.workspace_root,
                config.allow_outside_workspace,
            );
            if tool::requires_write_ahead(call.tool) {
                session
                    .write_ahead_tool(source.call_id, recovery)
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
            } else {
                session
                    .append_event(&SessionEvent::ToolStarted {
                        call_id: source.call_id,
                        recovery: None,
                    })
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
            }
        }

        // 并行执行（§12.2 第 3 条：同 wave 无冲突 calls）。
        // 无进展判定在构造 future 前同步完成（futures 不借用 progress/session）。
        // 实时输出通道：工具执行中 bash 增量 → 本 task 转发 → ui_tx。
        // BUG-012：有界通道（§12/§13）——UI 消费慢时工具侧 try_send 丢弃新帧，
        // 不阻塞进程读循环、不无限堆积。
        let (output_tx, mut output_rx) =
            tokio::sync::mpsc::channel::<tool::ToolStreamEvent>(tool::TOOL_STREAM_CAPACITY);
        let ui_for_stream = ui.clone();
        let stream_forwarder = tokio::spawn(async move {
            while let Some(event) = output_rx.recv().await {
                let _ = ui_for_stream
                    .send(RuntimeEvent::ToolOutputDelta {
                        call_id: event.call_id,
                        stream: event.stream,
                        text: event.text,
                    })
                    .await;
            }
        });
        // 本 wave 内写工具的 backup 清理路径（index → backup 路径）。
        let mut backup_cleanup: std::collections::HashMap<usize, std::path::PathBuf> =
            std::collections::HashMap::new();
        let mut futures = Vec::with_capacity(wave.len());
        for call in &wave {
            let source_index = call.source_index;
            let tool = call.tool;
            let args = call.args.clone();
            let action_key = call.action_key.clone();
            let access = call.access.clone();
            let ctx = tool::ToolContext {
                workspace_root: config.workspace_root.clone(),
                allow_outside_workspace: config.allow_outside_workspace,
                cancel: cancel.clone(),
                artifacts_root: config.artifacts_root.clone(),
                session_id: session.session_id().to_string(),
                call_id: calls[source_index].call_id,
                output_tx: Some(output_tx.clone()),
                scan_snapshots: scan_snapshots.clone(),
                shell_path: config.shell_path.clone(),
                snapshot_store: snapshot_store.clone(),
                current_plan: current_plan.clone(),
                interactive,
            };
            // §12.3：StateStamp（access footprint revisions + workspace epoch）。
            let state_stamp = format!(
                "{}|{}",
                state_stamp_from_ctx(&ctx, &access),
                progress.workspace_epoch()
            );
            let blocked = progress.should_block(&action_key, &state_stamp);

            // 工具真正启动前通知 TUI；长命令不再等执行结束才出现反馈。
            if tool != BuiltinTool::UpdatePlan {
                let (target, command) = tool_target(&calls[source_index]);
                let _ = ui
                    .send(RuntimeEvent::ToolStarted {
                        call_id: calls[source_index].call_id,
                        name: calls[source_index].name.clone(),
                        target,
                        command,
                    })
                    .await;
            }

            let plan = write_tool_plan(
                tool,
                &calls[source_index],
                &config.workspace_root,
                config.allow_outside_workspace,
            );
            // §10.7 第 6 步：backup 保留到 ToolCompleted 持久化之后；
            // 记录清理路径（成功后删除），崩溃恢复窗口依赖 backup 存在。
            if let Some(backup) = plan.as_ref().and_then(|p| p.backup_path.as_ref()) {
                backup_cleanup.insert(source_index, backup.clone());
            }
            futures.push(async move {
                if blocked {
                    let outcome = repeated_outcome(tool.name(), &action_key);
                    (source_index, outcome.into_stored())
                } else {
                    let outcome = tool::execute(tool, args, &ctx, plan.as_ref()).await;
                    (source_index, outcome.into_stored())
                }
            });
        }
        let results_vec = join_all(futures).await;
        for (index, outcome) in results_vec {
            // §12.3：执行后观察（ActionKey + ObservationKey + StateStamp 相同才算重复）。
            let observation = stable_observation(
                &outcome.session_metadata.tool,
                outcome.status_name(),
                &outcome.model_payload.output,
            );
            let state_stamp = {
                let access = wave
                    .iter()
                    .find(|p| p.source_index == index)
                    .map(|p| p.access.clone())
                    .unwrap_or(crate::agent::scheduler::ToolAccess::Pure);
                let ctx = tool::ToolContext {
                    workspace_root: config.workspace_root.clone(),
                    allow_outside_workspace: config.allow_outside_workspace,
                    cancel: cancel.clone(),
                    artifacts_root: config.artifacts_root.clone(),
                    session_id: session.session_id().to_string(),
                    call_id: calls[index].call_id,
                    output_tx: None,
                    scan_snapshots: scan_snapshots.clone(),
                    shell_path: config.shell_path.clone(),
                    snapshot_store: snapshot_store.clone(),
                    current_plan: current_plan.clone(),
                    interactive,
                };
                format!(
                    "{}|{}",
                    state_stamp_from_ctx(&ctx, &access),
                    progress.workspace_epoch()
                )
            };
            // §12.3：observe 使用 ActionKey（与 should_block 比较的同一键）。
            let action_key = wave
                .iter()
                .find(|p| p.source_index == index)
                .map(|p| p.action_key.clone())
                .unwrap_or_else(|| calls[index].name.clone());
            progress.observe(&action_key, &observation, &state_stamp);
            // §13：update_plan 成功 → PlanReplaced durable event。
            if outcome.status == ToolStatus::Succeeded && calls[index].name == "update_plan" {
                let plan = crate::util::lock_mutex(current_plan, "current_plan").clone();
                if let Some(plan) = plan {
                    session
                        .append_event(&SessionEvent::PlanReplaced { plan: plan.clone() })
                        .and_then(|_| session.sync_data())
                        .map_err(|e| RunFailure::Session(e.to_string()))?;
                    let _ = ui.send(RuntimeEvent::PlanUpdated { plan }).await;
                }
            }
            // §12.3：edit/write 成功 → workspace epoch 增加（允许基于新状态重试）。
            if outcome.status == ToolStatus::Succeeded
                && matches!(outcome.session_metadata.tool.as_str(), "edit" | "write")
            {
                progress.bump_workspace_epoch();
            }
            session
                .complete_tool(calls[index].call_id, &outcome)
                .map_err(|e| RunFailure::Session(e.to_string()))?;
            // §27：每个 tool call 记录可回答“哪个 call、什么工具、什么状态、
            // 耗时多少、exit code 与 artifact 引用”的诊断行。
            tracing::debug!(
                call_id = %calls[index].call_id,
                tool = %calls[index].name,
                status = ?outcome.status,
                duration_ms = outcome.model_payload.duration_ms,
                exit_code = ?outcome.model_payload.exit_code,
                artifact = ?outcome.model_payload.artifact,
                "tool completed"
            );
            // §10.7 第 6 步：ToolCompleted 已持久化，崩溃恢复窗口关闭。
            // P0-11：只有成功才删除 backup；失败（如 commit_recovery_failed，
            // 无法证明恢复完成）必须保留恢复现场（§10.7 第 5 条“保留所有文件”）。
            if backup_cleanup_allowed(outcome.status)
                && let Some(backup) = backup_cleanup.remove(&index)
            {
                let _ = std::fs::remove_file(backup);
            }
            if calls[index].name != "update_plan" {
                let _ = ui
                    .send(RuntimeEvent::ToolCompleted {
                        call_id: calls[index].call_id,
                        name: calls[index].name.clone(),
                        status: outcome.status,
                        duration_ms: outcome.model_payload.duration_ms,
                        exit_code: outcome.model_payload.exit_code,
                        tail: outcome.model_payload.output.clone(),
                    })
                    .await;
            }
            results.insert(index, outcome);
        }
        stream_forwarder.abort();
    }

    // 4. 按原 call index 回填（§12.2 第 6 条）。
    for (index, call) in calls.iter().enumerate() {
        if let Some(outcome) = results.remove(&index) {
            messages.push(tool_result_message(call, &outcome));
        }
    }
    Ok(BatchEnd::Continue)
}

/// 工具调用的可读展示摘要（TUI 工具卡片，§16.2）。
///
/// bash → `bash: <command>`；其余显示工具名。有界到 200 字符，避免整段脚本刷屏。
/// 工具调用的主行 target 与完整命令（整改 A2/A3：主行单行、命令进 overlay）。
///
/// - bash：target = 压缩后的命令（换行折空格、连续空白压缩、200 字符截断）；
///   command = 原文（≤8KiB，overlay 展示）。
/// - 其他文件工具：target = `name path`；其余只显示工具名。
fn tool_target(call: &ToolCall) -> (String, Option<String>) {
    fn truncate(text: &str, max_chars: usize) -> String {
        if text.chars().count() <= max_chars {
            text.to_string()
        } else {
            let mut t: String = text.chars().take(max_chars).collect();
            t.push('…');
            t
        }
    }
    let parsed = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
    match call.name.as_str() {
        "bash" => {
            if let Some(cmd) = parsed
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|c| c.as_str())
            {
                let compressed = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
                (truncate(&compressed, 200), Some(truncate(cmd, 8 * 1024)))
            } else {
                ("bash".into(), None)
            }
        }
        name if matches!(name, "read" | "write" | "edit" | "list" | "search") => {
            let target = parsed
                .as_ref()
                .and_then(|v| v.get("path"))
                .and_then(|p| p.as_str())
                .map(|path| format!("{name} {path}"))
                .unwrap_or_else(|| name.to_string());
            (truncate(&target, 120), None)
        }
        name => (name.to_string(), None),
    }
}

/// §12.3：无进展重复的拒绝结果。
fn repeated_outcome(tool: &str, action_key: &str) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        crate::tool::outcome::ModelPayload {
            status: ToolStatus::Rejected,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!(
                "status: rejected
tool: {tool}
error: repeated_without_progress
action: {action_key}
note: 相同动作已连续出现且无进展；请先读取相关文件或改变方法。
suggestions:
  - 使用 list 或 search 定位/确认文件与目录
  - 检查路径是否拼写错误
  - 如果是测试失败，读取完整输出（@artifact 引用）
  - 改用不同的实现路径或先制定/更新计划"
            ),
            effect: None,
            artifact: None,
        },
    )
}

/// compaction 一轮（§15.4）。
async fn compact_turn<P: Provider>(
    provider: &mut P,
    messages: &mut Vec<ChatMessage>,
    session: &mut SessionLog,
    config: &Config,
    cancel: &CancellationToken,
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
    let events = crate::session::read_events_with_seq(session.path())
        .map_err(|e| RunFailure::Session(e.to_string()))?;
    let projected = crate::session::project_messages_with_ranges(&events);
    let recent_start_seq = if projected.len() == pruned.len() {
        projected[split..]
            .iter()
            .find(|(_, start, _)| *start > 0)
            .map(|(_, start, _)| *start)
            .unwrap_or(seq + 1)
    } else {
        seq + 1
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
    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                if let ProviderEvent::TextDelta(text) = event {
                    summary_text.push_str(&text);
                }
            }
            result = &mut stream => {
                let _response = result.map_err(|e| RunFailure::Provider(e.to_string()))?;
                break;
            }
        }
    }
    // stream 返回后 channel 中可能还有已入队的尾部 delta，全部收完。
    while let Ok(event) = event_rx.try_recv() {
        if let ProviderEvent::TextDelta(text) = event {
            summary_text.push_str(&text);
        }
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

/// P0-11：backup 清理策略——只有工具成功提交后才允许删除 backup。
/// 失败（含 commit_recovery_failed：无法证明恢复完成）时 backup 是唯一
/// 恢复现场，必须保留（§10.7 第 5 条“保留所有文件”）。
fn backup_cleanup_allowed(status: ToolStatus) -> bool {
    status == ToolStatus::Succeeded
}

/// 拼接 system prompt 文本（P0-9 提取：预算估算与请求构造共用同一来源）。
fn system_prompt_text(config: &Config, plan: Option<&crate::tool::plan::Plan>) -> String {
    let mut system = DEFAULT_SYSTEM_PROMPT.to_string();
    if let Some(extra) = &config.system_prompt_extra {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    let snapshot = crate::tool::plan::plan_snapshot(plan);
    if !snapshot.is_empty() {
        system.push_str("\n\n");
        system.push_str(&snapshot);
    }
    system
}

/// 构造上下文 projection（§15.1 顺序：system → 用户目标 → 历史 turns → 当前输入在尾部）。
///
/// §13：每次 model request 的 runtime snapshot 都包含规范化计划（compaction 或
/// 长对话不会让模型只靠记忆遵循 Todo）。
fn build_context(
    config: &Config,
    messages: &[ChatMessage],
    plan: Option<&crate::tool::plan::Plan>,
) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len() + 2);
    out.push(ChatMessage::System(system_prompt_text(config, plan)));
    out.extend_from_slice(messages);
    out
}

/// 工具结果回填消息（§8.3：模型可见 envelope 是唯一回填内容）。
fn tool_result_message(call: &ToolCall, outcome: &StoredToolOutcome) -> ChatMessage {
    ChatMessage::Tool {
        tool_call_id: call.provider_id.clone(),
        name: call.name.clone(),
        content: outcome.model_payload.output.clone(),
    }
}

/// 未知工具：Rejected（不进入 chat 流，防止幻觉工具产生副作用）。
fn unknown_tool_outcome(name: &str) -> StoredToolOutcome {
    ToolOutcome::failed(
        name,
        crate::tool::outcome::ModelPayload {
            status: ToolStatus::Rejected,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!("status: rejected\ntool: {name}\nerror: unknown_tool"),
            effect: None,
            artifact: None,
        },
    )
    .into_stored()
}

/// 写工具的提交计划（§10.7 第 1 条：副作用前生成 temp/backup 路径）。
fn write_tool_plan(
    tool: BuiltinTool,
    call: &ToolCall,
    workspace_root: &Utf8PathBuf,
    allow_outside_workspace: bool,
) -> Option<tool::edit::CommitPlan> {
    match tool {
        BuiltinTool::Edit | BuiltinTool::Write => {
            let target = match tool {
                BuiltinTool::Edit => {
                    let parsed =
                        serde_json::from_str::<tool::edit::EditArgs>(&call.arguments).ok()?;
                    parsed.path
                }
                _ => {
                    let parsed =
                        serde_json::from_str::<tool::files::WriteArgs>(&call.arguments).ok()?;
                    parsed.path
                }
            };
            let target_path =
                crate::tool::resolve_write_path(workspace_root, &target, allow_outside_workspace)
                    .ok()?;
            Some(tool::edit::prepare_commit(&target_path))
        }
        _ => None,
    }
}

/// 写工具的 recovery metadata（§10.7 第 1 条：副作用前持久化；temp/backup 来自 plan）。
fn recovery_metadata(
    tool: BuiltinTool,
    call: &ToolCall,
    plan: Option<&tool::edit::CommitPlan>,
    workspace_root: &Utf8PathBuf,
    allow_outside_workspace: bool,
) -> Option<RecoveryMetadata> {
    let plan_paths = |plan: &tool::edit::CommitPlan| {
        (
            plan.temp_path.to_string_lossy().to_string(),
            plan.backup_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
        )
    };
    match (tool, &call.arguments) {
        (BuiltinTool::Edit, arguments) => {
            let parsed = serde_json::from_str::<tool::edit::EditArgs>(arguments).ok()?;
            let (temp, backup) = plan_paths(plan?);
            // §9.1：内部记录使用绝对路径（session 是内部事实源；恢复器据此定位文件）。
            let target = crate::tool::resolve_write_path(
                workspace_root,
                &parsed.path,
                allow_outside_workspace,
            )
            .ok()?;
            Some(RecoveryMetadata {
                tool: "edit".into(),
                target_path: target.to_string(),
                expected_revision: parsed.revision,
                temp_path: temp,
                backup_path: backup,
            })
        }
        (BuiltinTool::Write, arguments) => {
            let parsed = serde_json::from_str::<tool::files::WriteArgs>(arguments).ok()?;
            let (temp, backup) = plan_paths(plan?);
            let target = crate::tool::resolve_write_path(
                workspace_root,
                &parsed.path,
                allow_outside_workspace,
            )
            .ok()?;
            Some(RecoveryMetadata {
                tool: "write".into(),
                target_path: target.to_string(),
                expected_revision: String::new(),
                temp_path: temp,
                backup_path: backup,
            })
        }
        (BuiltinTool::Bash, arguments) => {
            let parsed = serde_json::from_str::<tool::command::BashArgs>(arguments).ok()?;
            Some(RecoveryMetadata {
                tool: "bash".into(),
                target_path: parsed.command,
                expected_revision: String::new(),
                temp_path: String::new(),
                backup_path: None,
            })
        }
        _ => None,
    }
}

impl StoredToolOutcome {
    pub fn status_name(&self) -> &'static str {
        match self.status {
            ToolStatus::Succeeded => "succeeded",
            ToolStatus::Failed => "failed",
            ToolStatus::TimedOut => "timed_out",
            ToolStatus::Cancelled => "cancelled",
            ToolStatus::Interrupted => "interrupted",
            ToolStatus::Rejected => "rejected",
        }
    }
}

impl BuiltinTool {
    pub fn from_name(name: &str) -> Option<BuiltinTool> {
        match name {
            "read" => Some(BuiltinTool::Read),
            "list" => Some(BuiltinTool::List),
            "search" => Some(BuiltinTool::Search),
            "edit" => Some(BuiltinTool::Edit),
            "write" => Some(BuiltinTool::Write),
            "bash" => Some(BuiltinTool::Bash),
            "update_plan" => Some(BuiltinTool::UpdatePlan),
            "web_search" => Some(BuiltinTool::WebSearch),
            "web_fetch" => Some(BuiltinTool::WebFetch),
            _ => None,
        }
    }
}

/// 恢复一个中断 session 后继续：把 Interrupted outcome 注入历史（§4.3）。
pub fn interrupted_as_messages(interrupted: &[(String, StoredToolOutcome)]) -> Vec<ChatMessage> {
    interrupted
        .iter()
        .map(|(provider_id, outcome)| ChatMessage::Tool {
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
    use super::*;

    /// P2：no-progress 拒绝输出必须带 suggestions（可恢复引导）。
    #[test]
    fn repeated_outcome_includes_suggestions() {
        let outcome = repeated_outcome("read", "read src/main.rs");
        let text = outcome.model_payload.output;
        assert!(text.contains("repeated_without_progress"), "{text}");
        assert!(text.contains("suggestions:"), "必须带下一步建议: {text}");
        assert!(text.contains("list 或 search"), "{text}");
    }

    /// P0-11：backup 只允许在工具成功时删除；失败（含 commit_recovery_failed，
    /// 无法证明恢复完成）必须保留恢复现场。
    #[test]
    fn backup_cleanup_policy_keeps_recovery_on_failure() {
        use crate::tool::outcome::ToolStatus;
        assert!(backup_cleanup_allowed(ToolStatus::Succeeded));
        assert!(!backup_cleanup_allowed(ToolStatus::Failed));
        assert!(!backup_cleanup_allowed(ToolStatus::Rejected));
        assert!(!backup_cleanup_allowed(ToolStatus::Cancelled));
    }
}
