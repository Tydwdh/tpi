//! Run-scoped state shared by every built-in tool invocation.
//!
//! Keeping construction here makes `ToolContext` an implementation detail of the
//! agent/tool boundary. New context fields have one initialization site instead
//! of one site for execution and another for post-execution observation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{RunFailure, RuntimeEvent};
use crate::config::Config;
use crate::ids::ToolCallId;
use crate::provider::{ChatMessage, ToolCall};
use crate::session::{RecoveryMetadata, SessionEvent, SessionLog};
use crate::tool::edit::SnapshotStore;
use crate::tool::outcome::{StoredToolOutcome, ToolOutcome, ToolStatus};
use crate::tool::plan::Plan;
use crate::tool::search::ScanSnapshot;
use crate::tool::{self, BuiltinTool, ToolContext, ToolStreamEvent};
use camino::Utf8PathBuf;

pub(super) struct ToolRuntime {
    config: RuntimeConfig,
    cancel: CancellationToken,
    session_id: String,
    interactive: bool,
    scan_snapshots: Arc<Mutex<HashMap<String, ScanSnapshot>>>,
    snapshot_store: Arc<Mutex<SnapshotStore>>,
    current_plan: Arc<Mutex<Option<Plan>>>,
}

#[derive(Clone)]
struct RuntimeConfig {
    workspace_root: camino::Utf8PathBuf,
    allow_outside_workspace: bool,
    artifacts_root: std::path::PathBuf,
    shell_path: Option<camino::Utf8PathBuf>,
}

impl ToolRuntime {
    pub(super) fn new(
        config: &Config,
        session_id: String,
        cancel: CancellationToken,
        interactive: bool,
    ) -> Self {
        Self {
            config: RuntimeConfig {
                workspace_root: config.workspace_root.clone(),
                allow_outside_workspace: config.allow_outside_workspace,
                artifacts_root: config.artifacts_root.clone(),
                shell_path: config.shell_path.clone(),
            },
            cancel,
            session_id,
            interactive,
            scan_snapshots: Default::default(),
            snapshot_store: Default::default(),
            current_plan: Default::default(),
        }
    }

    pub(super) fn plan_snapshot(&self) -> Option<Plan> {
        crate::util::lock_mutex(&self.current_plan, "current_plan").clone()
    }

    pub(super) fn context(
        &self,
        call_id: ToolCallId,
        output_tx: Option<mpsc::Sender<ToolStreamEvent>>,
    ) -> ToolContext {
        ToolContext {
            workspace_root: self.config.workspace_root.clone(),
            allow_outside_workspace: self.config.allow_outside_workspace,
            cancel: self.cancel.clone(),
            artifacts_root: self.config.artifacts_root.clone(),
            session_id: self.session_id.clone(),
            call_id,
            output_tx,
            scan_snapshots: self.scan_snapshots.clone(),
            shell_path: self.config.shell_path.clone(),
            snapshot_store: self.snapshot_store.clone(),
            current_plan: self.current_plan.clone(),
            interactive: self.interactive,
        }
    }
}
/// batch 执行结果（§12.4：预算超限时明确结束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchEnd {
    Continue,
    BudgetExceeded,
}

/// Owns the mutable boundary of one tool-call batch.
///
/// The model-turn state machine only supplies calls and a budget counter; tool
/// persistence, scheduling, progress detection, UI events, and result injection
/// remain cohesive inside this module.
pub(super) struct ToolBatchExecutor<'a> {
    config: &'a Config,
    session: &'a mut SessionLog,
    messages: &'a mut Vec<ChatMessage>,
    progress: &'a mut crate::agent::scheduler::ProgressTracker,
    runtime: &'a ToolRuntime,
    ui: &'a mpsc::Sender<RuntimeEvent>,
}

impl<'a> ToolBatchExecutor<'a> {
    pub(super) fn new(
        config: &'a Config,
        session: &'a mut SessionLog,
        messages: &'a mut Vec<ChatMessage>,
        progress: &'a mut crate::agent::scheduler::ProgressTracker,
        runtime: &'a ToolRuntime,
        ui: &'a mpsc::Sender<RuntimeEvent>,
    ) -> Self {
        Self {
            config,
            session,
            messages,
            progress,
            runtime,
            ui,
        }
    }

    pub(super) async fn execute(
        self,
        calls: Vec<ToolCall>,
        tool_calls_total: &mut u32,
    ) -> Result<BatchEnd, RunFailure> {
        execute_batch(self, calls, tool_calls_total).await
    }
}

/// 执行一次 tool-call batch（§12.2）。
///
/// 1. 全部参数预检（invalid → `rejected`，不启动）；
/// 2. 按资源冲突构建 waves；
/// 3. 同 wave 无冲突 Pure/Read 并行（受 `max_parallel_tools` 限制）；
///    Write / WorkspaceUnknown 按源顺序；
/// 4. 结果无论完成先后都按原 call index 送回 provider（§12.2 第 6 条）。
async fn execute_batch(
    executor: ToolBatchExecutor<'_>,
    calls: Vec<ToolCall>,
    tool_calls_total: &mut u32,
) -> Result<BatchEnd, RunFailure> {
    use crate::agent::scheduler::{
        PreparedCall, action_key, build_waves, stable_observation, state_stamp_from_ctx,
    };
    use futures_util::future::join_all;
    use std::collections::HashMap;

    let ToolBatchExecutor {
        config,
        session,
        messages,
        progress,
        runtime: tool_runtime,
        ui,
    } = executor;
    let max_parallel = config.limits.max_parallel_tools as usize;

    // 1. 预检全部参数（§12.2 第 1 条）。
    let mut prepared: Vec<PreparedCall> = Vec::with_capacity(calls.len());
    let mut rejected: HashMap<usize, StoredToolOutcome> = HashMap::new();
    for (index, call) in calls.iter().enumerate() {
        if *tool_calls_total >= config.limits.max_tool_calls {
            // §PointerHit 2：预算超限时，为剩余 tool calls 合成标准化拒绝结果
            // 并持久化——否则 assistant.tool_calls 有 call 但无对应 tool result，
            // 下一轮/resume 的 history 是非法消息序列（provider 可能拒绝）。
            for skipped in &calls[index..] {
                if let Some(tool) = BuiltinTool::from_name(&skipped.name) {
                    let outcome = ToolOutcome::failed(
                        tool.name(),
                        crate::tool::outcome::ModelPayload {
                            status: ToolStatus::Rejected,
                            program: None,
                            exit_code: None,
                            duration_ms: 0,
                            output: "status: rejected
tool: {tool}
error: tool_budget_exceeded

工具调用预算（max_tool_calls）已耗尽，本批剩余调用未执行。"
                                .replace("{tool}", tool.name()),
                            effect: None,
                            artifact: None,
                        },
                    )
                    .into_stored();
                    session
                        .append_event(&SessionEvent::ToolRequested {
                            call: skipped.clone(),
                        })
                        .and_then(|_| session.complete_tool(skipped.call_id, &outcome))
                        .map_err(|e| RunFailure::Session(e.to_string()))?;
                }
            }
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
                // §10.7：写工具预检时生成一次提交计划；write-ahead 与执行复用。
                let plan = write_tool_plan(
                    tool,
                    call,
                    &config.workspace_root,
                    config.allow_outside_workspace,
                );
                prepared.push(PreparedCall {
                    source_index: index,
                    tool,
                    args,
                    access,
                    action_key: action_key(tool, &call.arguments),
                    plan,
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
            // §10.7：复用预检阶段生成的同一 plan（temp/backup 路径一致）。
            let recovery = recovery_metadata(
                call.tool,
                source,
                call.plan.as_ref(),
                &config.workspace_root,
                config.allow_outside_workspace,
            );
            if call.tool.requires_write_ahead() {
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
            let ctx = tool_runtime.context(calls[source_index].call_id, Some(output_tx.clone()));
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

            // §10.7 第 6 步：backup 保留到 ToolCompleted 持久化之后；
            // 记录清理路径（成功后删除）。复用预检阶段生成的同一 plan。
            let plan = call.plan.clone();
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
                let ctx = tool_runtime.context(calls[index].call_id, None);
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
                let plan = tool_runtime.plan_snapshot();
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
                        diff: outcome.session_metadata.diff.clone(),
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

/// P0-11：backup 清理策略——只有工具成功提交后才允许删除 backup。
/// 失败（含 commit_recovery_failed：无法证明恢复完成）时 backup 是唯一
/// 恢复现场，必须保留（§10.7 第 5 条“保留所有文件”）。
fn backup_cleanup_allowed(status: ToolStatus) -> bool {
    status == ToolStatus::Succeeded
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
