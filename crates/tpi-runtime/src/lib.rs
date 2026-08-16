//! TPI 应用运行时（web_desktop.md §七 / Phase 2）。
//!
//! 这是所有前端的**唯一业务入口**。前端只能通过：
//!
//! ```text
//! RuntimeHandle.send(command) → CommandAck
//! RuntimeHandle.subscribe()     → EventEnvelope stream
//! ```
//!
//! 与 runtime 交互，绝不直接调用 AgentLoop / ToolExecutor / SessionStore /
//! Provider。所有 UI 都只是 Command producer + Event consumer。
//!
//! ## 职责
//!
//! - **会话管理**：创建 / 列出 / 恢复会话（基于 `tpi-session` durable store）。
//! - **命令串行化**：`ClientCommand` 经单消费队列处理，保证 one-authoritative-runtime。
//! - **事件广播**：`RuntimeEvent` 封装为 `EventEnvelope` 后广播给所有订阅者，
//!   全局 `seq` 单调递增，支持断线重连（`after_seq` 回放）。
//! - **Run 编排**：调用 `tpi-agent::run`，把 `LiveEvent` 转写为协议事件，
//!   管理 `request_input` 挂起/恢复生命周期。
//!
//! ## 类型参数
//!
//! `P: Provider`：provider 类型（trait 非 object-safe，故保留泛型；与
//! `AppServices<P>` 先例一致，前端实例化具体类型）。

pub mod handle;
pub mod service;

pub use handle::RuntimeHandle;
pub use service::{Emitter, RuntimeTask, SessionRuntime};

/// 事件广播容量（高频流式场景下的暂存深度；超出后新订阅者需要从 seq 回放）。
pub const EVENT_BROADCAST_CAPACITY: usize = 1024;
