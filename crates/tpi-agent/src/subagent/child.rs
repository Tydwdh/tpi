//! In-process agent worker: isolated session/context, shared tool directory,
//! graph-routed cancellation and interaction resume.

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::agent::{self, LiveEvent, RunInput};
use crate::provider::Provider;
use crate::subagent::{SubagentProvider, SubagentReport, SubagentRequest};
use tpi_capabilities::tool::ToolStreamEvent;
use tpi_capabilities::tool::registry::{ToolRegistry, builtin_registry};
use tpi_capabilities::workspace::ActiveWorkspace;
use tpi_core::ids::{RunId, ToolCallId};
use tpi_session::store::SessionLog;

/// in-process child provider（P8-04）。
///
/// `make_provider`：child 用独立 provider 实例（parent 与 child 不共享 &mut
/// provider——parent 阻塞等 child，child 独占自己的实例；concurrency 1 无竞争）。
pub struct InProcessChildProvider<P, F> {
    make_provider: F,
    config: Arc<tpi_config::config::Config>,
    workspace: ActiveWorkspace,
    /// P8-06：child 完成后发出 SubagentReported 的通道（None = 不投影 TUI）。
    report_tx: Option<tokio::sync::mpsc::Sender<crate::agent::LiveEvent>>,
    /// §子代理实时观察：child 活动事件（assistant 文本/工具调用）经此通道
    /// 以 ToolOutputDelta 转发到 parent 的 TUI（None = 不转发）。
    output_tx: Option<tokio::sync::mpsc::Sender<ToolStreamEvent>>,
    /// parent 视角的调用 id：转发 ToolStreamEvent 时用它匹配 parent 的卡片
    ///（child 内部事件的 call_id 是 child 命名空间，TUI 无法匹配）。
    parent_call_id: Option<ToolCallId>,
    /// Shared session registry. When present, this runtime gets the same
    /// tool surface as its parent; the legacy fallback is retained only for
    /// isolated compatibility tests.
    registry: Option<Arc<Mutex<ToolRegistry>>>,
    /// Shared agent graph for recursive delegation and tree cancellation.
    agents: Option<Arc<Mutex<crate::agent::manager::AgentManager>>>,
    /// Session-scoped process/terminal owner. Production child runs always
    /// receive the parent's manager; `None` is only for isolated provider
    /// compatibility tests.
    resources: Option<Arc<tpi_capabilities::resource::ResourceManager>>,
    _provider: std::marker::PhantomData<P>,
}

impl<P, F> InProcessChildProvider<P, F> {
    pub fn new(
        make_provider: F,
        config: Arc<tpi_config::config::Config>,
        workspace: ActiveWorkspace,
    ) -> Self {
        Self {
            make_provider,
            config,
            workspace,
            report_tx: None,
            output_tx: None,
            parent_call_id: None,
            registry: None,
            agents: None,
            resources: None,
            _provider: std::marker::PhantomData,
        }
    }

    /// P8-06：绑定 report 通道（child 完成时发 SubagentReported；
    /// None = 不投影）。
    pub fn with_report_tx(
        mut self,
        report_tx: Option<tokio::sync::mpsc::Sender<crate::agent::LiveEvent>>,
    ) -> Self {
        self.report_tx = report_tx;
        self
    }

    /// §子代理实时观察：绑定 parent 的流式输出通道 + 本调用在 parent 侧的
    /// call id（child 活动 → ToolOutputDelta → parent TUI 卡片实时可见）。
    pub fn with_output_tx(
        mut self,
        output_tx: Option<tokio::sync::mpsc::Sender<ToolStreamEvent>>,
        parent_call_id: ToolCallId,
    ) -> Self {
        self.output_tx = output_tx;
        self.parent_call_id = Some(parent_call_id);
        self
    }

    pub fn with_registry(mut self, registry: Arc<Mutex<ToolRegistry>>) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn with_agent_manager(
        mut self,
        agents: Arc<Mutex<crate::agent::manager::AgentManager>>,
    ) -> Self {
        self.agents = Some(agents);
        self
    }

    pub fn with_resource_manager(
        mut self,
        resources: Arc<tpi_capabilities::resource::ResourceManager>,
    ) -> Self {
        self.resources = Some(resources);
        self
    }
}

/// 从 assistant 文本提取证据引用（`@artifact/...`）。
fn evidence_from(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find("@artifact/") {
        let start = idx;
        let tail = &rest[start..];
        let end = tail
            .char_indices()
            .find(|(i, c)| *i > "@artifact/".len() && c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        out.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    out
}

/// 把 child 的 LiveEvent 映射为 TUI 可见的文本行（None = 不转发）。
///
/// §子代理实时观察：转发对观察 child 有意义的语义事件（assistant 文本增量、
/// 工具启动/完成、工具实时输出），忽略过程性事件（context/usage/恢复等）
/// 避免噪音。文本行直接拼接进 subagent 卡片 output（append_tool_output），
/// TUI 内部视图按原文渲染。
fn child_event_to_text(event: &LiveEvent) -> Option<String> {
    match event {
        LiveEvent::AssistantDelta { text, .. } if !text.is_empty() => Some(text.clone()),
        LiveEvent::ToolStarted {
            name, arguments, ..
        } => {
            let summary = arguments.trim();
            let summary = if summary.len() > 100 {
                let mut s: String = summary.chars().take(97).collect();
                s.push('…');
                s
            } else {
                summary.to_string()
            };
            Some(format!("\n▸ {name} {summary}\n"))
        }
        LiveEvent::ToolCompleted {
            name,
            status,
            duration_ms,
            ..
        } => Some(format!("  ✓ {name} · {status:?} · {duration_ms}ms\n")),
        LiveEvent::ToolOutputDelta { text, .. } if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

#[async_trait::async_trait]
impl<P: Provider + Send, F: Fn() -> P + Send> SubagentProvider for InProcessChildProvider<P, F> {
    async fn run_investigation(
        &mut self,
        request: SubagentRequest,
        cancel: CancellationToken,
    ) -> Result<SubagentReport, String> {
        // 独立 child provider 实例（不与 parent 争用 &mut provider）。
        let mut child_provider = (self.make_provider)();

        // 独立 child session（隔离目录，不与 parent session 混）。
        let child_root = self.config.sessions_root.join("child");
        std::fs::create_dir_all(&child_root)
            .map_err(|e| format!("创建 child sessions 目录失败: {e}"))?;
        let mut child_session = SessionLog::create_with_id(
            &child_root,
            self.config.workspace_root.as_std_path(),
            RunId::new_v7(),
            request.child_session,
        )
        .map_err(|e| format!("创建 child session 失败: {e}"))?;

        // Every normal runtime supplies the same full registry to root and child
        // agents. Keep a full built-in fallback for direct provider tests; a
        // child must never silently receive a reduced capability directory.
        let registry: Arc<Mutex<ToolRegistry>> = self
            .registry
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(builtin_registry())));
        let agents = self
            .agents
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(crate::agent::manager::AgentManager::new())));

        // §子代理实时观察：child 的 LiveEvent 转发为文本行经 output_tx 送出
        //（ToolOutputDelta，call_id 换成 parent 视角）——TUI 的 subagent 卡片
        // 运行中实时显示 child 内部活动；无 output_tx 时退化为丢弃（drain）。
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel::<crate::agent::LiveEvent>(64);
        let forward_tx = self.output_tx.clone();
        let forward_call_id = self.parent_call_id;
        let forward = tokio::spawn(async move {
            let Some(tx) = forward_tx else {
                while ui_rx.recv().await.is_some() {}
                return;
            };
            let call_id = forward_call_id.unwrap_or_else(ToolCallId::new_v7);
            while let Some(event) = ui_rx.recv().await {
                let Some(text) = child_event_to_text(&event) else {
                    continue;
                };
                // channel 满时丢弃新帧（lossy telemetry，BUG-012 同款处理）：
                // 实时观察是尽力而为；`send().await` 会背压到 child 的
                // LiveEvent 消费路径（64 容量），慢 UI 会拖慢 child。
                let _ = tx.try_send(ToolStreamEvent {
                    call_id,
                    stream: 1,
                    text,
                });
            }
        });

        let resources = self
            .resources
            .clone()
            .unwrap_or_else(|| Arc::new(tpi_capabilities::resource::ResourceManager::new()));
        let mut user_message = request.instruction.clone();
        let outcome = loop {
            let history = tpi_session::store::replay_messages(
                tpi_session::store::SessionStore::path(&child_session),
            )
            .map_err(|e| format!("读取 child history 失败: {e}"))?;
            let outcome = agent::run(
                &mut child_provider,
                &mut child_session,
                &self.config,
                RunInput {
                    history: &history,
                    user_message,
                    ui: ui_tx.clone(),
                    cancel: cancel.clone(),
                    interactive: true,
                    force_compaction: false,
                    workspace: Some(self.workspace.clone()),
                    registry: registry.clone(),
                    // Child is a normal runtime: it shares graph, tool
                    // registry, and managed resources, while its
                    // conversation/session remains isolated.
                    resources: resources.clone(),
                    agents: agents.clone(),
                },
            )
            .await
            .map_err(|e| format!("child run 失败: {e}"))?;
            let Some(awaiting) = outcome.awaiting_input.as_ref() else {
                break outcome;
            };
            let Some(agent_id) = agents
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .agent_for_session(request.child_session)
            else {
                return Err("child requested input without an agent graph owner".into());
            };
            let (interaction, answer) = agents
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .request_input(agent_id, awaiting.text.clone())?;
            tracing::info!(
                %agent_id,
                request_id = %interaction.request_id,
                "agent waiting for routed input"
            );
            // The report channel is the existing UI boundary for child
            // activity. Mark this as progress; the actual answer is routed by
            // `agent action=message` and never leaks into a parent prompt.
            if let Some(report_tx) = &self.report_tx {
                let _ = report_tx
                    .send(crate::agent::LiveEvent::SubagentReported {
                        child_session: request.child_session,
                        summary: format!(
                            "agent {} waiting for input (request {}): {}",
                            agent_id, interaction.request_id, awaiting.text
                        ),
                        evidence: vec![],
                    })
                    .await;
            }
            user_message = tokio::select! {
                answer = answer => answer.map_err(|_| "input request cancelled".to_string())?,
                _ = cancel.cancelled() => return Err("child run cancelled（parent cancel 传播）".into()),
            };
        };
        // No further child run can send UI events after the loop. Close the
        // producer before joining the forwarder; otherwise the forwarder
        // would wait forever on its receiver.
        drop(ui_tx);
        // child run 结束 → child 的 ui sender 已 drop → 转发任务 flush 后退出。
        let _ = forward.await;

        // O8（P8-09）：child trace 与 parent 的双向引用（link 事件）。child run
        // 总是新 TraceId（context 隔离，不继承 parent trace）；parent 侧上下文
        // 存在时记录双向 id，否则记 remote_boundary（无 parent 上下文，如独立
        // 测试/诊断路径）。
        if let Some(parent) = &request.parent {
            tracing::info!(
                parent_trace_id = %parent.trace_id,
                parent_span_id = %parent.span_id,
                child_trace_id = %outcome.trace_id,
                child_session_id = %request.child_session,
                "subagent.link"
            );
        } else {
            tracing::info!(
                parent_trace_id = "(remote_boundary)",
                child_trace_id = %outcome.trace_id,
                child_session_id = %request.child_session,
                "subagent.link"
            );
        }

        // parent cancel 传播：child run 以 Cancelled 正常结束（§11.5 取消是正常
        // 终态，非错误）——此时 parent 不需要 report，按契约返回 Err。
        if outcome.reason == tpi_session::CompletionReason::Cancelled {
            return Err("child run cancelled（parent cancel 传播）".into());
        }
        // O8：report commit（因果链终点：parent 发起 -> child run -> report）。
        tracing::info!(
            child_trace_id = %outcome.trace_id,
            child_session_id = %request.child_session,
            "subagent.report_committed"
        );
        // P8-06：child 完成 → 发 SubagentReported（parent TUI 投影 summary card）。
        if let Some(report_tx) = &self.report_tx {
            let _ = report_tx
                .send(crate::agent::LiveEvent::SubagentReported {
                    child_session: request.child_session,
                    summary: outcome.assistant_text.clone(),
                    evidence: evidence_from(&outcome.assistant_text),
                })
                .await;
        }
        Ok(SubagentReport {
            child_session: request.child_session,
            summary: outcome.assistant_text.clone(),
            evidence: evidence_from(&outcome.assistant_text),
            trace_id: Some(outcome.trace_id),
            cancelled: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::LiveEvent;
    use crate::provider::{Provider, ProviderResponse};
    use tpi_capabilities::workspace::LocalWorkspace;
    use tpi_config::config::Config;
    use tpi_core::ids::RequestId;
    use tpi_core::outcome::ToolStatus;

    /// §子代理实时观察：child 事件 → 文本行的映射——有意义的语义事件转发、
    /// 过程性事件忽略；空文本不转发。
    #[test]
    fn child_event_to_text_maps_semantic_events() {
        let rid = RequestId::new_v7();
        let text = child_event_to_text(&LiveEvent::AssistantDelta {
            request_id: rid,
            kind: crate::agent::DeltaKind::Text,
            text: "正在阅读 src/main.rs".into(),
        });
        assert_eq!(text.as_deref(), Some("正在阅读 src/main.rs"));

        let text = child_event_to_text(&LiveEvent::ToolStarted {
            call_id: ToolCallId::new_v7(),
            name: "read".into(),
            arguments: r#"{"path": "src/main.rs"}"#.into(),
        });
        assert!(
            text.as_deref()
                .is_some_and(|t| t.contains("▸ read") && t.contains("src/main.rs")),
            "工具启动行: {text:?}"
        );

        let text = child_event_to_text(&LiveEvent::ToolCompleted {
            call_id: ToolCallId::new_v7(),
            name: "read".into(),
            status: ToolStatus::Succeeded,
            duration_ms: 12,
            exit_code: None,
            output: String::new(),
            diff: None,
        });
        assert!(
            text.as_deref()
                .is_some_and(|t| t.contains("✓ read") && t.contains("12ms")),
            "工具完成行: {text:?}"
        );

        // 实时输出增量直接转发。
        let text = child_event_to_text(&LiveEvent::ToolOutputDelta {
            call_id: ToolCallId::new_v7(),
            stream: 1,
            text: "progress…".into(),
        });
        assert_eq!(text.as_deref(), Some("progress…"));

        // 过程性事件忽略。
        let text = child_event_to_text(&LiveEvent::ContextUsage {
            projected: 10,
            usable: 100,
        });
        assert_eq!(text, None, "ContextUsage 不转发");
        let text = child_event_to_text(&LiveEvent::StepStarted { step: 1 });
        assert_eq!(text, None, "StepStarted 不转发");
        // 空文本不转发。
        let text = child_event_to_text(&LiveEvent::AssistantDelta {
            request_id: rid,
            kind: crate::agent::DeltaKind::Text,
            text: String::new(),
        });
        assert_eq!(text, None, "空 assistant 增量不转发");
    }

    /// 脚本化 fake provider（实现 Provider；测试专用）。
    pub(crate) struct ChildFake;
    impl Provider for ChildFake {
        fn model_name(&self) -> &str {
            "child-fake"
        }
        async fn stream(
            &mut self,
            _request: crate::provider::ModelRequest,
            events: tokio::sync::mpsc::Sender<crate::provider::ProviderEvent>,
            cancel: CancellationToken,
        ) -> Result<ProviderResponse, crate::provider::ProviderError> {
            if cancel.is_cancelled() {
                return Err(crate::provider::ProviderError::Cancelled);
            }
            let response = crate::provider::ProviderResponse {
                finish_reason: crate::provider::FinishReason::Stop,
                tool_calls: Vec::new(),
                usage: tpi_session::Usage::default(),
            };
            let _ = events
                .send(crate::provider::ProviderEvent::TextDelta(
                    "child 调查完成，见 @artifact/child-1".into(),
                ))
                .await;
            Ok(response)
        }
    }

    pub(crate) fn test_config() -> Arc<Config> {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut config = tpi_config::config::test_config(&root);
        config.workspace_root = root.clone();
        config.sessions_root = root.join("sessions").into();
        config.artifacts_root = root.join("artifacts").into();
        Arc::new(config)
    }

    /// conformance：child 执行返回 structured report（child_session 匹配 + summary）。
    #[tokio::test]
    async fn child_returns_structured_report() {
        let mut provider = InProcessChildProvider::new(
            || ChildFake,
            test_config(),
            ActiveWorkspace::local(LocalWorkspace::new(
                camino::Utf8PathBuf::from("C:/proj"),
                false,
            )),
        );
        let child = tpi_core::ids::SessionId::new_v7();
        let report = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "调查 src/main.rs".into(),
                    child_session: child,
                    parent: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("child 执行成功");
        assert_eq!(report.child_session, child, "child session 匹配");
        assert!(
            report.summary.contains("child 调查完成"),
            "summary 携带 child 输出: {}",
            report.summary
        );
        assert!(
            report.evidence.contains(&"@artifact/child-1".to_string()),
            "evidence 提取 artifact 引用: {:?}",
            report.evidence
        );
    }

    /// parent cancel 传播：cancel 后 child 立即 cancelled（不继续执行）。
    #[tokio::test]
    async fn parent_cancel_propagates_to_child() {
        let mut provider = InProcessChildProvider::new(
            || ChildFake,
            test_config(),
            ActiveWorkspace::local(LocalWorkspace::new(
                camino::Utf8PathBuf::from("C:/proj"),
                false,
            )),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "调查".into(),
                    child_session: tpi_core::ids::SessionId::new_v7(),
                    parent: None,
                },
                cancel,
            )
            .await;
        assert!(
            result.is_err(),
            "parent cancel 后 child 必须失败（不继续执行）"
        );
    }

    /// Direct provider construction still receives the complete built-in tool
    /// directory, matching the production composition root.
    #[test]
    fn fallback_registry_matches_full_agent_directory() {
        let registry = builtin_registry();
        assert!(registry.get("read").is_some(), "read 可用");
        assert!(registry.get("bash").is_some(), "bash 可用");
        assert!(registry.get("edit").is_some(), "edit 可用");
        assert!(registry.get("write").is_some(), "write 可用");
    }
}

/// O8（P8-09）trace link 验收（追加在 child.rs 内联测试之后）。
#[cfg(test)]
mod o8_tests {
    use super::tests::{ChildFake, test_config};
    use super::*;
    use crate::subagent::ParentTraceContext;
    use tpi_capabilities::workspace::LocalWorkspace;

    /// child run 总是新 TraceId；report 携带（parent 可查询）。
    #[tokio::test]
    async fn child_report_carries_its_own_trace_id() {
        let mut provider = InProcessChildProvider::new(
            || ChildFake,
            test_config(),
            ActiveWorkspace::local(LocalWorkspace::new(
                camino::Utf8PathBuf::from("C:/proj"),
                false,
            )),
        );
        let report = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "调查".into(),
                    child_session: tpi_core::ids::SessionId::new_v7(),
                    parent: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("child 执行成功");
        assert!(report.trace_id.is_some(), "report 携带 child trace id");
        assert!(!report.cancelled, "正常完成非 cancelled");
    }

    /// link 事件字段（parent/child 双向 id）--用 tracing capture 验证不可行
    ///（无全局 subscriber 注入点）；改为验证 ParentTraceContext 可构造并
    /// 传递（类型层面链路完整；事件输出由 trace.rs catalog 测试保证 name 合法）。
    #[test]
    fn parent_trace_context_constructs_and_links_in_catalog() {
        let ctx = ParentTraceContext {
            trace_id: tpi_core::ids::TraceId::new_v7(),
            span_id: tpi_core::ids::SpanId::new_v7(),
        };
        // link 事件名已在 trace catalog 登记（无孤儿 name）。
        assert!(crate::trace::is_registered("subagent.link"));
        assert!(crate::trace::is_registered("subagent.report_committed"));
        let _ = ctx; // 类型可构造（parent 侧上下文完整）
    }

    /// cancel 因果链：parent cancel -> child Cancelled 终态 -> Err（report 不
    /// commit；child trace 仍可从 link 事件查询）。
    #[tokio::test]
    async fn cancel_causal_chain_returns_err_without_report() {
        let mut provider = InProcessChildProvider::new(
            || ChildFake,
            test_config(),
            ActiveWorkspace::local(LocalWorkspace::new(
                camino::Utf8PathBuf::from("C:/proj"),
                false,
            )),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "调查".into(),
                    child_session: tpi_core::ids::SessionId::new_v7(),
                    parent: Some(ParentTraceContext {
                        trace_id: tpi_core::ids::TraceId::new_v7(),
                        span_id: tpi_core::ids::SpanId::new_v7(),
                    }),
                },
                cancel,
            )
            .await;
        assert!(result.is_err(), "cancel -> Err（无 report commit）");
    }
}

/// P8-06：child 完成经 report_tx 发 SubagentReported（TUI summary card 数据源）。
#[cfg(test)]
mod p8_06_tests {
    use super::tests::{ChildFake, test_config};
    use super::*;
    use crate::agent::LiveEvent;
    use tpi_capabilities::workspace::ActiveWorkspace;
    use tpi_capabilities::workspace::LocalWorkspace;

    #[tokio::test]
    async fn child_emits_subagent_reported_event() {
        let (report_tx, mut report_rx) = tokio::sync::mpsc::channel::<LiveEvent>(8);
        let mut provider = InProcessChildProvider::new(
            || ChildFake,
            test_config(),
            ActiveWorkspace::local(LocalWorkspace::new(
                camino::Utf8PathBuf::from("C:/proj"),
                false,
            )),
        )
        .with_report_tx(Some(report_tx));
        let report = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "调查".into(),
                    child_session: tpi_core::ids::SessionId::new_v7(),
                    parent: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("child 执行成功");
        // report_tx 收到 SubagentReported（summary 与 report 一致）。
        let event = report_rx.recv().await.expect("收到 SubagentReported");
        match event {
            LiveEvent::SubagentReported { summary, .. } => {
                assert_eq!(summary, report.summary, "事件 summary == report summary");
            }
            other => panic!("期望 SubagentReported，实际 {other:?}"),
        }
    }
}
