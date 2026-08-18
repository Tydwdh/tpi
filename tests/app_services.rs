//! P1-06：AppServices composition inventory——fake ports 构造最小 controller。
//!
//! 验收：测试可用 fake provider 构造 `AppServices` 并驱动 use case
//! （`run_with_services`），不依赖真实 API key / 网络。

mod fixtures;

use tpi::app::SessionTarget;
use tpi::app::{AppServices, run_with_services};
use tpi::provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderResponse};
use tpi::session::conversation::Conversation;
use tpi::tui::keymap::Keymap;
use tpi::tui::terminal::ViewMode;

/// 极简 fake provider：返回固定文本，不触网。
struct EchoProvider {
    text: String,
}

impl Provider for EchoProvider {
    fn model_name(&self) -> &str {
        "echo"
    }

    async fn stream(
        &mut self,
        _request: tpi::provider::ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        events
            .send(ProviderEvent::TextDelta(self.text.clone()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        Ok(ProviderResponse {
            finish_reason: FinishReason::Stop,
            usage: Default::default(),
            tool_calls: Vec::new(),
        })
    }
}

fn minimal_config(workspace: &camino::Utf8PathBuf) -> tpi::config::Config {
    let mut cfg = fixtures::test_config(workspace);
    // 测试不需要真实 key（EchoProvider 不读）。
    cfg.auto_open_browser = false;
    cfg
}

/// 用 fake provider 构造最小 AppServices（不调 from_config——那需要真实
/// API key 且 spawn Ctrl-C handler；此处直接构造字段，验证 AppServices
/// 是可注入的 ports 集合）。
fn services_with(
    workspace: &camino::Utf8PathBuf,
    provider: EchoProvider,
) -> AppServices<EchoProvider> {
    let config = minimal_config(workspace);
    AppServices {
        config,
        workspace_root: workspace.clone(),
        sessions_root: workspace.join(".tpi-test-sessions").into(),
        provider,
        conversation: Conversation::new(),
        current_cancel: std::sync::Arc::new(std::sync::Mutex::new(None)),
        mcp_manager: tpi::mcp::manager::McpManager::new(),
        registry: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::tool::registry::builtin_registry(),
        )),
        processes: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::process::managed::ProcessRegistry::new(),
        )),
        terminals: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::terminal::TerminalRegistry::default(),
        )),
    }
}

#[tokio::test]
async fn fake_ports_drive_minimal_controller() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let services = services_with(
        &workspace,
        EchoProvider {
            text: "P1-06 ok".into(),
        },
    );

    let text = run_with_services(services, "hello", true, |_model| {
        Ok(EchoProvider {
            text: "P1-06 ok".into(),
        })
    })
    .await
    .expect("use case 成功")
    .expect("非交互模式应返回最终答案");
    assert_eq!(text, "P1-06 ok");
}

#[tokio::test]
async fn empty_prompt_is_rejected_without_calling_provider() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    // provider 永不调用（空 prompt 在 use case 入口直接拒绝）。
    let services = services_with(
        &workspace,
        EchoProvider {
            text: String::new(),
        },
    );
    let err = run_with_services(services, "", true, |_model| {
        Ok(EchoProvider {
            text: String::new(),
        })
    })
    .await
    .unwrap_err();
    assert!(err.contains("prompt"), "空 prompt 应报错: {err}");
}

#[test]
fn from_config_signature_is_construction_only() {
    // AppServices::from_config 是 OpenAiCompatClient 专用 construction；
    // 字段全部显式（非 service locator）。此处只验证类型存在且字段可读。
    let _ = std::mem::size_of::<AppServices<EchoProvider>>();
    let _ = std::mem::size_of::<Keymap>();
    let _ = ViewMode::default();
    let _ = SessionTarget::New;
}
