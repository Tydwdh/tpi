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
    AssistantDelta {
        request_id: RequestId,
        kind: DeltaKind,
        text: String,
    },
    ToolProgress {
        call_id: ToolCallId,
        chunk: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// 执行一次完整 run（§6.2）。
///
/// `history` 是之前已提交的对话；`user_message` 是本轮用户输入。
/// 所有 durable 事件经 `session` append；写工具在副作用前执行 write-ahead（§14.2）。
pub async fn run<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    history: &[ChatMessage],
    user_message: String,
    ui: mpsc::Sender<RuntimeEvent>,
    cancel: CancellationToken,
) -> Result<AgentOutcome, RunFailure> {
    let run_id = session.run_id();
    let request_id = RequestId::new_v7();
    let span = tracing::info_span!("agent.run", %run_id, %request_id);
    let _enter = span.enter();

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
    let (watchdog, _wall) =
        crate::agent::limits::spawn_watchdog(&config.limits, cancel.clone(), || {
            // 接近预算：状态栏提示由 UI 消费（此处仅记录）。
            tracing::info!("run approaching wall-time budget");
        });
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

        // §15.4：compaction 检查（只在下一次请求前；完整 boundary 之后）。
        if let Some(context_window) = config.model.context_window {
            let usable = crate::context::usable_input(
                context_window,
                config.model.max_output_tokens.unwrap_or(0) as u64,
                config.safety_reserve_tokens,
            );
            let projected = crate::context::estimate_messages(&messages);
            if crate::context::should_compact(projected, usable) && !compaction_failed {
                match compact_turn(provider, &mut messages, session, config, &cancel).await {
                    Ok(()) => {
                        // §15.4 第 6 条：compaction 成功后若仍无法容纳（窗口过小），
                        // 明确停止尝试，避免无限循环；继续执行并提示用户。
                        let after = crate::context::estimate_messages(&messages);
                        if after > usable {
                            compaction_failed = true;
                            tracing::warn!(
                                projected = after,
                                usable,
                                "context window too small even after compaction; stopping compaction attempts"
                            );
                        }
                    }
                    Err(error) => {
                        // §15.4 第 6 条：失败不循环；继续确定性 prune，仍无法容纳则明确停止。
                        compaction_failed = true;
                        tracing::warn!(error = %error, "compaction failed; not retrying in this band");
                        // 确定性 prune 兜底（§15.3：只影响投影）。
                        messages = crate::context::prune_messages(messages);
                    }
                }
            }
        }

        // 3. 构建 context projection 并发起一次请求（§6.2 第 3-4 步）。
        let request = ModelRequest {
            model: config.model.name.clone(),
            messages: build_context(config, &messages, current_plan.lock().unwrap().as_ref()),
            tools: tool_defs.clone(),
            max_output_tokens: config.model.max_output_tokens,
            reasoning: config.model.reasoning.clone(),
            context_window: config.model.context_window,
        };
        let (event_tx, mut event_rx) = mpsc::channel(crate::provider::EVENT_CHANNEL_CAPACITY);

        let response = provider
            .stream(request, event_tx, cancel.clone())
            .await
            .map_err(|e| {
                session
                    .append_event(&SessionEvent::RunCompleted {
                        reason: CompletionReason::Error,
                        usage: usage_total,
                    })
                    .ok();
                RunFailure::Provider(e.to_string())
            })?;

        // 消费剩余流事件并汇总文本。
        let mut content = String::new();
        while let Some(event) = event_rx.recv().await {
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
                | ProviderEvent::ToolArgumentsDelta { .. } => {}
                ProviderEvent::Usage(_) => {}
            }
        }
        usage_total = response.usage;

        // 5. 原子提交 assistant message（durable boundary，§14.2）。
        if !content.is_empty() {
            assistant_text = content.clone();
            session
                .append_event(&SessionEvent::AssistantMessageCommitted {
                    message: AssistantMessage { content },
                })
                .and_then(|_| session.sync_data())
                .map_err(|e| RunFailure::Session(e.to_string()))?;
        }

        // 6. 工具调用（§12.2 batch 调度）。
        if !response.tool_calls.is_empty() {
            messages.push(ChatMessage::Assistant {
                content: String::new(),
                tool_calls: response.tool_calls.clone(),
            });
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
            )
            .await?;
            if batch == BatchEnd::BudgetExceeded {
                session
                    .append_event(&SessionEvent::RunCompleted {
                        reason: CompletionReason::Error,
                        usage: usage_total,
                    })
                    .and_then(|_| session.sync_data())
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
                break 'run_loop CompletionReason::Error;
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
    Ok(AgentOutcome {
        reason: final_reason,
        usage: usage_total,
        messages,
        assistant_text,
    })
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
            rejected.insert(index, unknown_tool_outcome(&call.name));
            continue;
        };
        match tool.parse_args(&call.arguments) {
            Ok(args) => {
                let access = crate::agent::scheduler::tool_access(tool, &args);
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
            let plan = write_tool_plan(call.tool, source, &config.workspace_root);
            let recovery =
                recovery_metadata(call.tool, source, plan.as_ref(), &config.workspace_root);
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
        let mut futures = Vec::with_capacity(wave.len());
        for call in &wave {
            let source_index = call.source_index;
            let tool = call.tool;
            let args = call.args.clone();
            let action_key = call.action_key.clone();
            let access = call.access.clone();
            let ctx = tool::ToolContext {
                workspace_root: config.workspace_root.clone(),
                cancel: cancel.clone(),
                artifacts_root: config.artifacts_root.clone(),
                session_id: session.session_id().to_string(),
                scan_snapshots: scan_snapshots.clone(),
                shell_path: config.shell_path.clone(),
                snapshot_store: snapshot_store.clone(),
                current_plan: current_plan.clone(),
                web_brave_key_env: config.web_brave_key_env.clone(),
            };
            // §12.3：StateStamp（access footprint revisions + workspace epoch）。
            let state_stamp = format!(
                "{}|{}",
                state_stamp_from_ctx(&ctx, &access),
                progress.workspace_epoch()
            );
            let blocked = progress.should_block(&action_key, &state_stamp);

            let plan = write_tool_plan(tool, &calls[source_index], &config.workspace_root);
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
                    cancel: cancel.clone(),
                    artifacts_root: config.artifacts_root.clone(),
                    session_id: session.session_id().to_string(),
                    scan_snapshots: scan_snapshots.clone(),
                    shell_path: config.shell_path.clone(),
                    snapshot_store: snapshot_store.clone(),
                    current_plan: current_plan.clone(),
                    web_brave_key_env: config.web_brave_key_env.clone(),
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
            if outcome.status == ToolStatus::Succeeded
                && calls[index].name == "update_plan"
                && let Some(plan) = current_plan.lock().unwrap().clone()
            {
                session
                    .append_event(&SessionEvent::PlanReplaced { plan })
                    .and_then(|_| session.sync_data())
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
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
            let _ = ui
                .send(RuntimeEvent::ToolProgress {
                    call_id: calls[index].call_id,
                    chunk: format!("← {} {}", calls[index].name, outcome.status_name()),
                })
                .await;
            results.insert(index, outcome);
        }
    }

    // 4. 按原 call index 回填（§12.2 第 6 条）。
    for (index, call) in calls.iter().enumerate() {
        if let Some(outcome) = results.remove(&index) {
            messages.push(tool_result_message(call, &outcome));
        }
    }
    Ok(BatchEnd::Continue)
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
note: 相同动作已连续出现且无进展；请先读取相关文件或改变方法。"
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
    let pruned = crate::context::prune_messages(messages.clone());
    // 2. 保留最近约 25% 的完整原始上下文，且不小于两个完整 turns（§15.4 第 3 条）。
    let keep_count = (pruned.len() as f64 * crate::context::KEEP_RECENT_RATIO)
        .ceil()
        .max(crate::context::MIN_KEEP_TURNS as f64) as usize;
    let split = pruned.len().saturating_sub(keep_count);
    let history = pruned[..split].to_vec();
    let recent = pruned[split..].to_vec();

    // 3. 用 compaction 角色（默认 primary，§7.2）生成结构化 summary。
    let (event_tx, mut event_rx) = mpsc::channel(crate::provider::EVENT_CHANNEL_CAPACITY);
    let request = ModelRequest {
        model: config.model.name.clone(),
        messages: crate::context::compaction_request_messages(&history),
        tools: Vec::new(), // §15.4：compaction request 不提供任何工具 schema。
        max_output_tokens: Some(1024),
        reasoning: config.model.reasoning.clone(),
        context_window: config.model.context_window,
    };
    provider
        .stream(request, event_tx, cancel.clone())
        .await
        .map_err(|e| RunFailure::Provider(e.to_string()))?;
    let mut summary_text = String::new();
    while let Some(event) = event_rx.recv().await {
        if let ProviderEvent::TextDelta(text) = event {
            summary_text.push_str(&text);
        }
    }
    let summary_text = crate::context::parse_summary(&summary_text);
    let summary_tokens = crate::context::estimate_tokens(&summary_text);
    let original_tokens = crate::context::estimate_messages(&history);
    eprintln!(
        "COMPACT original={original_tokens} summary={summary_tokens} text_len={} messages={}",
        summary_text.len(),
        messages.len()
    );

    // 4. 校验必填字段和压缩后估算；只有明显缩小才提交（§15.4 第 5 条）。
    if summary_text.is_empty()
        || !crate::context::is_significant_shrink(original_tokens, summary_tokens)
    {
        return Err(RunFailure::ToolInfrastructure(
            "compaction summary invalid or not significant".into(),
        ));
    }
    let seq = session.seq();
    session
        .append_event(&SessionEvent::CompactionCommitted {
            covered: crate::session::EventRange {
                start: EventId::from_u128(1),
                end: EventId::from_u128(seq as u128),
            },
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
    out.push(ChatMessage::System(system));
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
            let target_path = crate::tool::resolve_workspace_path(workspace_root, &target);
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
            let target = crate::tool::resolve_workspace_path(workspace_root, &parsed.path);
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
            let target = crate::tool::resolve_workspace_path(workspace_root, &parsed.path);
            Some(RecoveryMetadata {
                tool: "write".into(),
                target_path: target.to_string(),
                expected_revision: String::new(),
                temp_path: temp,
                backup_path: backup,
            })
        }
        (BuiltinTool::Run, arguments) => {
            let parsed = serde_json::from_str::<tool::command::RunArgs>(arguments).ok()?;
            Some(RecoveryMetadata {
                tool: "run".into(),
                target_path: parsed.program,
                expected_revision: String::new(),
                temp_path: String::new(),
                backup_path: None,
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
            "run" => Some(BuiltinTool::Run),
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
