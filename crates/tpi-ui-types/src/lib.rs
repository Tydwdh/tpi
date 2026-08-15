//! TPI UI 数据类型（P7-02 拆 crate：config 与 TUI 共享，消除 config→tui 依赖）。
//!
//! - [`view_mode`]：视口模式（fullscreen/inline）；
//! - [`keymap`]：键位映射（KeyAction/Keymap/parse/format）。

pub mod keymap;
pub mod view_mode;

pub use keymap::{KeyAction, Keymap, format_key, parse_key};
pub use view_mode::ViewMode;
