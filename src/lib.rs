//! TPI：个人终端 Coding Agent（实现契约见 TPI_DESIGN.md）。
//!
//! M1：Walking Skeleton——CLI/配置、SSE provider + fake、read/edit/write/run、
//! 串行 tool-call loop、append-only session、write-ahead 与恢复、Ctrl-C 取消。

pub mod agent;
pub mod app;
pub mod auth;
pub mod clipboard;
pub mod config;
pub mod context;
pub mod doctor;
pub mod eval;
pub mod ids;
pub mod process;
pub mod provider;
pub mod session;
pub mod tool;
pub mod tui;
pub mod util;
