//! McpManager（README2 §6：MCP 生命周期；ToolRegistry 只拿 Tool）。
//!
//! 职责：config → spawn → initialize → tools/list → 注册 McpToolAdapter
//! 到 ToolRegistry；shutdown 时 terminate 所有子进程（不留孤儿，README2 §9）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Mutex;

use crate::tool::registry::ToolRegistry;

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
}

/// McpManager：管理所有 MCP server 生命周期（README2 §6）。
pub struct McpManager {
    servers: HashMap<String, RunningServer>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    /// 启动一个 MCP server 并注册其工具到 registry。
    pub async fn start_server(
        &mut self,
        config: McpServerConfig,
        registry: &mut ToolRegistry,
    ) -> Result<usize, McpError> {
        let mut client = McpClient::start(config).await?;
        let server_id = client.server_name().to_string();
        let mut tools = client.tools_list().await?;
        // 内部名：mcp::<server>::<tool>（README2 §5）；空工具名跳过。
        tools.retain(|tool| !tool.name.is_empty());

        let client = Arc::new(Mutex::new(client));
        let available = Arc::new(AtomicBool::new(true));

        let tool_count = tools.len();
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
            registry.register(Arc::new(adapter));
        }
        self.servers.insert(
            server_id.clone(),
            RunningServer {
                client,
                status: McpServerStatus::Connected { tool_count },
            },
        );
        tracing::info!(server = %server_id, count = tool_count, "MCP server initialized");
        Ok(tool_count)
    }

    /// 优雅关闭所有 server（README2 §9：terminate child，不留孤儿）。
    pub async fn shutdown_all(&mut self) {
        let servers = std::mem::take(&mut self.servers);
        for (_name, running) in servers {
            let mut guard = running.client.lock().await;
            guard.shutdown().await;
        }
    }

    /// 重启单个 server（Phase 3 /mcp restart；先卸载工具再启动）。
    pub async fn restart_server(
        &mut self,
        server_id: &str,
        registry: &mut ToolRegistry,
        configs: &[McpServerConfig],
    ) -> Result<usize, McpError> {
        // 卸载该 server 的工具（内部名 mcp::<server>::*）。
        let prefix = format!("mcp::{server_id}::");
        let names: Vec<String> = registry
            .list()
            .iter()
            .map(|t| t.name().to_string())
            .filter(|name| name.starts_with(&prefix))
            .collect();
        for name in names {
            registry.unregister(&name);
        }
        // 关掉旧 client。
        if let Some(running) = self.servers.remove(server_id) {
            let mut guard = running.client.lock().await;
            guard.kill().await;
        }
        // 重新启动。
        let Some(config) = configs.iter().find(|c| c.name == server_id).cloned() else {
            return Err(McpError::Protocol(format!("server {server_id} 未配置")));
        };
        self.start_server(config, registry).await
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
