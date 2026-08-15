//! TPI：面向 Windows 的个人终端 coding agent。
//!
//! 核心边界是 agent loop、provider adapter、durable session、内置工具和 TUI；
//! 每个模块的所有权与验证入口见仓库 README。
//!
//! P7-02 拆 crate：core 层（ids/message/plan/outcome/util）已拆为 `tpi-core`
//! crate；此处 re-export 保持 `tpi::ids` 等路径兼容（逐步迁移引用后再移除）。

pub use tpi_core::{ids, message, outcome, plan, revision, util};
// session crate 顶层即 session 模块（module alias 保持 `crate::session` 路径）。
pub use tpi_session as session;

pub mod agent;
pub mod app;
pub mod auth;
pub mod clipboard;
pub mod config;
pub mod context;
pub mod doctor;
pub mod eval;
pub mod mcp;
pub mod process;
pub mod provider;
pub mod remote;
pub mod shell;
pub mod skills;
pub mod subagent;
pub mod tool;
pub mod trace;
pub mod tui;
pub mod web;
pub mod workspace;
