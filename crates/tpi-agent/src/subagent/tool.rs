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

/// `subagent` 工具：发起一次只读调查（parent 侧接线）。
///
/// 泛型参数与 [`InProcessChildProvider`] 一致：`P` = provider 类型，
/// `F` = provider 工厂（每个 child 独立实例）。
pub struct SubagentTool<P, F>
where
    P: Provider + Send + 'static,
    F: Fn() -> P + Send + Sync + 'static,
{
    config: Arc<tpi_config::config::Config>,
    make_provider: F,
    /// P8-06：child 完成时发 SubagentReported 的通道（parent TUI summary card）。
    report_tx: Option<tokio::sync::mpsc::Sender<LiveEvent>>,
    _provider: std::marker::PhantomData<fn() -> P>,
}

impl<P, F> SubagentTool<P, F>
where
    P: Provider + Send + 'static,
    F: Fn() -> P + Send + Sync + 'static,
{
    pub fn new(
        config: Arc<tpi_config::config::Config>,
        make_provider: F,
        report_tx: Option<tokio::sync::mpsc::Sender<LiveEvent>>,
    ) -> Self {
        Self {
            config,
            make_provider,
            report_tx,
            _provider: std::marker::PhantomData,
        }
    }
}

/// `subagent` 工具参数（JSON schema 与 execute 解析一致）。
#[derive(Debug, serde::Deserialize)]
struct SubagentArgs {
    /// 调查指令（child 的 user message；只读调查，不修改 workspace）。
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
                ReadOnlyCapability::List,
                ReadOnlyCapability::Search,
                ReadOnlyCapability::Glob,
            ]);
        };
        let mut out = Vec::new();
        for name in caps {
            let cap = match name.as_str() {
                "read" => ReadOnlyCapability::Read,
                "list" => ReadOnlyCapability::List,
                "search" => ReadOnlyCapability::Search,
                "glob" => ReadOnlyCapability::Glob,
                other => {
                    return Err(format!(
                        "未知只读能力: {other:?}（可用: read/list/search/glob）"
                    ));
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
impl<P, F> Tool for SubagentTool<P, F>
where
    P: Provider + Send + 'static,
    F: Fn() -> P + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "发起一次只读子代理调查：child 拥有独立 session/trace，只能调用只读工具 \
         (read/list/search/glob)，返回结构化报告（summary + 证据引用）。适合并行 \
         独立调查、问题定位、代码审计。depth=1（child 不再发起 child）。"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "instruction": {
                    "type": "string",
                    "description": "调查指令（child 的 user message）"
                },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["read", "list", "search", "glob"] },
                    "description": "只读能力白名单（默认全部四项）"
                }
            },
            "required": ["instruction"]
        })
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
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

        // 发起一次只读调查（concurrency=1：工具每次一个 child；depth=1）。
        let child_session = SessionId::new_v7();
        let request = SubagentRequest {
            instruction: parsed.instruction.clone(),
            child_session,
            capabilities,
            parent: None, // 工具路径无 parent trace（child 自己起新 trace）
        };
        let mut child_provider = InProcessChildProvider::new(
            || (self.make_provider)(),
            self.config.clone(),
            tpi_capabilities::workspace::ActiveWorkspace::local(
                tpi_capabilities::workspace::LocalWorkspace::new(
                    self.config.workspace_root.clone(),
                    self.config.allow_outside_workspace,
                ),
            ),
        )
        .with_report_tx(self.report_tx.clone());

        // 取消传播（C5 修复）：必须用 ctx.cancel（= 当前 run 的 cancel token），
        // 不能用新 token——否则用户 Esc / Ctrl-C / watchdog 超时无法中止 child
        // 调查，join_all 会一直等它完成，run 卡住无法取消。
        let cancel = ctx.cancel.clone();
        match child_provider.run_investigation(request, cancel).await {
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
                ToolOutcome::succeeded(self.name(), output)
            }
            Err(e) => ToolOutcome::failed(
                self.name(),
                tpi_core::outcome::ModelPayload {
                    status: tpi_core::outcome::ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("subagent 调查失败: {e}"),
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
    let tool: Arc<dyn Tool> = Arc::new(SubagentTool::<P, F>::new(config, make_provider, report_tx));
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
        assert_eq!(schema["required"][0], "instruction");
        assert_eq!(
            schema["properties"]["capabilities"]["items"]["enum"][0],
            "read"
        );
    }
}

/// P8-04 接线：register_subagent_tool 把工具注册进 registry（composition root 路径）。
#[cfg(test)]
mod register_tests {
    use super::*;
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
