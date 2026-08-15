//! TPI core：纯数据与纯工具层（P7-02 拆 crate 的第一个 crate）。
//!
//! 只包含无内部副作用依赖的模块：标识符（`ids`）、domain 消息（`message`）、
//! 计划数据（`plan`）、工具结果协议（`outcome`）、通用工具（`util`）。
//! 不依赖 session/capabilities/agent/TUI——是依赖 DAG 的最底层。

pub mod ids;
pub mod message;
pub mod outcome;
pub mod plan;
pub mod revision;
pub mod util;
