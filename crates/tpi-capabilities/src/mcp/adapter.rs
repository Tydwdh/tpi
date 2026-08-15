//! MCP Tool Adapter（README2 §10：MCP 工具包装成普通 Tool）。
//!
//! Agent Runtime 不知道 MCP request/JSON-RPC/进程/stdio——只看到 Tool。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex;

use crate::tool::ToolContext;
use crate::tool::registry::{Tool, ToolOrigin};
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};

use super::client::McpClient;
use super::error::error_code;

/// MCP 工具适配器（README2 §10）。
pub struct McpToolAdapter {
    server_id: String,
    /// server 侧原始工具名（tools/call 用）。
    raw_tool_name: String,
    /// 内部唯一名 `mcp::<server>::<tool>`（README2 §5）。
    internal_name: String,
    description: String,
    schema: serde_json::Value,
    /// 共享 client（同一 server 的所有工具共用连接；用 Arc<Mutex> 串行调用）。
    client: Arc<Mutex<McpClient>>,
    /// server 是否可用（崩溃后置 false，工具立即报 Unavailable）。
    available: Arc<AtomicBool>,
}

impl McpToolAdapter {
    pub fn new(
        server_id: String,
        raw_tool_name: String,
        internal_name: String,
        description: String,
        schema: serde_json::Value,
        client: Arc<Mutex<McpClient>>,
        available: Arc<AtomicBool>,
    ) -> Self {
        Self {
            server_id,
            raw_tool_name,
            internal_name,
            description,
            schema,
            client,
            available,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        // 内部唯一名：mcp::<server>::<tool>（README2 §5）。
        &self.internal_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Mcp {
            server: self.server_id.clone(),
        }
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolOutcome {
        if !self.available.load(Ordering::SeqCst) {
            return mcp_outcome(
                "mcp_unavailable",
                format!("MCP server '{}' 不可用（已崩溃/退出）。", self.server_id),
            );
        }
        // 参数：args 是 JSON 字符串（工具 schema 的输入）。
        let arguments: serde_json::Value = match serde_json::from_str(args) {
            Ok(value) => value,
            Err(e) => {
                return mcp_outcome(
                    "mcp_invalid_arguments",
                    format!("工具参数不是合法 JSON: {e}"),
                );
            }
        };
        let start = std::time::Instant::now();
        let result = {
            let mut client = self.client.lock().await;
            client.call_tool(&self.raw_tool_name, arguments).await
        };
        match result {
            Ok(value) => {
                let output = format_mcp_result(&value);
                let mut outcome = ToolOutcome::succeeded(&self.internal_name, output);
                outcome.session_metadata = tpi_core::outcome::ToolMetadata {
                    tool: self.internal_name.clone(),
                    ..Default::default()
                };
                // §13：MCP 调用可观测性（server/tool/duration/status）。
                tracing::debug!(
                    server = %self.server_id,
                    tool = %self.raw_tool_name,
                    duration_ms = start.elapsed().as_millis(),
                    status = "ok",
                    "MCP tool call",
                );
                outcome
            }
            Err(error) => {
                tracing::debug!(
                    server = %self.server_id,
                    tool = %self.raw_tool_name,
                    duration_ms = start.elapsed().as_millis(),
                    status = "error",
                    error = %error,
                    "MCP tool call",
                );
                // server 进程层错误 → 标记不可用（README2 §12：server died →
                // mark unavailable）。
                if matches!(error.kind(), super::error::McpErrorKind::Unavailable) {
                    self.available.store(false, Ordering::SeqCst);
                }
                mcp_outcome(error_code(&error), format!("{error}"))
            }
        }
    }
}

/// 把 MCP tools/call 的 result 转成模型可读文本（README2 §11：简化结果）。
///
/// MCP result 形如 `{"content":[{"type":"text","text":"..."}], "isError": false}`。
fn format_mcp_result(result: &serde_json::Value) -> String {
    // 1. content 数组 → 拼接 text。
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for item in content {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            } else if let Some(blob) = item.get("blob") {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "[binary {} bytes]",
                    blob.as_str().map(|s| s.len()).unwrap_or(0)
                ));
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    // 2. 结构化 result → 紧凑 JSON。
    serde_json::to_string(result).unwrap_or_else(|_| "(空结果)".into())
}

fn mcp_outcome(error_code: &str, detail: String) -> ToolOutcome {
    ToolOutcome::failed(
        "mcp",
        ModelPayload {
            status: ToolStatus::Failed,
            program: Some("mcp".into()),
            exit_code: None,
            duration_ms: 0,
            output: format!("status: failed\ntool: mcp\nerror: {error_code}\n\n{detail}"),
            effect: None,
            artifact: None,
        },
    )
}
