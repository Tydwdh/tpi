//! TPI 多端协议（web_desktop.md Phase 1）。
//!
//! 这是整个多端架构最重要的新边界。它**只定义**：
//!
//! ```text
//! ClientCommand    —— 前端 → Runtime 的命令（唯一修改入口）
//! RuntimeEvent     —— Runtime → 前端的语义事件（UI 状态由 Snapshot + Event 流构造）
//! View DTO         —— 查询视图（SessionView 等，Anti-Corruption Layer）
//! Structured Error —— 结构化错误（code/message/retryable/details）
//! EventEnvelope    —— 带稳定身份的事件信封（session_id/run_id/seq/timestamp）
//! ProtocolVersion  —— 协议版本 + 握手消息
//! ```
//!
//! ## 约束（web_desktop.md §三）
//!
//! - 全部类型 `Serialize + Deserialize + Clone + Debug`（serde）。
//! - **禁止依赖任何 UI / Transport**：ratatui / crossterm / tauri / axum / warp /
//!   actix / tokio-tungstenite / React / Web-specific types 一律不得出现。
//!   协议不知道自己最终通过 Rust channel / WebSocket / HTTP / IPC 传输。
//! - 协议类型不暴露 Rust 内部 struct（Anti-Corruption Layer，§二十六）。
//! - 唯一允许的依赖是 `tpi-core`（纯数据层：ids/message/plan/outcome）。
//!
//! ## Invariants（§三十九）
//!
//! 6. Protocol types do not depend on any UI framework.
//! 16. Protocol is versioned from day one.

pub mod command;
pub mod envelope;
pub mod error;
pub mod event;
pub mod version;
pub mod view;

pub use command::{AckStatus, ClientCommand, CommandAck};
pub use envelope::EventEnvelope;
pub use error::{AppError, ErrorCode};
pub use event::{CompletionReasonDto, DeltaKind, RuntimeEvent, ToolState};
pub use version::{ClientHello, PROTOCOL_VERSION, ProtocolVersion, ServerHello};
pub use view::{
    ChatMessageDto, MessageRoleDto, QuestionDto, QuestionOptionDto, SessionStatus, SessionView,
};
