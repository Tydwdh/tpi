//! 键位映射（P7-02 拆 crate：定义移入 tpi-ui-types，此处 re-export 保持
//! `tui::keymap::*` 路径兼容；reducer/config 引用不变）。

pub use tpi_ui_types::keymap::{KeyAction, Keymap, format_key, parse_key};
