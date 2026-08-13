//! MCP Client（README2 §7：stdio/initialize/tools-list/tools-call + 生命周期）。
//!
//! 边界：MCP 生命周期（config/spawn/initialize/capability/tools/call/reconnect/
//! shutdown）都在本模块；ToolRegistry 只拿到已适配的 [`crate::tool::registry::Tool`]
//!（McpToolAdapter），Agent Loop 不知道 MCP 细节（README2 §6/§28）。

pub mod client;
pub mod config;
pub mod error;
pub mod manager;
pub mod adapter;
