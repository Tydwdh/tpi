//! MCP stdio client（README2 §7/§9/§12）。
//!
//! MCP stdio transport：spawn server 进程，stdin/stdout 传
//! **newline-delimited JSON-RPC 2.0**（每行一个 JSON 消息）。
//!
//! 生命周期：spawn → initialize → initialized 通知 → tools/list →
//! tools/call（可多次）→ shutdown。server 崩溃/管道断 → 明确错误，
//! 不做复杂 supervisor（README2 §12）。

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::config::McpServerConfig;
use super::error::McpError;

/// MCP 协议版本（2024-11-05 稳定版）。
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// 一个 MCP server 暴露的工具描述。
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    /// JSON Schema（inputSchema）。
    pub input_schema: serde_json::Value,
}

/// initialize 结果（capability 协商，README2 §9）。
#[derive(Debug, Clone)]
pub struct InitializeResult {
    pub server_name: String,
    pub server_version: String,
    pub protocol_version: String,
}

/// MCP stdio client（单 server）。
pub struct McpClient {
    config: McpServerConfig,
    child: Child,
    stdin: ChildStdin,
    id: AtomicU64,
    reader_rx: UnboundedReceiver<String>,
    init: Option<InitializeResult>,
    tools: Vec<McpToolInfo>,
    /// P2-07：stderr/stdout reader 任务的 owner（shutdown/kill 时 join，
    /// 不留下无主的 reader task）。
    reader_supervisor: crate::process::supervisor::Supervisor,
}

impl Drop for McpClient {
    /// 兜底：任何未走 `shutdown()`/`kill()` 的退出路径（start 握手失败、
    /// restart 原子发布、TPI 崩溃/panic）都必须杀掉子进程，否则 tokio
    /// `Child`（kill_on_drop=false）会把 MCP server 变成孤儿进程，且
    /// stdout/stderr reader task 因管道未 EOF 永久阻塞（F1/F3）。
    ///
    /// `start_kill` 是同步的（Tokio 支持），Drop 中可安全调用；子进程死
    /// 后管道 EOF，reader task 自然退出。已 kill/wait 的进程再次 start_kill
    /// 返回错误，忽略即可。
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl McpClient {
    /// 启动进程并完成 initialize + initialized 通知。
    pub async fn start(config: McpServerConfig) -> Result<Self, McpError> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &config.env {
            command.env(k, v);
        }
        #[cfg(windows)]
        {
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|e| McpError::ProcessDied(format!("spawn {} 失败: {e}", config.command)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::ProcessDied(format!("{} 无 stdin", config.command)))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::ProcessDied(format!("{} 无 stdout", config.command)))?;
        let stderr = child.stderr.take();

        // stderr → tracing（完整错误进 debug log，README2 §11）。
        // P2-07：reader 由 Supervisor 跟踪（shutdown 时 join）。
        let mut reader_supervisor = crate::process::supervisor::Supervisor::new();
        if let Some(stderr) = stderr {
            let name = config.name.clone();
            reader_supervisor.spawn("mcp.stderr_reader", move |_| async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(server = %name, line = %line, "MCP server stderr");
                }
            });
        }

        // stdout 后台读取循环 → 逐行发到 channel。
        let (tx, rx): (UnboundedSender<String>, UnboundedReceiver<String>) =
            tokio::sync::mpsc::unbounded_channel();
        let stdout_tx = tx.clone();
        reader_supervisor.spawn("mcp.stdout_reader", move |_| async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if stdout_tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut client = Self {
            config,
            child,
            stdin,
            id: AtomicU64::new(1),
            reader_rx: rx,
            init: None,
            tools: Vec::new(),
            reader_supervisor,
        };
        client.initialize().await?;
        client.notify_initialized().await?;
        Ok(client)
    }

    /// 发送 JSON-RPC 请求并等待对应 id 的响应（跳过通知/不匹配行；带 timeout）。
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let mut msg = serde_json::Map::new();
        msg.insert("jsonrpc".into(), serde_json::json!("2.0"));
        msg.insert("id".into(), serde_json::json!(id));
        msg.insert("method".into(), serde_json::json!(method));
        if let Some(params) = params {
            msg.insert("params".into(), params);
        }
        self.write_line(&serde_json::Value::Object(msg)).await?;

        let deadline = tokio::time::Instant::now() + self.config.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(McpError::Timeout);
            }
            let line = tokio::time::timeout(remaining, self.reader_rx.recv())
                .await
                .map_err(|_| McpError::Timeout)?
                .ok_or_else(|| {
                    McpError::Transport(format!("stdout 关闭（{} 可能已退出）", self.config.name))
                })?;
            let parsed: serde_json::Value = serde_json::from_str(&line)
                .map_err(|e| McpError::Protocol(format!("非法 JSON: {e}")))?;
            let Some(response_id) = parsed.get("id").and_then(|v| v.as_u64()) else {
                // 无 id：server 主动通知，跳过。
                continue;
            };
            if response_id != id {
                continue; // 其他请求的响应（不应发生，防御）
            }
            // 匹配的响应：error 或 result。
            if let Some(error) = parsed.get("error") {
                let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                return Err(McpError::ServerError { code, message });
            }
            return parsed
                .get("result")
                .cloned()
                .ok_or_else(|| McpError::Protocol("响应缺 result".into()));
        }
    }

    /// initialize：发送请求 + 读响应（带 timeout）。
    async fn initialize(&mut self) -> Result<(), McpError> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "tpi", "version": env!("CARGO_PKG_VERSION") },
        });
        let result = self.request("initialize", Some(params)).await?;
        let server_name = result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let server_version = result
            .get("serverInfo")
            .and_then(|s| s.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let protocol_version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.init = Some(InitializeResult {
            server_name,
            server_version,
            protocol_version,
        });
        Ok(())
    }

    /// initialized 通知（无响应）。
    async fn notify_initialized(&mut self) -> Result<(), McpError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        });
        self.write_line(&msg).await
    }

    /// tools/list：发现 server 暴露的工具。
    pub async fn tools_list(&mut self) -> Result<Vec<McpToolInfo>, McpError> {
        let result = self.request("tools/list", None).await?;
        let tools: Vec<McpToolInfo> = result
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|tool| McpToolInfo {
                        name: tool
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        description: tool
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        input_schema: tool
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.tools = tools.clone();
        Ok(tools)
    }

    /// 工具调用（tools/call）。
    pub async fn call_tool(
        &mut self,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let params = serde_json::json!({
            "name": tool,
            "arguments": arguments,
        });
        self.request("tools/call", Some(params)).await
    }

    pub fn init_result(&self) -> Option<&InitializeResult> {
        self.init.as_ref()
    }

    pub fn tools(&self) -> &[McpToolInfo] {
        &self.tools
    }

    /// 子进程 pid（lifecycle 测试用）。
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn server_name(&self) -> &str {
        &self.config.name
    }

    /// 优雅关闭：shutdown 请求 + 终止进程 + join reader（README2 §9：
    /// 不留孤儿进程；P2-07：不留无主 reader task）。
    pub async fn shutdown(&mut self) {
        let _ = self.request("shutdown", None).await;
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        let _ = self.reader_supervisor.shutdown().await;
    }

    /// 强制终止（server 卡死/异常时）+ join reader。
    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        let _ = self.reader_supervisor.shutdown().await;
    }

    /// P2-07 验收：当前被跟踪的 reader task 数（shutdown 后应为 0）。
    pub fn reader_tracked(&self) -> usize {
        self.reader_supervisor.tracked()
    }

    /// 写一行 JSON 到 stdin（newline-delimited：每消息一行）。
    async fn write_line(&mut self, msg: &serde_json::Value) -> Result<(), McpError> {
        let line = serde_json::to_string(msg)
            .map_err(|e| McpError::Protocol(format!("序列化失败: {e}")))?;
        let transport_err = |e: std::io::Error| {
            McpError::Transport(format!("写入 stdin 失败（server 可能已退出）: {e}"))
        };
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(transport_err)?;
        self.stdin.write_all(b"\n").await.map_err(transport_err)?;
        self.stdin.flush().await.map_err(transport_err)
    }
}
