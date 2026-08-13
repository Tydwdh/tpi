//! TPI：面向 Windows 的个人终端 coding agent。
//!
//! 核心边界是 agent loop、provider adapter、durable session、内置工具和 TUI；
//! 每个模块的所有权与验证入口见仓库 README。

pub mod agent;
pub mod app;
pub mod auth;
pub mod clipboard;
pub mod config;
pub mod context;
pub mod doctor;
pub mod eval;
pub mod ids;
pub mod mcp;
pub mod process;
pub mod provider;
pub mod remote;
pub mod session;
pub mod shell;
pub mod tool;
pub mod tui;
pub mod util;
pub mod workspace;
