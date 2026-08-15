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
    /// 常量/数字/属性色（One Dark Pro 的橙色 #d19a66；语法高亮 numeric 用）。
    pub orange: Color,
    pub error: Color,
    pub text: Color,
    pub muted: Color,
    /// 面板背景：User 消息/工具卡片/plan 的"面"（比终端底稍亮一档）。
    pub panel: Color,
    /// 左竖线边框与分隔线颜色（灰阶；opencode 单一边框语言）。
    pub border: Color,
    pub surface: Color,
    pub surface_subtle: Color,
    /// 代码块语法高亮用的 syntect 内置主题名（§用户诉求：任何 token 都有
    /// 独立颜色——由成熟主题库提供完整 scope 覆盖，而非手工映射少量语义色）。
    pub syntax_theme: &'static str,
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
            orange: Color::Rgb(0xf9, 0xe2, 0xaf),
            error: Color::Rgb(0xf3, 0x8b, 0xa8),
            text: Color::Rgb(0xcd, 0xd6, 0xf4),
            muted: Color::Rgb(0x7f, 0x84, 0x9c),
            // Catppuccin Mocha 底 #1e1e2e 的提亮灰阶：panel 面 / border 线。
            panel: Color::Rgb(0x26, 0x22, 0x33),
            border: Color::Rgb(0x45, 0x47, 0x5a),
            surface: Color::Rgb(0x21, 0x15, 0x22),
            surface_subtle: Color::Rgb(0x1b, 0x17, 0x24),
            // base16-mocha.dark：棕褐暖系 + 高对比，与 omp 紫粉面板同属 mocha 系。
            syntax_theme: "base16-mocha.dark",
        }
    }

    /// P2：按名称取主题（`[ui] theme`；未知值回退 omp，不报错）。
    pub fn named(name: &str) -> Self {
        match name {
            "dark" => Self::dark(),
            "light" => Self::light(),
            "opencode" => Self::opencode(),
            "onedarkpro" | "onedark" => Self::onedarkpro(),
            _ => Self::omp(),
        }
    }

    /// OneDarkPro 配色（§用户诉求，对齐 onedarkpro v3 官方色板）：
    /// bg #282c34、fg #abb2bf、comment #7f848e、red #e06c75、green #98c379、
    /// yellow #e5c07b、blue #61afef、purple #c678dd、cyan #56b6c2、orange #d19a66。
    /// 映射：primary=blue（强调/链接/AI rail）、accent=purple（思考/竖线）、
    /// info=cyan、success=green、warning=yellow、orange=#d19a66（常量/数字）、
    /// error=red、panel=#2c313a（currentLine 提亮面）、border=#3e4451（selection）、
    /// surface=#21252b（widget/sidebar）、surface_subtle=#1e2227（input 深底）。
    pub fn onedarkpro() -> Self {
        Self {
            primary: Color::Rgb(0x61, 0xaf, 0xef),        // blue
            accent: Color::Rgb(0xc6, 0x78, 0xdd),         // purple
            info: Color::Rgb(0x56, 0xb6, 0xc2),           // cyan
            success: Color::Rgb(0x98, 0xc3, 0x79),        // green
            warning: Color::Rgb(0xe5, 0xc0, 0x7b),        // yellow
            orange: Color::Rgb(0xd1, 0x9a, 0x66),         // orange（常量/数字）
            error: Color::Rgb(0xe0, 0x6c, 0x75),          // red
            text: Color::Rgb(0xab, 0xb2, 0xbf),           // fg
            muted: Color::Rgb(0x7f, 0x84, 0x8e),          // comment
            panel: Color::Rgb(0x2c, 0x31, 0x3a),          // currentLine 提亮面
            border: Color::Rgb(0x3e, 0x44, 0x51),         // selection 边框灰
            surface: Color::Rgb(0x21, 0x25, 0x2b),        // widget/sidebar
            surface_subtle: Color::Rgb(0x1e, 0x22, 0x27), // input 深底（代码块）
            // Solarized (dark)：青蓝 + 暖棕的经典克制配色，接近 One Dark 观感。
            syntax_theme: "Solarized (dark)",
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
            orange: Color::Rgb(0xd2, 0x99, 0x22),
            error: Color::Rgb(0xff, 0x6b, 0x6b),
            text: Color::Rgb(0xd8, 0xde, 0xe9),
            muted: Color::Rgb(0x7f, 0x8c, 0x9d),
            panel: Color::Rgb(0x22, 0x25, 0x2d),
            border: Color::Rgb(0x3d, 0x41, 0x49),
            surface: Color::Rgb(0x1e, 0x21, 0x28),
            surface_subtle: Color::Rgb(0x28, 0x2c, 0x34),
            // base16-ocean.dark：蓝冷系，与 dark（Nord 系蓝灰）同气质。
            syntax_theme: "base16-ocean.dark",
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
            orange: Color::Rgb(0xb0, 0x6a, 0x00),
            error: Color::Rgb(0xc6, 0x28, 0x28),
            text: Color::Rgb(0x24, 0x29, 0x2e),
            muted: Color::Rgb(0x61, 0x6e, 0x7c),
            panel: Color::Rgb(0xf0, 0xf1, 0xf3),
            border: Color::Rgb(0xd0, 0xd3, 0xd9),
            surface: Color::Rgb(0xf5, 0xf5, 0xf5),
            surface_subtle: Color::Rgb(0xe9, 0xea, 0xec),
            // base16-ocean.light：浅底下的高对比蓝系。
            syntax_theme: "base16-ocean.light",
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
            orange: Color::Rgb(0xf5, 0xa7, 0x42),
            error: Color::Rgb(0xe0, 0x6c, 0x75),
            text: Color::Rgb(0xee, 0xee, 0xee),
            muted: Color::Rgb(0x80, 0x80, 0x80),
            panel: Color::Rgb(0x16, 0x16, 0x16),
            border: Color::Rgb(0x48, 0x48, 0x48),
            surface: Color::Rgb(0x14, 0x14, 0x14),
            surface_subtle: Color::Rgb(0x1f, 0x1f, 0x1f),
            // base16-eighties.dark：复古暖色，与 opencode 的暖橙主色呼应。
            syntax_theme: "base16-eighties.dark",
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
            Theme::onedarkpro(),
        ] {
            assert_ne!(
                theme.panel, theme.border,
                "panel 与 border 不得同色（层次靠灰度区分）"
            );
            assert_ne!(theme.panel, theme.text, "panel 底不得与文字同色");
        }
    }

    /// §用户诉求：onedarkpro 按 OneDarkPro 配色解析；`onedark` 别名同样生效。
    /// 对齐 onedarkpro v3 官方色板：comment #7f848e、orange #d19a66、
    /// widget #21252b、input 深底 #1e2227。
    #[test]
    fn onedarkpro_palette_and_alias() {
        assert_eq!(Theme::named("onedarkpro"), Theme::onedarkpro());
        assert_eq!(Theme::named("onedark"), Theme::onedarkpro());
        let t = Theme::onedarkpro();
        assert_eq!(t.primary, Color::Rgb(0x61, 0xaf, 0xef)); // blue
        assert_eq!(t.success, Color::Rgb(0x98, 0xc3, 0x79)); // green
        assert_eq!(t.error, Color::Rgb(0xe0, 0x6c, 0x75)); // red
        assert_eq!(t.text, Color::Rgb(0xab, 0xb2, 0xbf));
        assert_eq!(t.panel, Color::Rgb(0x2c, 0x31, 0x3a));
        assert_eq!(t.muted, Color::Rgb(0x7f, 0x84, 0x8e)); // comment
        assert_eq!(t.orange, Color::Rgb(0xd1, 0x9a, 0x66)); // orange
        assert_eq!(t.surface, Color::Rgb(0x21, 0x25, 0x2b)); // widget/sidebar
        assert_eq!(t.surface_subtle, Color::Rgb(0x1e, 0x22, 0x27)); // input 深底
        assert_eq!(t.border, Color::Rgb(0x3e, 0x44, 0x51)); // selection 边框
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
        assert_eq!(t.orange, Color::Rgb(0xf9, 0xe2, 0xaf)); // 与 warning 同值
        assert_eq!(t.error, Color::Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(t.text, Color::Rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(t.muted, Color::Rgb(0x7f, 0x84, 0x9c));
        assert_eq!(t.panel, Color::Rgb(0x26, 0x22, 0x33));
        assert_eq!(t.border, Color::Rgb(0x45, 0x47, 0x5a));
        assert_eq!(t.surface, Color::Rgb(0x21, 0x15, 0x22));
        assert_eq!(t.surface_subtle, Color::Rgb(0x1b, 0x17, 0x24));
    }
}
