//! Integration-test fixtures shared by the contract suites.
//!
//! integration tests 通过 `mod fixtures;` 共享本目录。
//! 夹具函数由不同测试文件按需使用，允许单文件编译单元内未引用。
#![expect(
    dead_code,
    reason = "each integration-test crate consumes a different fixture subset"
)]

pub mod fake_provider;
pub mod remote_server;

use camino::Utf8PathBuf;
use tpi::config::{Config, LimitsConfig, ModelConfig};

/// 构造一个最小测试配置（model 名固定，避免"看不见的默认模型"）。
pub fn test_config(workspace_root: &Utf8PathBuf) -> Config {
    Config {
        model: ModelConfig {
            provider: "test".into(),
            name: "fake-model".into(),
            base_url: "https://example.invalid/v1".into(),
            reasoning: None,
            max_output_tokens: None,
            context_window: None,
            api_key_env: "TPI_TEST_API_KEY".into(),
            api_key: None,
            price_input: None,
            price_output: None,
        },
        models: Vec::new(),
        limits: LimitsConfig::default(),
        workspace_root: workspace_root.clone(),
        sessions_root: workspace_root.join(".tpi-test-sessions").into(),
        artifacts_root: workspace_root.join(".tpi-test-artifacts").into(),
        shell_path: None,
        safety_reserve_tokens: 8192,
        ui_mode: Default::default(),
        ui_keymap: tpi::tui::keymap::Keymap::builtin(),
        ui_collapsed_lines: 10,
        auto_open_browser: false,
        web_summary_model: "none".into(),
        system_prompt_extra: None,
        source: "test".into(),
        ui_theme: "omp".into(),
        allow_outside_workspace: true,
    }
}

/// 测试进程不是 tpi.exe；必须显式指向真实 host 二进制（§11.5 单二进制 handshake）。
pub fn point_host_at_real_tpi() {
    // SAFETY: TPI targets Windows, whose process environment is synchronized by
    // the OS. Every test writer uses the same immutable executable path.
    unsafe {
        std::env::set_var("TPI_PROCESS_HOST", env!("CARGO_BIN_EXE_tpi"));
    }
}

/// 构造最小工具执行上下文（M2 起字段齐备）。
pub fn test_tool_context(workspace_root: &Utf8PathBuf) -> tpi::tool::ToolContext {
    use std::sync::{Arc, Mutex};
    use tpi::tool::search::ScanSnapshot;
    // §W0：LocalWorkspace 拥有 shell；ctx.shell 与 ctx.workspace 共享同一 Arc。
    let local = tpi::workspace::LocalWorkspace::new(workspace_root.clone(), true);
    let workspace = Arc::new(Mutex::new(tpi::workspace::ActiveWorkspace::local(
        local.clone(),
    )));
    tpi::tool::ToolContext {
        workspace_root: workspace_root.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        artifacts_root: workspace_root.join(".tpi-test-artifacts").into(),
        session_id: "test-session".into(),
        call_id: tpi::ids::ToolCallId::new_v7(),
        output_tx: None,
        scan_snapshots: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            ScanSnapshot,
        >::new())),
        shell_path: None,
        snapshot_store: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::tool::edit::SnapshotStore::default(),
        )),
        current_plan: std::sync::Arc::new(std::sync::Mutex::new(None)),
        shell: local.shell.clone(),
        workspace,
        processes: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::process::managed::ProcessRegistry::new(),
        )),
        registry: std::sync::Arc::new(std::sync::Mutex::new(
            tpi::tool::registry::builtin_registry(),
        )),
        interactive: true,
        allow_outside_workspace: true,
    }
}
