//! OMP 语义主题（§16.3、§21 M5：OMP semantic theme）。
//!
//! 语义 token 与颜色分离：组件引用语义角色，主题决定具体颜色。

use ratatui::style::Color;

/// 语义主题。
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub user: Color,
    pub assistant: Color,
    pub reasoning: Color,
    pub tool: Color,
    pub system: Color,
    pub accent: Color,
    pub footer: Color,
}

impl Theme {
    /// OMP 风格主题（§21 M5）。
    pub fn omp() -> Self {
        Self {
            user: Color::Cyan,
            assistant: Color::Green,
            reasoning: Color::DarkGray,
            tool: Color::Yellow,
            system: Color::Magenta,
            accent: Color::LightBlue,
            footer: Color::Gray,
        }
    }
}
