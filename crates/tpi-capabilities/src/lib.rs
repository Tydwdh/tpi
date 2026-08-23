//! TPI capabilities 层（P7-02 拆 crate）。
//!
//! 工具能力：tool registry/pipeline/scheduler、shell session、workspace 端口、
//! 托管进程、MCP 生命周期。依赖 core（纯数据）与 session（durable 存储）。

pub mod mcp;
pub mod process;
pub mod remote;
pub mod resource;
pub mod shell;
pub mod skills;
pub mod terminal;
pub mod tool;
pub mod workspace;
