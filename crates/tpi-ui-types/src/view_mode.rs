//! 视口模式（§1：默认 fullscreen；inline 仅为兼容模式）。

/// 视口模式（P7-02 拆 crate：从 tui::terminal 下沉，config 与 TUI 共享）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Fullscreen,
    Inline,
}

impl ViewMode {
    /// 从配置字符串解析（`[ui] mode`）；未知值回退 fullscreen。
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "inline" => ViewMode::Inline,
            _ => ViewMode::Fullscreen,
        }
    }
}
