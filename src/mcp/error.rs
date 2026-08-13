//! MCP 错误模型（README2 §11：统一转换为模型可理解的结果）。
//!
//! 完整错误进 debug log；模型看到简化结果（ToolStatus + 简洁文本）。

use std::fmt;

/// MCP 层错误（映射到 ToolOutcome 前使用）。
#[derive(Debug, Clone)]
pub enum McpError {
    /// 进程无法启动 / 已退出。
    ProcessDied(String),
    /// 传输层失败（stdin 写失败 / stdout 关闭 / EOF）。
    Transport(String),
    /// 协议错误（非法 JSON-RPC / 缺字段 / 无法解析）。
    Protocol(String),
    /// 请求超时。
    Timeout,
    /// server 返回 JSON-RPC error（code/message）。
    ServerError { code: i64, message: String },
    /// 工具参数与 schema 不符（client 侧校验）。
    InvalidSchema(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::ProcessDied(detail) => write!(f, "MCP server 进程异常: {detail}"),
            McpError::Transport(detail) => write!(f, "MCP 传输失败: {detail}"),
            McpError::Protocol(detail) => write!(f, "MCP 协议错误: {detail}"),
            McpError::Timeout => write!(f, "MCP 调用超时"),
            McpError::ServerError { code, message } => {
                write!(f, "MCP server 错误 ({code}): {message}")
            }
            McpError::InvalidSchema(detail) => write!(f, "MCP 参数无效: {detail}"),
        }
    }
}

/// 映射到模型可见的 ToolStatus（README2 §11）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpErrorKind {
    InvalidArguments,
    Timeout,
    ExecutionFailed,
    Unavailable,
}

impl McpError {
    pub fn kind(&self) -> McpErrorKind {
        match self {
            McpError::InvalidSchema(_) => McpErrorKind::InvalidArguments,
            McpError::Timeout => McpErrorKind::Timeout,
            McpError::ServerError { .. } => McpErrorKind::ExecutionFailed,
            McpError::ProcessDied(_) | McpError::Transport(_) | McpError::Protocol(_) => {
                McpErrorKind::Unavailable
            }
        }
    }
}

/// 简洁错误码（进 ToolOutcome output 文本）。
pub fn error_code(error: &McpError) -> &'static str {
    match error.kind() {
        McpErrorKind::InvalidArguments => "mcp_invalid_arguments",
        McpErrorKind::Timeout => "mcp_timeout",
        McpErrorKind::ExecutionFailed => "mcp_execution_failed",
        McpErrorKind::Unavailable => "mcp_unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kinds_map_correctly() {
        assert_eq!(McpError::InvalidSchema("x".into()).kind(), McpErrorKind::InvalidArguments);
        assert_eq!(McpError::Timeout.kind(), McpErrorKind::Timeout);
        assert_eq!(
            McpError::ServerError { code: -32602, message: "bad".into() }.kind(),
            McpErrorKind::ExecutionFailed
        );
        assert_eq!(McpError::ProcessDied("crash".into()).kind(), McpErrorKind::Unavailable);
        assert_eq!(McpError::Transport("eof".into()).kind(), McpErrorKind::Unavailable);
        assert_eq!(McpError::Protocol("bad json".into()).kind(), McpErrorKind::Unavailable);
    }

    #[test]
    fn error_codes_are_short_and_stable() {
        assert_eq!(error_code(&McpError::Timeout), "mcp_timeout");
        assert_eq!(error_code(&McpError::InvalidSchema("x".into())), "mcp_invalid_arguments");
    }
}
