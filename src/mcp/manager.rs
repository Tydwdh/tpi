//! McpManager（README2 §6：MCP 生命周期；ToolRegistry 只拿 Tool）。
//!
//! 职责：config → spawn → initialize → tools/list → 注册 McpToolAdapter
//! 到 ToolRegistry；shutdown 时 terminate 所有子进程（不留孤儿，README2 §9）。
//!
//! 生命周期（AGENTS.md §11/§12）：每个 server 的工具注册返回 RAII 句柄
//! （[`crate::tool::registry::ToolRegistration`]），由 RunningServer 持有；
//! server 重启/关闭时随 drop 自动注销——不再按名字前缀扫描 registry。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Mutex;

use crate::tool::registry::{ToolRegistration, ToolRegistry};

use super::adapter::McpToolAdapter;
use super::client::McpClient;
use super::config::McpServerConfig;
use super::error::McpError;

/// 一个 MCP server 的运行状态（Phase 3 /mcp 页面用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerStatus {
    Connected { tool_count: usize },
    Failed(String),
    Stopped,
}

/// MCP server 运行时句柄。
struct RunningServer {
    client: Arc<Mutex<McpClient>>,
    status: McpServerStatus,
    /// RAII 注册句柄：server 关闭/重启时随 drop 自动从 registry 注销工具
    /// （谁注册谁清理；不再按 `mcp::<server>::*` 前缀扫描）。
    _registrations: Vec<ToolRegistration>,
}

/// McpManager：管理所有 MCP server 生命周期（README2 §6）。
pub struct McpManager {
    servers: HashMap<String, RunningServer>,
    /// 工具注册目标（RAII 句柄持有的 registry；默认进程级共享目录）。
    registry: Arc<std::sync::Mutex<ToolRegistry>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::with_registry(crate::tool::registry::global_registry())
    }

    /// 注入自定义 registry（测试用：隔离的本地 registry）。
    pub fn with_registry(registry: Arc<std::sync::Mutex<ToolRegistry>>) -> Self {
        Self {
            servers: HashMap::new(),
            registry,
        }
    }

    /// 启动一个 MCP server 并注册其工具到 registry。
    pub async fn start_server(&mut self, config: McpServerConfig) -> Result<usize, McpError> {
        let mut client = McpClient::start(config).await?;
        let server_id = client.server_name().to_string();
        let mut tools = client.tools_list().await?;
        // 内部名：mcp::<server>::<tool>（README2 §5）；空工具名跳过。
        tools.retain(|tool| !tool.name.is_empty());

        let client = Arc::new(Mutex::new(client));
        let available = Arc::new(AtomicBool::new(true));

        let tool_count = tools.len();
        let mut registrations = Vec::with_capacity(tool_count);
        for tool in tools {
            let internal = format!("mcp::{server_id}::{}", tool.name);
            let adapter = McpToolAdapter::new(
                server_id.clone(),
                tool.name,
                internal,
                tool.description.clone(),
                tool.input_schema.clone(),
                client.clone(),
                available.clone(),
            );
            registrations.push(
                ToolRegistry::register_owned(&self.registry, Arc::new(adapter))
                    .map_err(McpError::Protocol)?,
            );
        }
        self.servers.insert(
            server_id.clone(),
            RunningServer {
                client,
                status: McpServerStatus::Connected { tool_count },
                _registrations: registrations,
            },
        );
        tracing::info!(server = %server_id, count = tool_count, "MCP server initialized");
        Ok(tool_count)
    }

    /// 优雅关闭所有 server（README2 §9：terminate child，不留孤儿）。
    /// drop RunningServer 同时注销其全部工具（RAII）。
    pub async fn shutdown_all(&mut self) {
        let servers = std::mem::take(&mut self.servers);
        for (_name, running) in servers {
            let mut guard = running.client.lock().await;
            guard.shutdown().await;
            // running drop → _registrations 自动注销。
        }
    }

    /// 重启单个 server（Phase 3 /mcp restart）：drop 旧 RunningServer
    /// （工具自动注销 + client 终止），再按配置重新启动。
    pub async fn restart_server(
        &mut self,
        server_id: &str,
        configs: &[McpServerConfig],
    ) -> Result<usize, McpError> {
        if let Some(running) = self.servers.remove(server_id) {
            let client = running.client.clone();
            let mut guard = client.lock().await;
            guard.kill().await;
            // running drop → _registrations 自动注销（无需前缀扫描）。
        }
        let Some(config) = configs.iter().find(|c| c.name == server_id).cloned() else {
            return Err(McpError::Protocol(format!("server {server_id} 未配置")));
        };
        self.start_server(config).await
    }

    /// 各 server 状态（Phase 3 /mcp 页面）。
    pub fn statuses(&self) -> Vec<(String, McpServerStatus)> {
        let mut out: Vec<(String, McpServerStatus)> = self
            .servers
            .iter()
            .map(|(name, running)| (name.clone(), running.status.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    /// 从 `~/.tpi/config.toml` 加载并启动所有 enabled MCP servers（Phase 3）。
    /// 注册的工具并入**进程级共享 registry**（ToolRuntime 读取 → agent loop
    /// 可调用，README2 Phase 5）。
    pub async fn start_from_config(&mut self) -> usize {
        let home = crate::config::tpi_home();
        let configs = crate::mcp::config::load_enabled(&home);
        let mut started = 0usize;
        for config in configs {
            match self.start_server(config).await {
                Ok(_) => started += 1,
                Err(error) => {
                    tracing::warn!(error = %error, "MCP server 启动失败");
                }
            }
        }
        started
    }
}
