//! OMP 语义主题（§16.3：语义 token 与颜色分离，组件引用语义角色）。
//!
//! 调色板与 TPI_DESIGN.md §16.3 完全一致：
//! background terminal/default、surface #211522、surface_subtle #1b1724、
//! primary #cba6f7、accent #f38ba8、info #89dceb、success #a6e3a1、
//! warning #f9e2af、error #f38ba8、text #cdd6f4、muted #7f849c。

use ratatui::style::Color;

/// 语义主题（§16.3）。
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub primary: Color,
    pub accent: Color,
    pub info: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub text: Color,
    pub muted: Color,
    pub surface: Color,
    pub surface_subtle: Color,
}

impl Theme {
    /// §16.3 OMP 语义主题。
    pub fn omp() -> Self {
        Self {
            primary: Color::Rgb(0xcb, 0xa6, 0xf7),
            accent: Color::Rgb(0xf3, 0x8b, 0xa8),
            info: Color::Rgb(0x89, 0xdc, 0xeb),
            success: Color::Rgb(0xa6, 0xe3, 0xa1),
            warning: Color::Rgb(0xf9, 0xe2, 0xaf),
            error: Color::Rgb(0xf3, 0x8b, 0xa8),
            text: Color::Rgb(0xcd, 0xd6, 0xf4),
            muted: Color::Rgb(0x7f, 0x84, 0x9c),
            surface: Color::Rgb(0x21, 0x15, 0x22),
            surface_subtle: Color::Rgb(0x1b, 0x17, 0x24),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §16.3 调色板与设计文档逐项一致（主题是契约的一部分）。
    #[test]
    fn omp_palette_matches_design_doc() {
        let t = Theme::omp();
        assert_eq!(t.primary, Color::Rgb(0xcb, 0xa6, 0xf7));
        assert_eq!(t.accent, Color::Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(t.info, Color::Rgb(0x89, 0xdc, 0xeb));
        assert_eq!(t.success, Color::Rgb(0xa6, 0xe3, 0xa1));
        assert_eq!(t.warning, Color::Rgb(0xf9, 0xe2, 0xaf));
        assert_eq!(t.error, Color::Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(t.text, Color::Rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(t.muted, Color::Rgb(0x7f, 0x84, 0x9c));
        assert_eq!(t.surface, Color::Rgb(0x21, 0x15, 0x22));
        assert_eq!(t.surface_subtle, Color::Rgb(0x1b, 0x17, 0x24));
    }
}
