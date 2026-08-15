//! Agent 状态机与模型—工具执行循环。
//!
//! §6.2 一轮的精确算法：接收用户消息 → append UserSubmitted → 构建 context →
//! 发起一次 provider request → 消费规范化 stream → 原子提交 assistant message →
//! 若有 tool calls：校验、调度执行、按原 index 回填 tool messages → 再次请求；
//! 若无 tool call 且 finish=stop，立即完成 run，绝不追加第二次模型请求（§3.2 不变量 2）。

use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::config::Config;

pub mod answer;
pub mod limits;
/// 调度原语（§12：资源声明 / waves / 无进展检测）物理上属于 tool 领域
/// （src/tool/scheduler.rs）。此处 re-export 仅兼容既有 `agent::scheduler`
/// 引用（测试契约）；新代码请直接用 `crate::tool::scheduler`，
/// 测试引用迁移完成后删除本行（AGENTS.md §27 清理）。
pub use crate::tool::scheduler;
mod tool_runtime;
use self::tool_runtime::{BatchEnd, ToolBatchExecutor, ToolRuntime};
use crate::ids::{EventId, RequestId, ToolCallId};
use crate::provider::{ChatMessage, ModelRequest, Provider, ProviderEvent, ToolCall};
use crate::session::{
    self, AssistantMessage, CompletionReason, ModelRef, RunLimits, SessionEvent, Usage,
};
use crate::tool::outcome::{StoredToolOutcome, ToolStatus};

/// 内建 system prompt（§23 草案）。
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

/// §4.3 第二阶段：text-only attempt 断联后的最大自动续写次数。
/// §用户诉求：与第 1 层传输层重试一致提升到 10 次（每次续写注入 recovery
/// instruction，从断点继续且不复述）；已出现 tool delta 的 attempt 不自动续。
pub const MAX_STREAM_RECOVERIES: u32 = 10;

/// §4.3 第三阶段：partial tool-call 后整个 model turn 重新生成的最大次数。
/// §用户诉求：同样提升到 10 次；每次重新发起原始请求（不续接 partial JSON，
/// 避免解析风险），全部失败后如实上报 ProviderInterrupted。
pub const MAX_TURN_RESTARTS: u32 = 10;

/// §用户诉求（软着陆）：达到 max_model_turns 前的最后一轮注入收尾指令——
/// 让模型总结已完成工作与剩余建议（OpenCode 式），而不是硬断。
/// harness control metadata：不进 durable conversation，不进 session。
const FINAL_TURN_INSTRUCTION: &str = "\
这是本次运行的最后一个回合。请不要再调用新的工具，
总结你已完成的工作，并给出剩余待办与建议，然后结束。";

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

/// 用户对一个已经提交了 partial assistant 的 Error/Length turn 执行 `/retry`
/// 时，不能用空输入让模型猜语义；明确要求续写且不复述。该控制指令只用于
/// retry 的首个 model request，不写入 durable conversation。
const MANUAL_CONTINUE_INSTRUCTION: &str = "\
The previous assistant response did not complete successfully.
Continue it from the exact point where it stopped.
Do not restart, summarize, or repeat text already present in the conversation.";

const MIN_RECOVERY_OVERLAP_BYTES: usize = 8;

/// 单轮 run 的结果。
pub struct AgentOutcome {
    pub reason: CompletionReason,
    pub usage: Usage,
    /// 本轮新增的对话消息（供调用方保存/继续）。
    pub messages: Vec<ChatMessage>,
    /// 最终 assistant 文本（UI 展示）。
    pub assistant_text: String,
    /// §13（AGENTS.md）：run 因 `request_input` 挂起时的完整信息
    /// （TUI 显示问题并等待用户回答；非挂起为 None）。
    pub awaiting_input: Option<AwaitingInput>,
}

/// §13：`request_input` 挂起的结构化信息。
///
/// `text` 是参数渲染后的多行文本（编号 + header + 选项），session 的
/// `UserInputRequested.prompt` 与 TUI 展示共用；`questions` 是结构化问题
/// 列表（TUI 内联展示在 transcript，单问题带选项时支持数字编号回答）。
pub struct AwaitingInput {
    pub text: String,
    pub questions: Vec<crate::tool::request_input::RequestInputQuestion>,
}

/// Inputs that describe one agent run independently of its provider, session,
/// and immutable configuration dependencies.
pub struct RunInput<'a> {
    pub history: &'a [ChatMessage],
    pub user_message: String,
    pub ui: mpsc::Sender<LiveEvent>,
    pub cancel: CancellationToken,
    pub interactive: bool,
    /// P1-10：手动 `/compact`——无条件在第一个完整边界执行一次压缩。
    pub force_compaction: bool,
    /// ActiveWorkspace（§26-§30）：None = Local（默认）；Some = 远端/自定义
    /// workspace（R4：agent 测试注入 remote workspace，bash/file 按此分发）。
    pub workspace: Option<crate::workspace::ActiveWorkspace>,
    /// P4-02/P4 gate：工具注册表由 composition root 注入（builtin + MCP 同一
    /// registry；禁止 global registry）。
    pub registry: std::sync::Arc<std::sync::Mutex<crate::tool::registry::ToolRegistry>>,
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

/// compaction 失败的细分原因（§用户诉求：手动 /compact 失败时 UI 区分提示）。
#[derive(Debug)]
enum CompactionFailure {
    /// 模型未返回有效摘要（空输出或极短无内容；raw 为原始输出供 UI 诊断）。
    SummaryInvalid { raw: String },
    /// 摘要解析成功但缩小比例不显著（is_significant_shrink 不满足）。
    NotSignificant,
    /// provider 层失败（连接/响应异常）。
    Provider(String),
    /// session 持久化失败。
    Session(String),
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

/// UI-agnostic 运行时事件（P1-03：agent 只发语义事实，**不含任何展示字段**）。
///
/// - headless / RPC / 测试可直接消费，无需理解 TUI 表示（target/diff/tail）；
/// - TUI 的展示投影（`RuntimeEvent`）由 app projector 生成（`app::project_live_event`）。
///
/// 工具事件携带**原始参数**（`arguments`）而非渲染后的 target/command——
/// target/command 是 view 概念，由 projector 按需生成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// 每次实际 provider 请求前发送（= 目标词汇的 Step 边界，P1-01）。
    StepStarted { step: u32 },
    AssistantDelta {
        request_id: RequestId,
        kind: DeltaKind,
        text: String,
    },
    /// 工具真正启动前发送（语义事实：call + name + 原始参数）。
    ToolStarted {
        call_id: ToolCallId,
        name: String,
        /// 模型发出的原始参数 JSON（projector 用它生成 target/command 摘要）。
        arguments: String,
    },
    /// 工具终态（语义事实 + 有界输出/diff；`tail` 展示裁剪由 projector 生成）。
    ToolCompleted {
        call_id: ToolCallId,
        name: String,
        status: ToolStatus,
        duration_ms: u64,
        exit_code: Option<i32>,
        /// 有界输出摘要（模型/用户可见内容）。
        output: String,
        /// edit/write 的结构化 diff（无独立数据源可重建，随语义终态携带）。
        diff: Option<String>,
    },
    /// 工具执行中的实时输出增量（bash stdout/stderr）。
    ToolOutputDelta {
        call_id: ToolCallId,
        stream: u8,
        text: String,
    },
    /// 上下文占用投影（每次请求前）。
    ContextUsage { projected: u64, usable: u64 },
    /// 每次 provider 请求返回 usage 后（缓存命中实时展示）。
    UsageUpdated {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
    },
    /// 接近 wall-time 预算。
    BudgetWarning,
    /// `update_plan` 提交后的独立状态。
    PlanUpdated { plan: crate::tool::plan::Plan },
    /// 流中断后正在自动续写（text-only attempt 恢复）。
    StreamRecovering { attempt: u32 },
    /// partial tool-call 后整个 model step 重新生成。
    TurnRestarting { attempt: u32 },
    /// 手动 /compact 的结果反馈。
    CompactionNotice { message: String },
}

/// TUI view event（P1-03：由 app projector 从 [`LiveEvent`] 生成，agent 不再直接发）。
/// 含展示字段（target/diff/tail），只被 TUI 消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// 每次实际 provider 请求前发送，供 TUI 更新运行状态。
    StepStarted { step: u32 },
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
    /// 每次 provider 请求返回 usage 后发送（§用户诉求：缓存命中实时展示，
    /// 类似 Claude Code——footer 显示本次请求的缓存命中率，不等 run 结束）。
    UsageUpdated {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
    },
    /// 接近 wall-time 预算（P1-3：TUI 状态栏/系统行提示，此前只写日志）。
    BudgetWarning,
    /// `update_plan` 提交后的独立 UI 状态；不作为聊天流水的一部分。
    PlanUpdated { plan: crate::tool::plan::Plan },
    /// 流中断后正在自动续写（第二阶段 §4.3：text-only attempt 恢复）。
    StreamRecovering { attempt: u32 },
    /// partial tool-call 后整个 model turn 重新生成（第三阶段 §4.3）。
    /// UI 应丢弃当前 attempt 的 partial 展示（不进 transcript）。
    TurnRestarting { attempt: u32 },
    /// 手动 /compact 的结果反馈（§用户诉求：压缩未生效时用户可见，
    /// 此前只写日志、界面无感知）。
    CompactionNotice { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// 执行一次完整 run（§6.2）。
pub async fn run<P: Provider, S: crate::session::store::SessionStore>(
    provider: &mut P,
    session: &mut S,
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
        workspace,
        registry,
    } = input;
    let run_id = session.begin_run();
    // O1（P1-07）：一次 public Agent Run = 一个 TraceId；span 用 SpanId。
    // 只在真实边界注入（这里是 agent 的入口边界），后续 follow-up 新 run
    // 生成新 TraceId，跨 run 因果用显式 link（O8/P8 子代理时落地）。
    let trace_id = crate::ids::TraceId::new_v7();
    let span_id = crate::ids::SpanId::new_v7();
    let span = tracing::info_span!("agent.run", %run_id, %trace_id, %span_id);
    // O0（Medium-7）：async 函数体不能用同步 enter guard 跨 await 持有——
    // thread-local scope 在 future yield 后仍持锁，同线程其他任务会被错误
    // 归入当前 span，造成并发 run 的 parent/child 关系交叉（tests/
    // trace_ancestry.rs 复现：enter 深度 = 2）。用 Future::instrument 使
    // span 随 future 的 poll enter/exit（每次 poll 配对，yield 即释放）。
    run_inner(
        provider,
        session,
        config,
        run_id,
        RunInput {
            history,
            user_message,
            ui,
            cancel,
            interactive,
            force_compaction,
            workspace,
            registry,
        },
    )
    .instrument(span)
    .await
}

/// `run` 的实际执行体（由 `run` 以 `.instrument(span)` 包裹，见 O0）。
async fn run_inner<P: Provider, S: crate::session::store::SessionStore>(
    provider: &mut P,
    session: &mut S,
    config: &Config,
    run_id: crate::ids::RunId,
    input: RunInput<'_>,
) -> Result<AgentOutcome, RunFailure> {
    let RunInput {
        history,
        user_message,
        ui,
        cancel,
        interactive,
        force_compaction,
        workspace,
        registry,
    } = input;
    // P1-05：run_inner 只读窄视图 AgentConfig。
    let agent_cfg = config.agent_config();
    // §44：run 级耗时基线。
    let run_started = std::time::Instant::now();

    // 1. 用户提交（durable boundary）。
    // 空 user_message = retry 语义（`/retry`）：复用已有 history（上次失败的 run
    // 不重复记录 UserSubmitted，也不追加新 User 消息）；仍记录 RunStarted 作为新 run
    // （词汇审计 §3.2：retry 是 Run 级操作，不是 attempt）。app 层已拦截空输入，
    // 正常对话不会传空消息。
    if !user_message.is_empty() {
        session
            .commit(&SessionEvent::UserSubmitted {
                content: user_message.clone(),
            })
            .map_err(|e| RunFailure::Session(e.to_string()))?;
    }
    session
        .commit(&SessionEvent::RunStarted {
            model: ModelRef {
                name: agent_cfg.model.name.clone(),
                provider: agent_cfg.model.provider.clone(),
            },
            limits: RunLimits {
                max_turns: agent_cfg.limits.max_model_turns,
                max_tool_calls: agent_cfg.limits.max_tool_calls,
            },
        })
        .map_err(|e| RunFailure::Session(e.to_string()))?;

    // watchdog 必须覆盖手动 compaction 在内的整个模型工作阶段，并通过
    // Supervisor 保证所有 `?`/early return 路径都停止后台任务（P2-06：
    // 删除 AbortTaskOnDrop 直接 abort；正常路径 shutdown() join，Drop 兜底 cancel）。
    let cancel_cause = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
        crate::agent::limits::CANCEL_CAUSE_USER,
    ));
    // §用户诉求：max_wall_time_minutes=0（默认）不启动 watchdog——不限制。
    // P2-06：watchdog 逻辑由 Supervisor 直接承载（不再嵌套 spawn_watchdog 的
    // 内部 spawn；保留 limits::spawn_watchdog 供测试与诊断）。
    let mut watchdog_supervisor = crate::process::supervisor::Supervisor::new();
    if agent_cfg.limits.max_wall_time_minutes > 0 {
        let warn_ui = ui.clone();
        let cause_for_watchdog = cancel_cause.clone();
        let run_cancel = cancel.clone();
        let wall_secs = agent_cfg.limits.max_wall_time_minutes.saturating_mul(60);
        watchdog_supervisor.spawn("agent.watchdog", move |sup_cancel| async move {
            let wall = std::time::Duration::from_secs(wall_secs.max(1));
            let deadline = tokio::time::Instant::now() + wall;
            let warn_at =
                deadline - std::time::Duration::from_secs((wall.as_secs() as f64 * 0.1) as u64);
            // 接近预算提示（剩余 10%）。
            tokio::select! {
                _ = tokio::time::sleep_until(warn_at) => {
                    tracing::info!("run approaching wall-time budget");
                    let _ = warn_ui.try_send(LiveEvent::BudgetWarning);
                }
                _ = sup_cancel.cancelled() => return,
                _ = run_cancel.cancelled() => return,
            }
            // 硬限制：先写取消来源，再取消 run token（§16 区分用户/超时）。
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    cause_for_watchdog.store(
                        crate::agent::limits::CANCEL_CAUSE_WALL_TIME,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    run_cancel.cancel();
                }
                _ = sup_cancel.cancelled() => {}
                _ = run_cancel.cancelled() => {}
            }
        });
    }

    let mut usage_total = Usage::default();
    let mut messages: Vec<ChatMessage> = history.to_vec();
    let initial_plan = crate::session::latest_plan_from_events(
        &session
            .events_with_seq()
            .map_err(|e| RunFailure::Session(format!("read events: {e}")))?,
    )
    .map_err(|e| RunFailure::Session(format!("restore plan: {e}")))?;
    // 恢复态只补一次并留在本轮 runtime history 中；不能在每次 request 构造时
    // 都把计划重新追加到尾部，否则即使改成 Tool 角色也会持续抢占最新上下文。
    ensure_plan_state_messages(&mut messages, initial_plan.as_ref());
    let manual_retry_continuation = user_message.is_empty()
        && matches!(
            history.last(),
            Some(ChatMessage::Assistant { content, .. }) if !content.trim().is_empty()
        );
    if !user_message.is_empty() {
        messages.push(ChatMessage::User(user_message.clone()));
    }
    // P1-10：手动 /compact——在第一个完整边界无条件执行一次压缩。
    // 结果（成功/跳过）反馈到 UI（§用户诉求）：压缩成功、无收益、历史不足
    // 都明确提示，不再只写日志。
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
            Ok(()) => {
                ensure_plan_state_messages(&mut messages, initial_plan.as_ref());
                let _ = ui
                    .send(LiveEvent::CompactionNotice {
                        message: "手动压缩完成：旧历史已压缩为摘要，上下文已精简".into(),
                    })
                    .await;
            }
            // §用户诉求：细分失败原因——模型未按格式返回 / 无显著收益 /
            // provider 失败 / session 失败，分别提示，不再笼统一句。
            Err(CompactionFailure::SummaryInvalid { raw }) => {
                tracing::warn!("manual compaction: summary invalid");
                // §用户诉求：失败可见——带模型原文前缀，用户能判断是格式
                // 问题还是模型没干活。
                let preview: String = raw.chars().take(120).collect();
                let preview = if raw.chars().count() > 120 {
                    format!("{preview}…")
                } else {
                    preview
                };
                let _ = ui
                    .send(LiveEvent::CompactionNotice {
                        message: format!(
                            "手动压缩未生效：模型未返回有效摘要。模型输出：{preview:?}"
                        ),
                    })
                    .await;
            }
            Err(CompactionFailure::NotSignificant) => {
                tracing::warn!("manual compaction: not significant");
                let _ = ui
                    .send(LiveEvent::CompactionNotice {
                        message: "手动压缩未生效：摘要无显著收益（历史本身已较精简或摘要过长）"
                            .into(),
                    })
                    .await;
            }
            Err(CompactionFailure::Provider(error)) => {
                tracing::warn!(error = %error, "manual compaction: provider failed");
                let _ = ui
                    .send(LiveEvent::CompactionNotice {
                        message: format!("手动压缩未生效：压缩请求失败（{error}）"),
                    })
                    .await;
            }
            Err(CompactionFailure::Session(error)) => {
                return Err(RunFailure::Session(error));
            }
        }
    } else if force_compaction {
        let _ = ui
            .send(LiveEvent::CompactionNotice {
                message: "手动压缩未生效：没有可压缩的历史（当前对话过短）".into(),
            })
            .await;
    }
    // 工具共享状态与 ToolContext 构造由 run-scoped runtime 统一管理。
    // §R4：workspace 由调用方注入（测试可传 remote）；None = Local。
    let active_workspace = workspace.unwrap_or_else(|| {
        let local = crate::workspace::LocalWorkspace::new(
            agent_cfg.workspace_root.clone(),
            config.allow_outside_workspace,
        );
        crate::workspace::ActiveWorkspace::local(local)
    });
    let tool_runtime = ToolRuntime::new(
        config,
        session.session_id().to_string(),
        cancel.clone(),
        interactive,
        initial_plan,
        active_workspace,
        // P4-02/P4 gate：registry 由 composition root 注入（无全局）。
        registry,
    );

    let mut turn = 0u32;
    let mut tool_calls_total = 0u32;
    // §Phase 5：工具定义来自 ToolRegistry（builtin + MCP），经 ToolSelector
    // 按用户消息选择——MCP 大量工具不一次塞给 LLM（README2 §14）。
    // P4-03：Step 开始 reload 工具快照（defs + external lookup 共用）。
    let tool_defs: Vec<crate::provider::ToolDef> = tool_runtime.reload(&user_message);

    let mut assistant_text = String::new();
    // §13：`request_input` 挂起时模型的问题（随 AgentOutcome 返回给 app 层显示）。
    let mut awaiting_input: Option<AwaitingInput> = None;
    // §12.3：确定性无进展检测（不调用额外模型）。
    let mut progress = crate::tool::scheduler::ProgressTracker::default();
    // §15.4：同一阈值区间内 compaction 失败后不反复调用模型。
    let mut compaction_failed = false;
    let final_reason: CompletionReason = 'run_loop: loop {
        // §用户诉求：max_model_turns=0 = 不限制（默认）。
        if agent_cfg.limits.max_model_turns > 0 && turn >= agent_cfg.limits.max_model_turns {
            session
                .commit_terminal(&SessionEvent::RunCompleted {
                    reason: CompletionReason::MaxTurns,
                    usage: usage_total,
                })
                .map_err(|e| RunFailure::Session(e.to_string()))?;
            break CompletionReason::MaxTurns;
        }
        turn += 1;
        // 一个 request_id 标识一个逻辑 model turn；自动续写/restart 沿用它，
        // 下一轮（通常在工具结果之后）必须分配新 id。
        let request_id = RequestId::new_v7();
        let _ = ui.send(LiveEvent::StepStarted { step: turn }).await;

        // §15.4：compaction 检查（只在下一次请求前；完整 boundary 之后）。
        // P0-9：投影用请求级估算（system prompt + 计划工具轮 + 工具 schema），
        // 只算 messages 会低估实际请求，导致 compaction 触发过晚。
        if let Some(context_window) = agent_cfg.model.context_window {
            let usable = crate::context::usable_input(
                context_window,
                agent_cfg.model.max_output_tokens.unwrap_or(0) as u64,
                agent_cfg.safety_reserve_tokens,
            );
            let system_prompt = system_prompt_text(agent_cfg.system_prompt_extra.as_deref(), None);
            let projected = crate::context::estimate_request(&system_prompt, &messages, &tool_defs);
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
                        let plan = tool_runtime.plan_snapshot();
                        ensure_plan_state_messages(&mut messages, plan.as_ref());
                        // P1-4：compaction 成功后若仍无法容纳（窗口过小），
                        // 不再发起普通请求（必然 length error），明确结束并提示用户。
                        let system_prompt =
                            system_prompt_text(agent_cfg.system_prompt_extra.as_deref(), None);
                        let after =
                            crate::context::estimate_request(&system_prompt, &messages, &tool_defs);
                        if after > usable {
                            session
                                .commit_terminal(&SessionEvent::RunCompleted {
                                    reason: CompletionReason::ContextOverflow,
                                    usage: usage_total,
                                })
                                .map_err(|e| RunFailure::Session(e.to_string()))?;
                            break 'run_loop CompletionReason::ContextOverflow;
                        }
                    }
                    Err(CompactionFailure::Session(error)) => {
                        return Err(RunFailure::Session(error));
                    }
                    Err(error) => {
                        // §15.4 第 6 条：失败不循环；继续确定性 prune，仍无法容纳则明确停止。
                        compaction_failed = true;
                        tracing::warn!(error = ?error, "compaction failed; not retrying in this band");
                        // 确定性 prune 兜底（§15.3：只影响投影）。
                        messages = crate::context::prune_messages(messages);
                        let plan = tool_runtime.plan_snapshot();
                        ensure_plan_state_messages(&mut messages, plan.as_ref());
                        // P1-4：prune 后仍超窗口（如 user 消息本身巨大）→ 明确结束。
                        let system_prompt =
                            system_prompt_text(agent_cfg.system_prompt_extra.as_deref(), None);
                        let after =
                            crate::context::estimate_request(&system_prompt, &messages, &tool_defs);
                        if after > usable {
                            session
                                .commit_terminal(&SessionEvent::RunCompleted {
                                    reason: CompletionReason::ContextOverflow,
                                    usage: usage_total,
                                })
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
            let mut ephemeral_system = if stream_recoveries > 0 {
                Some(STREAM_RECOVERY_INSTRUCTION.replace("{partial}", &content))
            } else if manual_retry_continuation && turn == 1 {
                Some(MANUAL_CONTINUE_INSTRUCTION.to_string())
            } else {
                None
            };
            // §用户诉求（软着陆）：max_model_turns 已配且这是最后一轮时，
            // 在既有 ephemeral 指令上追加收尾提示（turn 在循环开头已 ++，
            // 第 max 轮检查通过后 ++ 到 max = 最后一轮）。
            if agent_cfg.limits.max_model_turns > 0 && turn == agent_cfg.limits.max_model_turns {
                let base = ephemeral_system.take().unwrap_or_default();
                ephemeral_system = Some(if base.is_empty() {
                    FINAL_TURN_INSTRUCTION.to_string()
                } else {
                    format!("{base}\n\n{FINAL_TURN_INSTRUCTION}")
                });
            }
            // 自动续写的文本先在 attempt 缓冲区聚合。provider 常会从上一段或
            // 整个回答开头重放；若边收边直接 push 到 TUI，去重时已经太晚。
            let recovering_text = stream_recoveries > 0;
            let mut recovery_content = String::new();
            let ws_snapshot = tool_runtime.workspace_snapshot();
            let process_snapshot = tool_runtime.processes_snapshot();
            let request = ModelRequest {
                model: agent_cfg.model.name.clone(),
                messages: build_context(
                    config,
                    &messages,
                    ephemeral_system.as_deref(),
                    tool_runtime.plan_snapshot().as_ref(),
                    Some(&ws_snapshot),
                    process_snapshot.as_deref(),
                ),
                tools: tool_defs.clone(),
                max_output_tokens: agent_cfg.model.max_output_tokens,
                reasoning: agent_cfg.model.reasoning.clone(),
                context_window: agent_cfg.model.context_window,
            };
            // 上下文占用投影（TUI 用量条；请求前发送）。
            if let Some(window) = agent_cfg.model.context_window {
                let usable = crate::context::usable_input(
                    window,
                    agent_cfg.model.max_output_tokens.unwrap_or(0) as u64,
                    agent_cfg.safety_reserve_tokens,
                );
                let system_prompt = system_prompt_text(
                    agent_cfg.system_prompt_extra.as_deref(),
                    ephemeral_system.as_deref(),
                );
                let projected =
                    crate::context::estimate_request(&system_prompt, &messages, &tool_defs);
                let _ = ui.send(LiveEvent::ContextUsage { projected, usable }).await;
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
                        consume_attempt_stream_event(
                            event,
                            request_id,
                            &mut content,
                            &mut recovery_content,
                            recovering_text,
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
                                    consume_attempt_stream_event(
                                        event,
                                        request_id,
                                        &mut content,
                                        &mut recovery_content,
                                        recovering_text,
                                        &mut saw_any_semantic,
                                        &mut saw_tool_calls,
                                        &ui,
                                    )
                                    .await;
                                }
                                flush_recovery_text(
                                    recovering_text,
                                    request_id,
                                    &mut content,
                                    &mut recovery_content,
                                    &ui,
                                )
                                .await;
                                // §6.2/§11.5：取消（Esc/Ctrl-C）是正常结束——提交已到达的内容，
                                // 记录 Cancelled 原因并保留 session，而不是让 run 以错误退出。
                                if !content.is_empty() {
                                    assistant_text = content.clone();
                                    session.commit(&SessionEvent::AssistantMessageCommitted {
                                            message: AssistantMessage {
                                                content: content.clone(),
                                                tool_calls: Vec::new(),
                                            },
                                        })
                                        .map_err(|e| RunFailure::Session(e.to_string()))?;
                                    // P1-1：已提交 session 的内容必须同步进 outcome.messages，
                                    // 否则继续对话时模型上下文与 session 事实不一致。
                                    messages.push(ChatMessage::Assistant {
                                        content: content.clone(),
                                        tool_calls: Vec::new(),
                                    });
                                }
                                // §16：区分取消来源——watchdog 超时不是用户取消。
                                let cause = cancel_cause.load(std::sync::atomic::Ordering::SeqCst);
                                let cancel_reason =
                                    crate::agent::limits::cancel_reason_for_cause(cause);
                                session.commit_terminal(&SessionEvent::RunCompleted {
                                        reason: cancel_reason,
                                        usage: usage_total,
                                    })
                                    .map_err(|e| RunFailure::Session(e.to_string()))?;
                                break 'run_loop cancel_reason;
                            }
                            Err(e) => {
                                // 错误详情记录到日志（§19.2：provider 错误可诊断）。
                                tracing::error!(%request_id, error = %e, "provider request failed");
                                // 先收完已入队的残余 delta（与 Cancelled 分支一致）。
                                while let Ok(event) = event_rx.try_recv() {
                                    consume_attempt_stream_event(
                                        event,
                                        request_id,
                                        &mut content,
                                        &mut recovery_content,
                                        recovering_text,
                                        &mut saw_any_semantic,
                                        &mut saw_tool_calls,
                                        &ui,
                                    )
                                    .await;
                                }
                                flush_recovery_text(
                                    recovering_text,
                                    request_id,
                                    &mut content,
                                    &mut recovery_content,
                                    &ui,
                                )
                                .await;
                            // §4.3 第二阶段：text-only attempt 断联 → 自动续写一次。
                            // 条件：已产生文本后流截断（StreamInterrupted，非未收内容
                            // 的 Connection）、未出现 tool delta（partial JSON 恢复风险大）、
                            // 还有续写额度。
                            if saw_any_semantic
                                && !saw_tool_calls
                                && recoverable_stream_interrupt(&e)
                                && stream_recoveries < MAX_STREAM_RECOVERIES
                            {
                                let cause = interrupt_cause(&e);
                                session.commit(&SessionEvent::AssistantAttemptInterrupted {
                                        request_id,
                                        content: content.clone(),
                                        cause,
                                        saw_tool_calls: false,
                                    })
                                    .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                let _ = ui
                                    .send(LiveEvent::StreamRecovering {
                                        attempt: stream_recoveries + 1,
                                    })
                                    .await;
                                stream_recoveries += 1;
                                // 续写请求（content 已累积 partial，recovery instruction
                                // 在下一个 attempt 循环开头注入 request）。
                                continue 'attempt;
                            }
                            // §4.3 第三阶段：partial tool-call 后断联 → 整个 turn 重新生成。
                            // 条件：已出现 tool delta（saw_tool_calls=true）、流截断
                            // （StreamInterrupted）、还有 restart 额度。
                            // 不尝试续接 partial JSON（风险大）；重新发起原始请求。
                            if saw_tool_calls
                                && recoverable_stream_interrupt(&e)
                                && turn_restarts < MAX_TURN_RESTARTS
                            {
                                let cause = interrupt_cause(&e);
                                session.commit(&SessionEvent::AssistantAttemptInterrupted {
                                        request_id,
                                        content: content.clone(),
                                        cause,
                                        saw_tool_calls: true,
                                    })
                                    .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                let _ = ui
                                    .send(LiveEvent::TurnRestarting {
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
                            // §4.3 第四阶段：无任何语义内容的**瞬时传输类失败**
                            // （Connection/Http/RateLimited）→ 自动重启整个 turn。
                            // 此时 content 为空，重启不会重复任何内容，完全安全。
                            // provider 内部已对无内容失败重试 MAX_ATTEMPTS 次；
                            // 这里再做 turn 级重启（每次 = 全新 provider 请求），
                            // 额度 MAX_TURN_RESTARTS——网络抖动/服务端瞬时错误
                            // 不应让整个任务失败、逼用户盯着重来（§用户诉求：
                            // 任何失败都应自动重试）。
                            if !saw_any_semantic
                                && is_transient_transport_error(&e)
                                && turn_restarts < MAX_TURN_RESTARTS
                            {
                                let _ = ui
                                    .send(LiveEvent::TurnRestarting {
                                        attempt: turn_restarts + 1,
                                    })
                                    .await;
                                turn_restarts += 1;
                                continue 'attempt;
                            }
                                // §4.3：区分"未收到任何语义事件"（连接不可用）与
                                // "已收到部分内容后断联"（记录 interrupted attempt）。
                                // partial content 已发给 UI 但不是一个完整 turn：
                                // 不提交为 AssistantMessageCommitted，写入记录型事件。
                                if saw_any_semantic {
                                    let cause = interrupt_cause(&e);
                                    session.commit(&SessionEvent::AssistantAttemptInterrupted {
                                            request_id,
                                            content: content.clone(),
                                            cause,
                                            saw_tool_calls,
                                        })
                                        .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                    session.commit_terminal(&SessionEvent::RunCompleted {
                                            reason: CompletionReason::ProviderInterrupted,
                                            usage: usage_total,
                                        })
                                        .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                    // 保留 partial 供 UI/outcome 展示，但不动 messages（不是已提交事实）。
                                    assistant_text = content.clone();
                                    break 'run_loop CompletionReason::ProviderInterrupted;
                                }
                                session.commit_terminal(&SessionEvent::RunCompleted {
                                        reason: CompletionReason::ProviderUnavailable,
                                        usage: usage_total,
                                    })
                                    .map_err(|e2| RunFailure::Session(e2.to_string()))?;
                                return Err(RunFailure::Provider(e.to_string()));
                            }
                        };
                        // provider 返回后仍可能有已经入队的末尾 delta；这些发送都已完成，
                        // 因而非阻塞 drain 即可，也不会依赖 future 内部 Sender 的析构时机。
                        while let Ok(event) = event_rx.try_recv() {
                            consume_attempt_stream_event(
                                event,
                                request_id,
                                &mut content,
                                &mut recovery_content,
                                recovering_text,
                                &mut saw_any_semantic,
                                &mut saw_tool_calls,
                                &ui,
                            )
                            .await;
                        }
                        flush_recovery_text(
                            recovering_text,
                            request_id,
                            &mut content,
                            &mut recovery_content,
                            &ui,
                        )
                        .await;
                        break 'attempt response;
                    }
                }
            }
        };
        // provider 返回的 usage 是本次请求的用量；跨轮累加（§2.2/§12.4）。
        accumulate_usage(&mut usage_total, response.usage);
        // §用户诉求：缓存命中实时展示（Claude Code 式）——每次请求结束就把
        // 本次 input/cache token 发给 TUI，不等 run 结束（此前只有 run 完成
        // 后的累计 ⇄ 显示，无法看到“本次请求缓存命中率”）。
        let _ = ui
            .send(LiveEvent::UsageUpdated {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                cache_read_tokens: response.usage.cache_read_tokens,
            })
            .await;
        // §27：每个 model turn 至少记录可回答“哪一轮、什么模型、多少工具、
        // 什么 finish reason、多少 token”的诊断行。
        tracing::debug!(
            turn,
            model = %agent_cfg.model.name,
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
            .commit(&SessionEvent::AssistantMessageCommitted {
                message: AssistantMessage {
                    content: content.clone(),
                    tool_calls: response.tool_calls.clone(),
                },
            })
            .map_err(|e| RunFailure::Session(e.to_string()))?;
        messages.push(ChatMessage::Assistant {
            content: content.clone(),
            tool_calls: response.tool_calls.clone(),
        });

        // 6. 工具调用（§12.2 batch 调度）。
        if !response.tool_calls.is_empty() {
            let batch = ToolBatchExecutor::<'_, _>::new(
                config,
                session,
                &mut messages,
                &mut progress,
                &tool_runtime,
                &ui,
            )
            .execute(
                provider,
                response.tool_calls,
                &mut tool_calls_total,
                &mut usage_total,
            )
            .await?;
            if let BatchEnd::SuspendRequested { args } = batch {
                // §13（AGENTS.md）：request_input 成功 → run 在该点挂起。
                // 记录 UserInputRequested（durable 事实）+ RunCompleted
                // (AwaitingUserInput)——等待用户输入**不等于** run 完成。
                let text = args.render();
                let text = if text.is_empty() {
                    "请提供你的输入".to_string()
                } else {
                    text
                };
                let questions = args.normalized_questions().unwrap_or_default();
                session
                    .commit(&SessionEvent::UserInputRequested {
                        prompt: text.clone(),
                    })
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
                session
                    .commit_terminal(&SessionEvent::RunCompleted {
                        reason: CompletionReason::AwaitingUserInput,
                        usage: usage_total,
                    })
                    .map_err(|e| RunFailure::Session(e.to_string()))?;
                awaiting_input = Some(AwaitingInput { text, questions });
                break 'run_loop CompletionReason::AwaitingUserInput;
            }
            if batch == BatchEnd::BudgetExceeded {
                // P1-2：工具预算超限用独立 reason（此前归为 Error，
                // 用户/模型会误以为是协议错误）。
                session
                    .commit_terminal(&SessionEvent::RunCompleted {
                        reason: CompletionReason::MaxToolCalls,
                        usage: usage_total,
                    })
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
            .commit_terminal(&SessionEvent::RunCompleted {
                reason,
                usage: usage_total,
            })
            .map_err(|e| RunFailure::Session(e.to_string()))?;
        break reason;
    };

    // P2-06：watchdog 由 Supervisor join（而非 abort）；Drop 兜底 cancel。
    let _ = watchdog_supervisor.shutdown().await;
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
        awaiting_input,
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
    ui: &mpsc::Sender<LiveEvent>,
    emit_text: bool,
) {
    match &event {
        ProviderEvent::TextDelta(_) => *saw_any_semantic = true,
        ProviderEvent::ToolCallStarted { .. } | ProviderEvent::ToolArgumentsDelta { .. } => {
            *saw_any_semantic = true;
            *saw_tool_calls = true;
        }
        ProviderEvent::ReasoningDelta(_) | ProviderEvent::Usage(_) => {}
    }
    forward_provider_event(event, request_id, content, ui, emit_text).await;
}

/// 自动续写 attempt 的文本不立即投影到 UI，而是先聚合、去掉 provider 重放的
/// 前缀，再作为一个 delta 追加。非文本事件仍实时转发。
#[allow(clippy::too_many_arguments)]
async fn consume_attempt_stream_event(
    event: ProviderEvent,
    request_id: RequestId,
    content: &mut String,
    recovery_content: &mut String,
    recovering_text: bool,
    saw_any_semantic: &mut bool,
    saw_tool_calls: &mut bool,
    ui: &mpsc::Sender<LiveEvent>,
) {
    let target = if recovering_text {
        recovery_content
    } else {
        content
    };
    consume_stream_event(
        event,
        request_id,
        target,
        saw_any_semantic,
        saw_tool_calls,
        ui,
        !recovering_text,
    )
    .await;
}

async fn flush_recovery_text(
    recovering_text: bool,
    request_id: RequestId,
    content: &mut String,
    recovery_content: &mut String,
    ui: &mpsc::Sender<LiveEvent>,
) {
    if !recovering_text || recovery_content.is_empty() {
        return;
    }
    let overlap = recovery_overlap_bytes(content, recovery_content);
    let append = recovery_content[overlap..].to_string();
    content.push_str(&append);
    recovery_content.clear();
    if !append.is_empty() {
        let _ = ui
            .send(LiveEvent::AssistantDelta {
                request_id,
                kind: DeltaKind::Text,
                text: append,
            })
            .await;
    }
}

/// 求 `existing` 后缀与 `replayed` 前缀的最长精确重叠。
/// provider 从整段开头重放是常见情况，单独用 `starts_with(existing)` 快速处理；
/// 其余用 KMP 在线扫描，保持线性复杂度并在 UTF-8 字符边界回退。
fn recovery_overlap_bytes(existing: &str, replayed: &str) -> usize {
    if existing.is_empty() || replayed.is_empty() {
        return 0;
    }
    if replayed.starts_with(existing) {
        return existing.len();
    }
    if existing.starts_with(replayed) {
        return replayed.len();
    }

    let pattern = replayed.as_bytes();
    let mut prefix = vec![0usize; pattern.len()];
    for i in 1..pattern.len() {
        let mut matched = prefix[i - 1];
        while matched > 0 && pattern[i] != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if pattern[i] == pattern[matched] {
            matched += 1;
        }
        prefix[i] = matched;
    }

    let bytes = existing.as_bytes();
    let mut matched = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate() {
        while matched > 0 && byte != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if byte == pattern[matched] {
            matched += 1;
        }
        if matched == pattern.len() && index + 1 < bytes.len() {
            matched = prefix[matched - 1];
        }
    }
    while matched > 0
        && (!replayed.is_char_boundary(matched)
            || !existing.is_char_boundary(existing.len().saturating_sub(matched)))
    {
        matched = prefix[matched - 1];
    }
    if matched >= MIN_RECOVERY_OVERLAP_BYTES {
        matched
    } else {
        0
    }
}

/// 把 provider 的单个增量同时投影到 session 待提交内容和 TUI。
async fn forward_provider_event(
    event: ProviderEvent,
    request_id: RequestId,
    content: &mut String,
    ui: &mpsc::Sender<LiveEvent>,
    emit_text: bool,
) {
    match event {
        ProviderEvent::TextDelta(text) => {
            content.push_str(&text);
            if emit_text {
                let _ = ui
                    .send(LiveEvent::AssistantDelta {
                        request_id,
                        kind: DeltaKind::Text,
                        text,
                    })
                    .await;
            }
        }
        ProviderEvent::ReasoningDelta(text) => {
            // §15.5：reasoning 只在 UI 展示，不进入 durable facts。
            let _ = ui
                .send(LiveEvent::AssistantDelta {
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
        ProviderError::Connection(_) | ProviderError::StreamInterrupted(_) => {
            InterruptCause::Connection
        }
        ProviderError::Http(_) => InterruptCause::Connection,
        ProviderError::Protocol(_) => InterruptCause::Protocol,
        ProviderError::RateLimited(_) => InterruptCause::RateLimited,
        ProviderError::Auth(_) => InterruptCause::Auth,
        ProviderError::Cancelled => InterruptCause::Other,
    }
}

/// §4.3：流中断是否值得自动恢复（续写/重生成）。
///
/// 只有连接层面已建立、已收到部分内容后才断流（`StreamInterrupted`）才恢复：
/// - 已产生文本但未出 tool delta → 续写（新请求带 recovery instruction）；
/// - 已出 tool delta → 整个 turn 重新生成。
///
/// `Connection`（未收到任何内容的传输失败）不恢复——provider 内部已按退避
/// 重试 `MAX_ATTEMPTS` 次仍失败，agent 再续写大概率同样失败，只增加网络请求
/// 与 UI 噪音（§用户诉求：修复"大量 run 失败"刷屏）。
fn recoverable_stream_interrupt(error: &crate::provider::ProviderError) -> bool {
    matches!(error, crate::provider::ProviderError::StreamInterrupted(_))
}

/// §4.3：瞬时传输类失败（网络抖动/服务端瞬时错误/限流）——重试可能成功，
/// 值得自动重启 turn；协议/认证错误是确定性问题，重试无意义（不重试）。
fn is_transient_transport_error(error: &crate::provider::ProviderError) -> bool {
    use crate::provider::ProviderError;
    matches!(
        error,
        ProviderError::Connection(_) | ProviderError::Http(_) | ProviderError::RateLimited(_)
    )
}

/// compaction 一轮（§15.4）。
async fn compact_turn<P: Provider, S: crate::session::store::SessionStore>(
    provider: &mut P,
    messages: &mut Vec<ChatMessage>,
    session: &mut S,
    config: &Config,
    cancel: &CancellationToken,
    usage_total: &mut Usage,
) -> Result<(), CompactionFailure> {
    // P1-05：compact_turn 只读窄视图 AgentConfig。
    let agent_cfg = config.agent_config();
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
        .ok_or_else(|| CompactionFailure::Session("session seq 已耗尽".into()))?;
    let events = session
        .events_with_seq()
        .map_err(|e| CompactionFailure::Session(e.to_string()))?;
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
        model: agent_cfg.model.name.clone(),
        messages: crate::context::compaction_request_messages(&history),
        tools: Vec::new(), // §15.4：compaction request 不提供任何工具 schema。
        // §用户诉求：摘要输出预算 1024 → 2048——大历史（300k 窗口）下
        // 1024 token 偏小，摘要被截断更容易丢核心字段。
        max_output_tokens: Some(2048),
        reasoning: agent_cfg.model.reasoning.clone(),
        context_window: agent_cfg.model.context_window,
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
                break result.map_err(|e| CompactionFailure::Provider(e.to_string()))?;
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
        return Err(CompactionFailure::Provider(format!(
            "invalid compaction response: finish={:?}, tool_calls={}",
            response.finish_reason,
            response.tool_calls.len()
        )));
    }
    let raw_summary = summary_text.clone(); // 保留原始输出（诊断用）
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
    // §用户诉求：细分失败原因（SummaryInvalid vs NotSignificant）。
    if summary_text.is_empty() {
        return Err(CompactionFailure::SummaryInvalid { raw: raw_summary });
    }
    if !crate::context::is_significant_shrink(original_tokens, summary_tokens) {
        return Err(CompactionFailure::NotSignificant);
    }
    session
        .commit(&SessionEvent::CompactionCommitted {
            covered,
            summary: crate::session::CompactSummary {
                text: summary_text.clone(),
            },
        })
        .map_err(|e| CompactionFailure::Session(e.to_string()))?;

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
fn system_prompt_text(system_prompt_extra: Option<&str>, ephemeral_system: Option<&str>) -> String {
    let mut system = DEFAULT_SYSTEM_PROMPT.to_string();
    if let Some(extra) = system_prompt_extra {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    // §Skills（README2 §20）：模型需要先看到 Available skills（metadata-only，
    // 不加载 body）才会调用 activate_skill。
    let available = crate::skills::SkillManager::global()
        .lock()
        .map(|manager| manager.available())
        .unwrap_or_default();
    if !available.is_empty() {
        system.push_str(
            "\n\n[Available skills]（metadata-only；用 activate_skill 激活后获取完整说明）：\n",
        );
        for skill in available {
            system.push_str(&format!("  {} — {}\n", skill.name, skill.description));
        }
        system.push_str("\nSkill 是 Instructions/Workflow/Knowledge，不是工具；激活后按其中步骤组合现有工具完成任务。");
    }
    if let Some(ephemeral) = ephemeral_system {
        system.push_str("\n\n");
        system.push_str(ephemeral);
    }
    system
}

/// 构造上下文 projection（§15.1 顺序：system → 用户目标 → 历史 turns → 当前输入在尾部）。
///
/// §13：计划通过正常的 `assistant(update_plan) → tool result` 协议事实进入上下文，
/// 但那只保证计划“存在过”，长任务中段 plan 会被后续工具输出挤到注意力边缘。
/// §用户诉求：每轮在**请求尾部**注入一条 system 角色的当前计划快照（`[当前计划·唯一权威]`）——
/// 用 system 角色（不是 User 消息）避免模型把计划当“用户再次要求按 Todo 继续”
/// 而反复确认/复述（旧坑见下）；放**尾部**而非 system 后，保持 system + 全部
/// 历史这个缓存前缀不变——plan 只在变化那轮打断尾部的一小段，稳定历史仍可命中
/// provider prompt cache。
///
/// 不变量：
/// - 有 plan 且非空才注入（无 plan 不打扰）；
/// - 注入内容 = plan_snapshot（§用户诉求：全部活跃项 + 完成计数——模型
///   每轮看到完整剩余计划才能准确增量更新），与侧边栏展示同一数据源
///   （tool::plan::plan_snapshot）；
/// - 不落 session、不进入对话投影（每次请求重建）。
///
/// 旧坑：不能把计划伪造成尾部 User 消息，否则模型反复确认/复述计划。
/// 正常计划轮保持原始时序，由 compaction 在窗口压力下统一归纳。
///
/// `ephemeral_system`：本次 request 的 harness control metadata（§4.3 续写指令），
/// 以 system 指令注入，不进入对话投影。
fn build_context(
    config: &Config,
    messages: &[ChatMessage],
    ephemeral_system: Option<&str>,
    plan: Option<&crate::tool::plan::Plan>,
    workspace: Option<&crate::workspace::ActiveWorkspace>,
    process_snapshot: Option<&str>,
) -> Vec<ChatMessage> {
    // P1-05：build_context 只读窄视图 AgentConfig。
    let agent_cfg = config.agent_config();
    let mut out = Vec::with_capacity(messages.len() + 3);
    out.push(ChatMessage::System(system_prompt_text(
        agent_cfg.system_prompt_extra.as_deref(),
        ephemeral_system,
    )));
    // §53：workspace identity 属于 Harness Context——每轮注入一条 system
    // 消息让模型知道当前工作区（Local/Remote）与 shell cwd，但不在每个
    // ToolOutcome 重复。
    if let Some(workspace) = workspace {
        let id = workspace.id().to_string();
        let cwd = {
            let shell = workspace.shell().lock().unwrap();
            shell.cwd.to_string()
        };
        out.push(ChatMessage::System(format!(
            "[当前 workspace]\nWorkspace: {id}\nShell cwd: {cwd}\n\n（工作区状态由 harness 管理，无需模型自行执行 ssh/cd 确认。）"
        )));
    }
    out.extend_from_slice(messages);
    // 尾部注入当前计划快照（system 角色；无 plan 或空 plan 不注入）。
    // §用户诉求：明确同步节奏——每完成一个步骤或方向改变就 update_plan。
    // §注入可靠性（用户反馈）：历史里 update_plan 的 tool result 不再带快照，
    // 但**必须**让模型区分“当前权威快照”与任何历史计划文本——用专属标记
    // 前缀 + 明确“以此为准，忽略历史中的任何旧计划”，防止模型引用过期快照。
    let snapshot = crate::tool::plan::plan_snapshot(plan);
    if !snapshot.is_empty() {
        out.push(ChatMessage::System(format!(
            "[当前计划·唯一权威·完整快照·以此为准]（每次 update_plan 都提交完整显式计划；每完成一项立即单独标记 completed，未完成项保持 pending/in_progress，不要一次性把全部项标记 completed。需要用户决定或外部条件时，先标记 blocked，再提问；忽略对话历史中出现的任何旧计划）：\n{snapshot}"
        )));
    }
    // §25/§26/§60：ManagedProcess 快照（system 角色 harness metadata，不是
    // User 指令）；只含 active + 近期状态变化，避免 context 膨胀；跨 turn /
    // compaction 后模型仍知道有后台进程存在（§25：不能彻底忘记 p17）。
    if let Some(process_snapshot) = process_snapshot {
        out.push(ChatMessage::System(format!(
            "[Managed processes]\n{process_snapshot}\n\n（后台进程由 TPI 管理；需要结果时用 `process` wait/status，不要频繁轮询）"
        )));
    }
    out
}

fn tool_result_succeeded(content: &str) -> bool {
    content
        .lines()
        .any(|line| line.trim() == "status: succeeded")
}

/// 确保当前 runtime history 至少包含一次成功计划结果。只在恢复/压缩边界调用，
/// 补入的消息随后随真实 assistant/tool 事实向后增长，不会每轮重新占据尾部。
fn ensure_plan_state_messages(
    messages: &mut Vec<ChatMessage>,
    plan: Option<&crate::tool::plan::Plan>,
) {
    let already_present = messages.iter().any(|message| {
        matches!(
            message,
            ChatMessage::Tool { name, content, .. }
                if crate::tool::BuiltinTool::from_name(name) == Some(crate::tool::BuiltinTool::UpdatePlan)
                    && tool_result_succeeded(content)
        )
    });
    if already_present {
        return;
    }
    if let Some(plan) = plan {
        append_restored_plan_round(messages, plan);
    }
}

/// Compaction 可能只留下 summary 与独立持久化的 PlanReplaced。此时用合法的
/// assistant/tool 配对恢复运行时计划，避免重新伪造一条 User 指令。
fn append_restored_plan_round(messages: &mut Vec<ChatMessage>, plan: &crate::tool::plan::Plan) {
    let snapshot = crate::tool::plan::plan_snapshot(Some(plan));
    if snapshot.is_empty() {
        return;
    }
    let provider_id = "tpi-restored-plan".to_string();
    messages.push(ChatMessage::Assistant {
        content: String::new(),
        tool_calls: vec![ToolCall {
            call_id: ToolCallId::new_v7(),
            provider_id: provider_id.clone(),
            name: "update_plan".into(),
            arguments: serde_json::to_string(plan).unwrap_or_else(|_| "{\"items\":[]}".into()),
        }],
    });
    messages.push(ChatMessage::Tool {
        tool_call_id: provider_id,
        name: "update_plan".into(),
        content: format!("status: succeeded\ntool: update_plan\n{snapshot}"),
    });
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
    /// 遗留测试辅助（P2-06 前 watchdog 的直接 abort 语义；现仅测试验证 Drop 行为）。
    struct AbortTaskOnDrop<T>(tokio::task::JoinHandle<T>);
    impl<T> Drop for AbortTaskOnDrop<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    use super::{
        build_context, ensure_plan_state_messages, recoverable_stream_interrupt,
        recovery_overlap_bytes,
    };
    use crate::provider::ChatMessage;
    use crate::tool::plan::{Plan, PlanItem, PlanStatus};

    /// 最小可用 Config（build_context 只读 system_prompt_extra 等字段）。
    /// P1 Exit gate：经 config::test_config 构造（tui 依赖收敛在 config），
    /// agent 测试不再直接引用 crate::tui。
    fn unit_config() -> crate::config::Config {
        crate::config::test_config(&camino::Utf8PathBuf::from("fake"))
    }

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

    /// §用户诉求：build_context 在请求尾部注入当前计划快照（system 角色）——
    /// 模型每轮都能看到 todo，而不是只存在于历史某条 update_plan tool result。
    /// 缓存语义：plan 不变时注入文本稳定，不破坏前缀缓存。
    #[test]
    fn build_context_appends_current_plan_as_system_role() {
        let plan = Plan {
            explanation: Some("修复侧边栏".into()),
            items: vec![PlanItem {
                text: "加宽侧边栏".into(),
                status: PlanStatus::InProgress,
            }],
        };
        let config = unit_config();
        let messages = vec![ChatMessage::User("hello".into())];
        let ctx = build_context(&config, &messages, None, Some(&plan), None, None);
        // 首条 = system prompt；中间 = 原 messages；尾部 = plan 快照（system 角色）。
        assert!(
            matches!(&ctx[0], ChatMessage::System(_)),
            "首条是 system prompt"
        );
        assert!(matches!(&ctx[1], ChatMessage::User(_)), "中间是原消息");
        assert_eq!(ctx.len(), 3, "尾部追加一条 plan 快照");
        let tail = match &ctx[2] {
            ChatMessage::System(text) => text.clone(),
            other => panic!("尾部必须是 system 角色: {other:?}"),
        };
        assert!(
            tail.contains("[当前计划·唯一权威"),
            "必须带唯一权威标记: {tail}"
        );
        assert!(tail.contains("加宽侧边栏"), "快照含计划项: {tail}");
        assert!(
            !tail.contains("hello"),
            "注入不得污染原对话（不是 User 消息）: {tail}"
        );
    }

    /// §用户诉求：全部项完成/取消后计划结束，build_context 不再注入尾部。
    #[test]
    fn build_context_skips_injection_when_plan_fully_completed() {
        let plan = Plan {
            explanation: None,
            items: vec![PlanItem {
                text: "done".into(),
                status: PlanStatus::Completed,
            }],
        };
        let config = unit_config();
        let messages = vec![ChatMessage::User("hi".into())];
        let ctx = build_context(&config, &messages, None, Some(&plan), None, None);
        assert_eq!(ctx.len(), 2, "全部完成后的计划不再注入尾部");
    }

    /// 无 plan 时不注入尾部（不打扰、不增加 token）。
    #[test]
    fn build_context_without_plan_injects_nothing() {
        let config = unit_config();
        let messages = vec![ChatMessage::User("hi".into())];
        let ctx = build_context(&config, &messages, None, None, None, None);
        assert_eq!(ctx.len(), 2, "无 plan 时只有 system + 原消息");
        assert!(matches!(&ctx[1], ChatMessage::User(_)));
    }

    /// §25/§26/§60：ManagedProcess snapshot 以 system 角色注入（harness
    /// metadata，不是 User 指令）；跨 turn 模型不会忘记活跃后台进程。
    #[test]
    fn build_context_injects_managed_process_snapshot_as_system_role() {
        let config = unit_config();
        let messages = vec![ChatMessage::User("hi".into())];
        let snapshot = "p17 running   python server.py  42.8s\np18 exited 0  wget model.bin";
        let ctx = build_context(&config, &messages, None, None, None, Some(snapshot));
        assert_eq!(ctx.len(), 3, "system + 原消息 + process snapshot");
        let tail = match &ctx[2] {
            ChatMessage::System(text) => text.clone(),
            other => panic!("process snapshot 必须是 system 角色: {other:?}"),
        };
        assert!(tail.contains("[Managed processes]"), "{tail}");
        assert!(tail.contains("p17 running"), "{tail}");
        assert!(tail.contains("p18 exited 0"), "{tail}");
        assert!(
            !tail.contains("hi"),
            "注入不得伪装成 User 消息（§26）: {tail}"
        );
    }

    /// §25：无活跃进程时不注入（避免 context 膨胀）。
    #[test]
    fn build_context_skips_process_snapshot_when_none() {
        let config = unit_config();
        let messages = vec![ChatMessage::User("hi".into())];
        let ctx = build_context(&config, &messages, None, None, None, None);
        assert_eq!(ctx.len(), 2, "无进程快照时不注入");
    }

    #[test]
    fn restored_plan_uses_tool_protocol_instead_of_fake_user_message() {
        let plan = Plan {
            explanation: None,
            items: vec![PlanItem {
                text: "检查 gcodes".into(),
                status: PlanStatus::InProgress,
            }],
        };
        let mut messages = Vec::new();
        ensure_plan_state_messages(&mut messages, Some(&plan));
        messages.push(ChatMessage::User("继续审查".into()));

        assert_eq!(messages.len(), 3);
        assert!(
            matches!(&messages[0], ChatMessage::Assistant { content, tool_calls }
            if content.is_empty() && tool_calls.len() == 1 && tool_calls[0].name == "update_plan")
        );
        assert!(
            matches!(&messages[1], ChatMessage::Tool { name, content, .. }
            if name == "update_plan" && content.contains("检查 gcodes"))
        );
        assert!(matches!(messages.last(), Some(ChatMessage::User(text)) if text == "继续审查"));
    }

    #[test]
    fn restored_plan_is_inserted_only_once() {
        let plan = Plan {
            explanation: None,
            items: vec![PlanItem {
                text: "检查 gcodes".into(),
                status: PlanStatus::InProgress,
            }],
        };
        let mut messages = Vec::new();
        ensure_plan_state_messages(&mut messages, Some(&plan));
        ensure_plan_state_messages(&mut messages, Some(&plan));
        assert_eq!(messages.len(), 2, "恢复态不能在每次请求尾部重复注入");
    }

    /// §修复：只有"已收到部分内容后截断"（StreamInterrupted）才值得自动续写/
    /// 重生成；未收内容的传输失败（Connection）不再触发——provider 内部已重试
    /// 耗尽，agent 续写只增加网络请求与 UI 噪音（"大量 run 失败"刷屏）。
    #[test]
    fn recoverable_stream_interrupt_distinguishes_truncation_from_connect_failure() {
        use crate::provider::ProviderError;
        assert!(
            recoverable_stream_interrupt(&ProviderError::StreamInterrupted(
                "stream ended before [DONE]".into()
            )),
            "已收内容后截断必须可恢复（续写/重生成）"
        );
        assert!(
            !recoverable_stream_interrupt(&ProviderError::Connection(
                "attempt 3: connect timeout".into()
            )),
            "未收内容的连接失败不得自动续写（provider 已重试耗尽）"
        );
        assert!(
            !recoverable_stream_interrupt(&ProviderError::Protocol("x".into())),
            "协议错误不可恢复"
        );
    }

    #[test]
    fn recovery_overlap_handles_full_replay_suffix_and_utf8() {
        let existing = "第一段\n第二段已经输出";
        let full_replay = "第一段\n第二段已经输出，继续内容";
        assert_eq!(
            recovery_overlap_bytes(existing, full_replay),
            existing.len()
        );

        let suffix_replay = "第二段已经输出，继续内容";
        assert_eq!(
            recovery_overlap_bytes(existing, suffix_replay),
            "第二段已经输出".len()
        );
        assert_eq!(recovery_overlap_bytes(existing, "完全不同的新内容"), 0);
    }
}
