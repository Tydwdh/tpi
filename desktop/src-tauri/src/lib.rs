//! TPI Desktop 应用库（Tauri 2 要求 lib target）。
//!
//! 复用 Web frontend + embedded TPI server（同一 Application Protocol）。

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tauri::{Manager, WindowEvent};

use tpi_agent::provider::openai_compat::OpenAiCompatClient;
use tpi_config::config::{Config, ModelConfig};
use tpi_runtime::RuntimeTask;
use tpi_server::embedded::{EmbeddedServer, spawn_embedded};

/// 构建 Tauri 应用（main.rs 调用）。
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let config = match load_config() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("TPI 配置加载失败: {e}\n请先运行 `tpi init` 或编辑 ~/.tpi/config.toml");
                    return Err(Box::new(std::io::Error::other(e)));
                }
            };
            let registry: Arc<
                StdMutex<tpi_capabilities::tool::registry::ToolRegistry>,
            > = Arc::new(StdMutex::new(
                tpi_capabilities::tool::registry::builtin_registry(),
            ));
            let build_provider: Box<
                dyn FnMut(&ModelConfig) -> Result<OpenAiCompatClient, String> + Send,
            > = Box::new(|model: &ModelConfig| {
                let api_key = tpi_config::config::read_api_key_for(model)?;
                Ok(OpenAiCompatClient::new(
                    model.base_url.clone(),
                    model.name.clone(),
                    api_key,
                    model.reasoning.clone(),
                    model.max_output_tokens,
                    model.context_window,
                ))
            });

            let task = RuntimeTask::new(Arc::new(config), build_provider, registry);

            // WebView 从 embedded server 加载同一份 Web UI（同源 + token 注入），
            // 完全复用 Web 端协议路径。web_dist 指向构建好的前端产物。
            let web_dist = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../apps/web/dist");
            let web_dist = if web_dist.is_dir() {
                Some(web_dist)
            } else {
                eprintln!("警告: 未找到 Web 前端构建产物 {}（先运行 `cd apps/web && npm run build`）", web_dist.display());
                None
            };

            // 异步启动 embedded server（tauri async runtime）。
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match spawn_embedded(task, web_dist).await {
                    Ok(server) => {
                        app_handle.manage(server);
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let server = app_handle.state::<EmbeddedServer>();
                            let url = tpi_server::embedded::webview_url(server.inner());
                            let _ = window.navigate(url.parse().unwrap());
                        }
                    }
                    Err(e) => {
                        eprintln!("embedded server 启动失败: {e}");
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Destroyed = event {
                let app = window.app_handle();
                if let Some(server) = app.try_state::<EmbeddedServer>() {
                    let shutdown = server.shutdown.clone();
                    tauri::async_runtime::spawn(async move {
                        shutdown.cancel();
                    });
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("TPI Desktop 运行失败");
}

/// 加载 TPI 配置（与 `tpi` CLI 同一事实源：~/.tpi/config.toml）。
fn load_config() -> Result<Config, String> {
    let workspace_root = std::env::current_dir().unwrap_or_default();
    let workspace = camino::Utf8PathBuf::from_path_buf(workspace_root)
        .unwrap_or_else(|_| camino::Utf8PathBuf::from("."));
    tpi_config::config::load(&workspace, None)
}
