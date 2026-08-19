//! P3-02：AppController 验收——fake runtime/session/platform 的 integration tests。
//!
//! 覆盖：controller 接收 `UiIntent` → 返回 AppEffect；cancel run 传播到 cancel
//! token；start new session 重置会话；Quit 请求渲染。不引用 Crossterm/Ratatui。

use std::sync::{Arc, Mutex};

use tpi::app::AppServices;
use tpi::app::controller::AppController;
use tpi::app::intent::{AppCommand, AppEffect, IntentSource, UiIntent};
use tpi::provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderResponse};

/// 最小 fake provider（同 `tests/app_services.rs` 的 `EchoProvider`）。
struct EchoProvider;
impl Provider for EchoProvider {
    fn model_name(&self) -> &'static str {
        "echo"
    }
    async fn stream(
        &mut self,
        _request: tpi::provider::ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        events
            .send(ProviderEvent::TextDelta("fake ok".into()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        Ok(ProviderResponse {
            finish_reason: FinishReason::Stop,
            usage: Default::default(),
            tool_calls: Vec::new(),
        })
    }
}

fn services() -> AppServices<EchoProvider> {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = tpi::config::Config {
        model: tpi::config::ModelConfig {
            provider: "openai".into(),
            name: "echo".into(),
            base_url: "http://localhost".into(),
            reasoning: None,
            max_output_tokens: None,
            context_window: None,
            api_key_env: "TEST_KEY".into(),
            api_key: None,
            price_input: None,
            price_output: None,
        },
        models: Vec::new(),
        limits: Default::default(),
        workspace_root: workspace.clone(),
        sessions_root: dir.path().join("sessions"),
        artifacts_root: dir.path().join("artifacts"),
        shell_path: None,
        safety_reserve_tokens: 0,
        auto_open_browser: false,
        web_summary_model: String::new(),
        system_prompt_extra: None,
        source: "test".into(),
        ui_theme: "omp".into(),
        ui_mode: tpi::tui::terminal::ViewMode::Fullscreen,
        ui_keymap: tpi::tui::keymap::Keymap::default(),
        ui_collapsed_lines: 10,
        allow_outside_workspace: true,
    };
    AppServices {
        config,
        workspace_root: workspace,
        sessions_root: dir.path().join("sessions"),
        provider: EchoProvider,
        conversation: tpi::session::conversation::Conversation::new(),
        current_cancel: Arc::new(Mutex::new(None)),
        mcp_manager: tpi::mcp::manager::McpManager::new(),
        registry: Arc::new(Mutex::new(tpi::tool::registry::builtin_registry())),
        processes: Arc::new(Mutex::new(tpi::process::managed::ProcessRegistry::new())),
        terminals: Arc::new(Mutex::new(tpi::terminal::TerminalRegistry::default())),
        agents: Arc::new(Mutex::new(tpi_agent::agent::manager::AgentManager::new())),
    }
}

fn intent(cmd: AppCommand) -> UiIntent {
    UiIntent::new(cmd, IntentSource::Keyboard)
}

/// Cancel run：controller 取消 `current_cancel` token（若在跑）。
#[test]
fn cancel_run_cancels_token() {
    let mut controller = AppController::new(services());
    let token = tokio_util::sync::CancellationToken::new();
    *controller.services.current_cancel.lock().unwrap() = Some(token.clone());
    assert!(!token.is_cancelled());

    let effects = controller
        .handle(intent(AppCommand::CancelRun))
        .expect("cancel run 不失败");
    assert!(token.is_cancelled(), "cancel run 必须取消当前 token");
    assert!(
        effects.iter().any(|e| matches!(e, AppEffect::Notify(_))),
        "cancel 应有用户反馈"
    );
}

/// Cancel run：无 run 在跑时不 panic（幂等）。
#[test]
fn cancel_run_idempotent_when_idle() {
    let mut controller = AppController::new(services());
    let effects = controller
        .handle(intent(AppCommand::CancelRun))
        .expect("idle cancel 不失败");
    assert!(!effects.is_empty());
}

/// Start new session：重置 conversation（log + history 清空）。
#[test]
fn start_new_session_resets_conversation() {
    let mut controller = AppController::new(services());
    // 制造一个已启动会话（模拟有历史）。
    controller
        .services
        .conversation
        .ensure_started(
            &controller.services.sessions_root,
            &controller.services.workspace_root,
        )
        .expect("ensure_started");
    let _ = controller
        .handle(intent(AppCommand::StartNewSession))
        .expect("new session 不失败");
    // reset 后 conversation 无 log（未启动）。
    assert!(
        controller.services.conversation.parts_for_run().is_err(),
        "reset 后 parts_for_run 应报未启动"
    );
}

/// Quit：请求渲染（surface 收到后退出）。
#[test]
fn quit_requests_draw() {
    let mut controller = AppController::new(services());
    let effects = controller
        .handle(intent(AppCommand::Quit))
        .expect("quit 不失败");
    assert!(
        effects.iter().any(|e| matches!(e, AppEffect::Draw)),
        "quit 应请求渲染"
    );
}

/// `ToggleSidebar` 是视图意图：controller 返回渲染 effect（不产生业务副作用）。
#[test]
fn toggle_sidebar_is_view_intent() {
    let mut controller = AppController::new(services());
    let effects = controller
        .handle(intent(AppCommand::ToggleSidebar))
        .expect("toggle 不失败");
    assert!(effects.iter().any(|e| matches!(e, AppEffect::Draw)));
}

// ---- P3-04：Platform effects adapter 验收（fake + 错误反馈）----

use tpi::app::effects::{PlatformEffects, apply_effect};

/// fake platform：可配置失败，记录调用。
#[derive(Default)]
struct FakePlatform {
    clipboard: std::cell::RefCell<Vec<String>>,
    urls: std::cell::RefCell<Vec<String>>,
    titles: std::cell::RefCell<Vec<String>>,
    fail_clipboard: bool,
    fail_url: bool,
}

impl PlatformEffects for FakePlatform {
    fn copy_to_clipboard(&self, text: &str) -> Result<(), String> {
        if self.fail_clipboard {
            return Err("injected clipboard failure".into());
        }
        self.clipboard.borrow_mut().push(text.to_string());
        Ok(())
    }
    fn open_url(&self, url: &str) -> Result<(), String> {
        if self.fail_url {
            return Err("injected url failure".into());
        }
        self.urls.borrow_mut().push(url.to_string());
        Ok(())
    }
    fn set_terminal_title(&self, title: &str) -> Result<(), String> {
        self.titles.borrow_mut().push(title.to_string());
        Ok(())
    }
    fn notify(&self, _message: &str) {}
}

/// `CopyToClipboard` 成功：调用 fake，错误不静默。
#[test]
fn copy_to_clipboard_success_and_error() {
    let fake = FakePlatform::default();
    assert!(apply_effect(&fake, &AppEffect::CopyToClipboard("hi".into())).is_ok());
    assert_eq!(fake.clipboard.borrow().as_slice(), ["hi"]);

    // 失败：错误反馈（不 let _ = 静默）。
    let failing = FakePlatform {
        fail_clipboard: true,
        ..Default::default()
    };
    let err = apply_effect(&failing, &AppEffect::CopyToClipboard("x".into()))
        .expect_err("clipboard 失败必须反馈");
    assert!(err.contains("injected"), "{err}");
}

/// OpenUrl：scheme 校验（非 http/https 拒绝）在 effects 层执行。
#[test]
fn open_url_rejects_non_http() {
    let fake = FakePlatform::default();
    let err = apply_effect(&fake, &AppEffect::OpenUrl("file:///etc/passwd".into()))
        .expect_err("非 http/https 必须拒绝");
    assert!(err.contains("http"), "{err}");
    assert!(fake.urls.borrow().is_empty(), "拒绝的 URL 不得执行");

    // 合法 URL 执行。
    assert!(apply_effect(&fake, &AppEffect::OpenUrl("https://example.com".into())).is_ok());
    assert_eq!(fake.urls.borrow().as_slice(), ["https://example.com"]);
}

/// `OpenUrl` 平台失败：错误反馈。
#[test]
fn open_url_platform_error_feedback() {
    let fake = FakePlatform {
        fail_url: true,
        ..Default::default()
    };
    let err = apply_effect(&fake, &AppEffect::OpenUrl("https://example.com".into()))
        .expect_err("平台失败必须反馈");
    assert!(err.contains("injected"), "{err}");
}

/// SetTerminalTitle：成功与反馈。
#[test]
fn terminal_title_sets_and_reports() {
    let fake = FakePlatform::default();
    assert!(apply_effect(&fake, &AppEffect::SetTerminalTitle("TPI".into())).is_ok());
    assert_eq!(fake.titles.borrow().as_slice(), ["TPI"]);
}

/// 未支持的 effect（OpenFilePicker）反馈错误，不静默。
#[test]
fn unsupported_effect_reports_error() {
    let fake = FakePlatform::default();
    let err = apply_effect(&fake, &AppEffect::OpenFilePicker { filter: None })
        .expect_err("未实现 effect 必须反馈");
    assert!(!err.is_empty());
}
