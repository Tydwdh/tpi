//! P8-03/P8-04 接线：把 in-process child 调查暴露为 `subagent` 工具。
//!
//! 模型可调用 `subagent` 发起一次**只读调查**（depth=1、concurrency=1）：
//! child 拥有独立 session/trace，只读工具白名单（read/list/search/glob），
//! 完成返回 structured report（summary + evidence）。parent 只接收 report，
//! 不接收 child 的流式事件（raw stream 不灌主 transcript）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use tpi_capabilities::tool::ToolContext;
use tpi_capabilities::tool::registry::{Tool, ToolOrigin};
use tpi_core::ids::SessionId;
use tpi_core::outcome::ToolOutcome;

use crate::agent::LiveEvent;
use crate::provider::Provider;
use crate::subagent::child::InProcessChildProvider;
use crate::subagent::{ReadOnlyCapability, SubagentProvider, SubagentRequest};

/// `subagent` 工具：发起只读调查（parent 侧接线）。
///
/// `P` = provider 类型；工厂用 `Arc<dyn Fn() -> P>` 持有——并行执行时每个
/// child 需要独立 provider 实例，Arc 工厂可重复调用（P8-10 多指令并行）。
pub struct SubagentTool<P>
where
    P: Provider + Send + 'static,
{
    config: Arc<tpi_config::config::Config>,
    make_provider: std::sync::Arc<dyn Fn() -> P + Send + Sync>,
    /// P8-06：child 完成时发 SubagentReported 的通道（parent TUI summary card）。
    report_tx: Option<tokio::sync::mpsc::Sender<LiveEvent>>,
    _provider: std::marker::PhantomData<fn() -> P>,
}

impl<P> SubagentTool<P>
where
    P: Provider + Send + 'static,
{
    pub fn new<F>(
        config: Arc<tpi_config::config::Config>,
        make_provider: F,
        report_tx: Option<tokio::sync::mpsc::Sender<LiveEvent>>,
    ) -> Self
    where
        F: Fn() -> P + Send + Sync + 'static,
    {
        Self {
            config,
            make_provider: std::sync::Arc::new(make_provider),
            report_tx,
            _provider: std::marker::PhantomData,
        }
    }
}

/// `subagent` 工具参数（JSON schema 与 execute 解析一致）。
#[derive(Debug, serde::Deserialize)]
struct SubagentArgs {
    /// 调查指令（child 的 user message；只读调查，不修改 workspace）。
    /// §去重（A/B 单选）：一次调用 = 一个 child = 一张 TUI 卡片；并行调查
    /// 由模型在同一 wave 发多个 `subagent` 调用（ReadOnly 类别批内并行）。
    #[serde(default)]
    instruction: String,
    /// 只读能力白名单（默认 read/list/search/glob；显式传则覆盖）。
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

impl SubagentArgs {
    fn parse_capabilities(&self) -> Result<Vec<ReadOnlyCapability>, String> {
        let Some(caps) = &self.capabilities else {
            return Ok(vec![
                ReadOnlyCapability::Read,
                ReadOnlyCapability::Search,
                ReadOnlyCapability::Glob,
            ]);
        };
        let mut out = Vec::new();
        for name in caps {
            let cap = match name.as_str() {
                "read" => ReadOnlyCapability::Read,
                "search" => ReadOnlyCapability::Search,
                "glob" => ReadOnlyCapability::Glob,
                other => {
                    return Err(format!("未知只读能力: {other:?}（可用: read/search/glob）"));
                }
            };
            if !out.contains(&cap) {
                out.push(cap);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl<P> Tool for SubagentTool<P>
where
    P: Provider + Send + 'static,
{
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "发起一次只读子代理调查：child 拥有独立 session/trace，只能调用只读工具 \
         (read/search/glob)，返回结构化报告（summary + 证据引用）。适合并行 \
         独立调查、问题定位、代码审计：一次调用 = 一个 child；需要并行时在同一 \
         wave 发起多个 subagent 调用（每个独立卡片、独立观察）。depth=1（child 不再发起 child）。"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "instruction": {
                    "type": "string",
                    "description": "单条调查指令（child 的 user message；只读调查，不修改 workspace）"
                },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["read", "search", "glob"] },
                    "description": "只读能力白名单（默认 read/search/glob；目录浏览走 read depth）"
                }
            },
            "description": "发起一次只读子代理调查：child 拥有独立 session/trace，只能调用只读工具 (read/search/glob)，返回结构化报告（summary + 证据引用）。适合并行独立调查、问题定位、代码审计：一次调用 = 一个 child；需要并行时在同一 wave 发起多个 subagent 调用（每个独立卡片、独立观察）。depth=1（child 不再发起 child）。"
        })
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    /// P8-10：`subagent` 是只读工具（child 白名单仅 read/list/search/glob，
    /// 无写副作用）——声明 `ReadOnly` 使其与批内 read/search 并行执行
    ///（默认 WorkspaceUnknown 会按独立 wave 串行）。
    fn access_class(&self) -> tpi_capabilities::tool::registry::ToolAccessClass {
        tpi_capabilities::tool::registry::ToolAccessClass::ReadOnly
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolOutcome {
        let parsed: SubagentArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failed(
                    self.name(),
                    tpi_core::outcome::ModelPayload {
                        status: tpi_core::outcome::ToolStatus::Failed,
                        program: None,
                        exit_code: Some(2),
                        duration_ms: 0,
                        output: format!("subagent 参数解析失败: {e}"),
                        effect: None,
                        artifact: None,
                    },
                );
            }
        };
        if parsed.instruction.trim().is_empty() {
            return ToolOutcome::failed(
                self.name(),
                tpi_core::outcome::ModelPayload {
                    status: tpi_core::outcome::ToolStatus::Failed,
                    program: None,
                    exit_code: Some(2),
                    duration_ms: 0,
                    output: "subagent 需要非空 instruction".into(),
                    effect: None,
                    artifact: None,
                },
            );
        }
        let capabilities = match parsed.parse_capabilities() {
            Ok(c) => c,
            Err(e) => {
                return ToolOutcome::failed(
                    self.name(),
                    tpi_core::outcome::ModelPayload {
                        status: tpi_core::outcome::ToolStatus::Failed,
                        program: None,
                        exit_code: Some(2),
                        duration_ms: 0,
                        output: e,
                        effect: None,
                        artifact: None,
                    },
                );
            }
        };
        if capabilities.is_empty() {
            return ToolOutcome::failed(
                self.name(),
                tpi_core::outcome::ModelPayload {
                    status: tpi_core::outcome::ToolStatus::Failed,
                    program: None,
                    exit_code: Some(2),
                    duration_ms: 0,
                    output: "subagent capabilities 不能为空（至少一项只读能力）".into(),
                    effect: None,
                    artifact: None,
                },
            );
        }

        // §去重（A/B 单选）：一次调用 = 一个 child。child 独立 session/trace/
        // provider 实例；取消用 ctx.cancel（= 当前 run 的 token），用户 Esc /
        // Ctrl-C / watchdog 超时能中止 child。child 活动事件（assistant 文本 /
        // 工具调用）经 ctx.output_tx 以 ToolOutputDelta 转发——TUI 卡片运行中
        // 实时可见、可进入内部视图观察。
        let child_workspace = tpi_capabilities::workspace::ActiveWorkspace::local(
            tpi_capabilities::workspace::LocalWorkspace::new(
                self.config.workspace_root.clone(),
                self.config.allow_outside_workspace,
            ),
        );
        let report_tx = self.report_tx.clone();
        let output_tx = ctx.output_tx.clone();
        let parent_call_id = ctx.call_id;
        let mut child = InProcessChildProvider::<P, _>::new(
            {
                let make_provider = self.make_provider.clone();
                move || (make_provider)()
            },
            self.config.clone(),
            child_workspace,
        )
        .with_report_tx(report_tx)
        .with_output_tx(output_tx, parent_call_id);
        let request = SubagentRequest {
            instruction: parsed.instruction.clone(),
            child_session: SessionId::new_v7(),
            capabilities,
            parent: None, // 工具路径无 parent trace（child 自己起新 trace）
        };
        // §bug 修复：测量 child 全部完成的实际耗时并写入结果——此前直接
        // ToolOutcome::succeeded/failed（duration_ms 默认 0），TUI 工具卡片
        // 显示耗时恒为 0。
        let started = std::time::Instant::now();
        let result = child.run_investigation(request, ctx.cancel.clone()).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(report) => {
                let mut output = format!(
                    "子代理调查完成（session {}）\n{}",
                    report.child_session, report.summary
                );
                if !report.evidence.is_empty() {
                    output.push_str("\n证据:\n");
                    for e in &report.evidence {
                        output.push_str(&format!("  {e}\n"));
                    }
                }
                if let Some(trace_id) = report.trace_id {
                    output.push_str(&format!("trace: {trace_id}\n"));
                }
                ToolOutcome::succeeded(self.name(), output).with_timing(duration_ms)
            }
            Err(e) => ToolOutcome::failed(
                self.name(),
                tpi_core::outcome::ModelPayload {
                    status: tpi_core::outcome::ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms,
                    output: format!("子代理调查失败: {e}"),
                    effect: None,
                    artifact: None,
                },
            ),
        }
    }
}

/// 供 registry 注册使用的类型擦除构造（composition root 调用）。
/// P8-04：parent 侧把 `subagent` 工具注入 registry，模型即可发起只读调查。
pub fn register_subagent_tool<P, F>(
    registry: &Arc<std::sync::Mutex<tpi_capabilities::tool::registry::ToolRegistry>>,
    config: Arc<tpi_config::config::Config>,
    make_provider: F,
    report_tx: Option<tokio::sync::mpsc::Sender<LiveEvent>>,
) where
    P: Provider + Send + 'static,
    F: Fn() -> P + Send + Sync + 'static,
{
    let tool: Arc<dyn Tool> = Arc::new(SubagentTool::<P>::new(config, make_provider, report_tx));
    registry
        .lock()
        .unwrap()
        .register_validated(tool)
        .expect("subagent 工具定义必须合法");
}

/// P8-04 接线：subagent 工具端到端（ChildFake 返回 report → 工具输出含 summary）。
#[cfg(test)]
mod tool_tests {
    use super::*;
    use crate::provider::{FinishReason, ProviderEvent, ProviderResponse};
    use tokio_util::sync::CancellationToken;
    use tpi_capabilities::shell::ShellSessionState;
    use tpi_capabilities::tool::ToolContext;
    use tpi_capabilities::workspace::{ActiveWorkspace, LocalWorkspace};
    use tpi_config::config::Config;
    use tpi_core::ids::ToolCallId;

    /// 脚本化 fake provider（测试专用；child 返回一条调查结论）。
    struct ChildFake;
    impl Provider for ChildFake {
        fn model_name(&self) -> &str {
            "child-fake"
        }
        async fn stream(
            &mut self,
            _request: crate::provider::ModelRequest,
            events: tokio::sync::mpsc::Sender<ProviderEvent>,
            cancel: CancellationToken,
        ) -> Result<ProviderResponse, crate::provider::ProviderError> {
            if cancel.is_cancelled() {
                return Err(crate::provider::ProviderError::Cancelled);
            }
            let response = ProviderResponse {
                finish_reason: FinishReason::Stop,
                tool_calls: Vec::new(),
                usage: tpi_session::Usage::default(),
            };
            let _ = events
                .send(ProviderEvent::TextDelta(
                    "child 调查完成，见 @artifact/child-1".into(),
                ))
                .await;
            Ok(response)
        }
    }

    fn test_config() -> Arc<Config> {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut config = tpi_config::config::test_config(&root);
        config.workspace_root = root.clone();
        config.sessions_root = root.join("sessions").into();
        config.artifacts_root = root.join("artifacts").into();
        Arc::new(config)
    }

    /// 最小 ToolContext（subagent 工具 execute 只读 ctx.cancel 做取消传播）。
    fn minimal_ctx() -> ToolContext {
        ToolContext {
            workspace_root: camino::Utf8PathBuf::from("C:/proj"),
            allow_outside_workspace: false,
            cancel: CancellationToken::new(),
            artifacts_root: std::path::PathBuf::from("C:/artifacts"),
            session_id: "test-session".into(),
            call_id: ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            shell: Arc::new(std::sync::Mutex::new(ShellSessionState::new(
                camino::Utf8PathBuf::from("C:/proj"),
            ))),
            workspace: Arc::new(std::sync::Mutex::new(ActiveWorkspace::local(
                LocalWorkspace::new(camino::Utf8PathBuf::from("C:/proj"), false),
            ))),
            processes: Default::default(),
            registry: Default::default(),
            interactive: false,
        }
    }

    #[tokio::test]
    async fn subagent_tool_returns_report() {
        let tool = SubagentTool::new(test_config(), || ChildFake, None);
        let ctx = minimal_ctx();
        let outcome = tool
            .execute(r#"{"instruction": "调查 src/main.rs"}"#, &ctx)
            .await;
        assert_eq!(outcome.status, tpi_core::outcome::ToolStatus::Succeeded);
        let text = outcome.model_text();
        assert!(text.contains("子代理调查完成"), "输出含标题: {text}");
        assert!(text.contains("child 调查完成"), "输出含 summary: {text}");
        assert!(
            text.contains("@artifact/child-1"),
            "输出含 evidence: {text}"
        );
        assert!(text.contains("session 0"), "输出含 child_session: {text}");
    }

    #[tokio::test]
    async fn subagent_tool_rejects_empty_instruction() {
        let tool = SubagentTool::new(test_config(), || ChildFake, None);
        let ctx = minimal_ctx();
        let outcome = tool.execute(r#"{"instruction": "  "}"#, &ctx).await;
        assert_eq!(outcome.status, tpi_core::outcome::ToolStatus::Failed);
        assert!(outcome.model_text().contains("非空 instruction"));
    }

    #[tokio::test]
    async fn subagent_tool_rejects_unknown_capability() {
        let tool = SubagentTool::new(test_config(), || ChildFake, None);
        let ctx = minimal_ctx();
        let outcome = tool
            .execute(r#"{"instruction": "x", "capabilities": ["write"]}"#, &ctx)
            .await;
        assert_eq!(outcome.status, tpi_core::outcome::ToolStatus::Failed);
        assert!(outcome.model_text().contains("未知只读能力"));
    }

    #[tokio::test]
    async fn subagent_tool_name_and_schema() {
        let tool = SubagentTool::new(test_config(), || ChildFake, None);
        assert_eq!(tool.name(), "subagent");
        let schema = tool.input_schema();
        // §去重（A/B 单选）：一次调用 = 一个 child；schema 只有 instruction。
        assert_eq!(schema["properties"]["instruction"]["type"], "string");
        assert!(
            schema["properties"].get("instructions").is_none(),
            "instructions 数组已移除（一次调用 = 一个 child）"
        );
        assert_eq!(
            schema["properties"]["capabilities"]["items"]["enum"][0],
            "read"
        );
    }

    /// §去重（A/B 单选）：instructions 数组已移除——传它应失败（未知参数
    /// 容忍或按指令非空处理；此处验证不产生多 child 行为）。
    #[tokio::test]
    async fn subagent_tool_ignores_instructions_array() {
        let tool = SubagentTool::new(test_config(), || ChildFake, None);
        let ctx = minimal_ctx();
        // instruction 缺失：必须失败（不再有 instructions 兜底）。
        let outcome = tool.execute(r#"{"instructions": ["a", "b"]}"#, &ctx).await;
        assert_eq!(outcome.status, tpi_core::outcome::ToolStatus::Failed);
        assert!(outcome.model_text().contains("非空 instruction"));
    }

    /// §bug 修复：结果携带真实耗时——此前 duration_ms 恒为 0，TUI 工具卡片
    /// 显示耗时 0。SlowChildFake sleep 150ms（单 child）。
    #[tokio::test]
    async fn subagent_result_reports_real_duration() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        struct SlowChildFake {
            _active: Arc<AtomicUsize>,
        }
        impl Provider for SlowChildFake {
            fn model_name(&self) -> &str {
                "slow-child-fake"
            }
            async fn stream(
                &mut self,
                _request: crate::provider::ModelRequest,
                events: tokio::sync::mpsc::Sender<ProviderEvent>,
                cancel: CancellationToken,
            ) -> Result<ProviderResponse, crate::provider::ProviderError> {
                if cancel.is_cancelled() {
                    return Err(crate::provider::ProviderError::Cancelled);
                }
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                let _ = events
                    .send(ProviderEvent::TextDelta("child 调查完成".into()))
                    .await;
                Ok(ProviderResponse {
                    finish_reason: FinishReason::Stop,
                    tool_calls: Vec::new(),
                    usage: tpi_session::Usage::default(),
                })
            }
        }

        let tool = SubagentTool::new(
            test_config(),
            || SlowChildFake {
                _active: Arc::new(AtomicUsize::new(0)),
            },
            None,
        );
        let ctx = minimal_ctx();
        let outcome = tool.execute(r#"{"instruction": "调查 a.rs"}"#, &ctx).await;
        assert_eq!(outcome.status, tpi_core::outcome::ToolStatus::Succeeded);
        let duration = outcome.model_payload.duration_ms;
        assert!(
            duration >= 100,
            "duration_ms 必须反映真实耗时（>=100ms，实际 {duration}）"
        );
        assert_eq!(
            outcome.timing.duration_ms, duration,
            "timing 与 model_payload 一致"
        );
    }
}

/// P8-04 接线：register_subagent_tool 把工具注册进 registry（composition root 路径）。
#[cfg(test)]
mod register_tests {
    use super::*;
    use tokio_util::sync::CancellationToken;
    use tpi_capabilities::tool::registry::ToolRegistry;

    #[test]
    fn register_adds_subagent_to_registry() {
        use crate::provider::{FinishReason, ProviderEvent, ProviderResponse};
        struct FakeP;
        impl Provider for FakeP {
            fn model_name(&self) -> &str {
                "fake"
            }
            async fn stream(
                &mut self,
                _req: crate::provider::ModelRequest,
                events: tokio::sync::mpsc::Sender<ProviderEvent>,
                _cancel: CancellationToken,
            ) -> Result<ProviderResponse, crate::provider::ProviderError> {
                let _ = events.send(ProviderEvent::TextDelta("ok".into())).await;
                Ok(ProviderResponse {
                    finish_reason: FinishReason::Stop,
                    tool_calls: Vec::new(),
                    usage: tpi_session::Usage::default(),
                })
            }
        }
        let registry = Arc::new(std::sync::Mutex::new(ToolRegistry::new()));
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config = tpi_config::config::test_config(&root);
        register_subagent_tool::<FakeP, _>(&registry, Arc::new(config), || FakeP, None);
        let reg = registry.lock().unwrap();
        assert!(reg.get("subagent").is_some(), "registry 含 subagent 工具");
        let desc = reg
            .descriptors()
            .into_iter()
            .find(|d| d.name == "subagent")
            .expect("subagent descriptor");
        assert!(desc.description.contains("只读子代理调查"));
    }
}
