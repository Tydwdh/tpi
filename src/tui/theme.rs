//! OMP 语义主题（§16.3：语义 token 与颜色分离，组件引用语义角色）。
//!
//! TUI semantic palette：
//! background terminal/default、panel #262233（消息/卡片背景块）、
//! border #45475a（左竖线/分隔线）、surface #211522、surface_subtle #1b1724、
//! primary #cba6f7、accent #f38ba8、info #89dceb、success #a6e3a1、
//! warning #f9e2af、error #f38ba8、text #cdd6f4、muted #7f849c。
//!
//! 层次感（§美化）：`panel` 用于 User 消息/工具卡片/plan 的面（背景块），
//! `border` 用于左竖线边框与分隔线；`surface`/`surface_subtle` 保留给
//! 选区、菜单、行内代码等小颗粒（与 opencode 的 backgroundPanel/Element 对应）。

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
    /// 面板背景：User 消息/工具卡片/plan 的"面"（比终端底稍亮一档）。
    pub panel: Color,
    /// 左竖线边框与分隔线颜色（灰阶；opencode 单一边框语言）。
    pub border: Color,
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
            // Catppuccin Mocha 底 #1e1e2e 的提亮灰阶：panel 面 / border 线。
            panel: Color::Rgb(0x26, 0x22, 0x33),
            border: Color::Rgb(0x45, 0x47, 0x5a),
            surface: Color::Rgb(0x21, 0x15, 0x22),
            surface_subtle: Color::Rgb(0x1b, 0x17, 0x24),
        }
    }

    /// P2：按名称取主题（`[ui] theme`；未知值回退 omp，不报错）。
    pub fn named(name: &str) -> Self {
        match name {
            "dark" => Self::dark(),
            "light" => Self::light(),
            "opencode" => Self::opencode(),
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
            panel: Color::Rgb(0x22, 0x25, 0x2d),
            border: Color::Rgb(0x3d, 0x41, 0x49),
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
            panel: Color::Rgb(0xf0, 0xf1, 0xf3),
            border: Color::Rgb(0xd0, 0xd3, 0xd9),
            surface: Color::Rgb(0xf5, 0xf5, 0xf5),
            surface_subtle: Color::Rgb(0xe9, 0xea, 0xec),
        }
    }

    /// opencode 风格预设（§美化）：近黑观感靠终端底，灰阶层 = panel 面 /
    /// border 线 / surface 元素；暖橙 primary + 紫 accent，状态色克制。
    pub fn opencode() -> Self {
        Self {
            primary: Color::Rgb(0xfa, 0xb2, 0x83),
            accent: Color::Rgb(0x9d, 0x7c, 0xd8),
            info: Color::Rgb(0x56, 0xb6, 0xc2),
            success: Color::Rgb(0x7f, 0xd8, 0x8f),
            warning: Color::Rgb(0xf5, 0xa7, 0x42),
            error: Color::Rgb(0xe0, 0x6c, 0x75),
            text: Color::Rgb(0xee, 0xee, 0xee),
            muted: Color::Rgb(0x80, 0x80, 0x80),
            panel: Color::Rgb(0x16, 0x16, 0x16),
            border: Color::Rgb(0x48, 0x48, 0x48),
            surface: Color::Rgb(0x14, 0x14, 0x14),
            surface_subtle: Color::Rgb(0x1f, 0x1f, 0x1f),
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
        assert_eq!(Theme::named("opencode"), Theme::opencode());
        assert_eq!(Theme::named("totally-unknown"), Theme::omp());
        // light 主题文字必须与底色可区分（语义角色保持正确方向）。
        assert_ne!(Theme::light().text, Theme::light().surface);
        assert_ne!(Theme::light().text, Theme::light().panel);
        assert_ne!(Theme::dark().text, Theme::dark().surface);
    }

    /// §美化：panel 是"面"、border 是"线"——panel 与终端底/文字都不同色。
    #[test]
    fn panel_and_border_are_visually_distinct() {
        for theme in [
            Theme::omp(),
            Theme::dark(),
            Theme::light(),
            Theme::opencode(),
        ] {
            assert_ne!(
                theme.panel, theme.border,
                "panel 与 border 不得同色（层次靠灰度区分）"
            );
            assert_ne!(theme.panel, theme.text, "panel 底不得与文字同色");
        }
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
        assert_eq!(t.panel, Color::Rgb(0x26, 0x22, 0x33));
        assert_eq!(t.border, Color::Rgb(0x45, 0x47, 0x5a));
        assert_eq!(t.surface, Color::Rgb(0x21, 0x15, 0x22));
        assert_eq!(t.surface_subtle, Color::Rgb(0x1b, 0x17, 0x24));
    }
}
