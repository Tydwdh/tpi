//! OMP 语义主题（§16.3：语义 token 与颜色分离，组件引用语义角色）。
//!
//! 调色板与 TPI_DESIGN.md §16.3 完全一致：
//! background terminal/default、surface #211522、surface_subtle #1b1724、
//! primary #cba6f7、accent #f38ba8、info #89dceb、success #a6e3a1、
//! warning #f9e2af、error #f38ba8、text #cdd6f4、muted #7f849c。

use ratatui::style::Color;

/// 语义主题（§16.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// P2：按名称取主题（`[ui] theme`；未知值回退 omp，不报错）。
    pub fn named(name: &str) -> Self {
        match name {
            "dark" => Self::dark(),
            "light" => Self::light(),
            _ => Self::omp(),
        }
    }

    /// 简洁 dark 主题（深色底、高对比语义色）。
    pub fn dark() -> Self {
        Self {
            primary: Color::Rgb(0x82, 0xaa, 0xff),
            accent: Color::Rgb(0xff, 0x7b, 0x72),
            info: Color::Rgb(0x79, 0xc0, 0xff),
            success: Color::Rgb(0x57, 0xab, 0x5a),
            warning: Color::Rgb(0xd2, 0x99, 0x22),
            error: Color::Rgb(0xff, 0x6b, 0x6b),
            text: Color::Rgb(0xd8, 0xde, 0xe9),
            muted: Color::Rgb(0x7f, 0x8c, 0x9d),
            surface: Color::Rgb(0x1e, 0x21, 0x28),
            surface_subtle: Color::Rgb(0x28, 0x2c, 0x34),
        }
    }

    /// 简洁 light 主题（浅底、深色文字）。
    pub fn light() -> Self {
        Self {
            primary: Color::Rgb(0x4c, 0x6e, 0xf5),
            accent: Color::Rgb(0xd6, 0x3a, 0x5c),
            info: Color::Rgb(0x0b, 0x7c, 0xc4),
            success: Color::Rgb(0x2e, 0x7d, 0x32),
            warning: Color::Rgb(0xb0, 0x6a, 0x00),
            error: Color::Rgb(0xc6, 0x28, 0x28),
            text: Color::Rgb(0x24, 0x29, 0x2e),
            muted: Color::Rgb(0x61, 0x6e, 0x7c),
            surface: Color::Rgb(0xf5, 0xf5, 0xf5),
            surface_subtle: Color::Rgb(0xe9, 0xea, 0xec),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2：主题按名称解析；未知值回退 omp。
    #[test]
    fn named_theme_resolves_and_falls_back() {
        assert_eq!(Theme::named("omp"), Theme::omp());
        assert_eq!(Theme::named("dark"), Theme::dark());
        assert_eq!(Theme::named("light"), Theme::light());
        assert_eq!(Theme::named("totally-unknown"), Theme::omp());
        // light 主题文字必须与底色可区分（语义角色保持正确方向）。
        assert_ne!(Theme::light().text, Theme::light().surface);
        assert_ne!(Theme::dark().text, Theme::dark().surface);
    }

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
