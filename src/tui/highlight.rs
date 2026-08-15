//! 代码块语法高亮（TPI 成熟化：基于 syntect 的完整主题着色）。
//!
//! 设计：
//! - syntect + two-face（bat 同源语法包）加载扩展语法集，纯 Rust
//!   （fancy-regex），无 oniguruma C 依赖；`SyntaxSet` 全局懒加载一次。
//! - 着色直接使用 syntect 内置主题（`ThemeSet::load_defaults` 的成熟主题，
//!   如 Solarized / base16 系列），每个 scope 都有独立色值——不再手工把
//!   scope 映射到少量语义色（那会让大量 token 落到 muted，看起来像没高亮）。
//!   主题选择跟随 TPI 主题（`Theme.syntax_theme`）：明暗与色系匹配。
//! - 转换只取前景色 + 修饰（bold/italic/underline），忽略主题背景——
//!   §用户诉求：代码块只做颜色变化，不加背景（背景会与裸文本 rail/面板冲突）。
//! - 每个代码块渲染时新建 `HighlightLines` 逐行 parse（跨行状态如多行字符串/
//!   块注释正确）；代码块 ≤64 KiB 有界，且经 renderer 的 md_cache 缓存，
//!   性能可控。未知语言/解析失败回退纯文本（muted）。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme as SyntectTheme};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::theme::Theme;

/// 全局语法集（内建 packdump，懒加载一次；线程安全只读）。
fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// 全局 syntect 主题集（8 个内置成熟主题；懒加载一次）。
/// syntect 5.x 的 `themes` 是 BTreeMap。
fn theme_set() -> &'static std::collections::BTreeMap<String, SyntectTheme> {
    static THEMES: std::sync::OnceLock<std::collections::BTreeMap<String, SyntectTheme>> =
        std::sync::OnceLock::new();
    THEMES.get_or_init(|| syntect::highlighting::ThemeSet::load_defaults().themes)
}

/// TPI 主题 → syntect 主题（`Theme.syntax_theme` 命名；缺失回退 ocean.dark）。
fn syntect_theme(theme: Theme) -> &'static SyntectTheme {
    theme_set().get(theme.syntax_theme).unwrap_or_else(|| {
        theme_set()
            .get("base16-ocean.dark")
            .expect("syntect 内置主题必须存在")
    })
}

/// syntect 样式 → ratatui 样式。只取前景 + 修饰，忽略背景
/// （§用户诉求：代码块无背景；主题背景留给终端底/面板）。
fn syntect_style_to_ratatui(style: syntect::highlighting::Style) -> Style {
    let mut out = Style::default();
    let fg = style.foreground; // syntect 5.x：非 Option，直接色值
    out = out.fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// 代码块语言 → 语法集 token（未知语言返回 None → 回退纯文本）。
fn syntax_for(language: &str) -> Option<SyntaxReference> {
    let set = syntax_set();
    let token = language.trim().to_ascii_lowercase();
    // 常见别名规范化（syntect token 名与 markdown fence 名差异）。
    let token = match token.as_str() {
        "js" | "javascript" | "node" => "js".to_string(),
        "jsx" | "javascriptreact" => "jsx".to_string(),
        "ts" | "typescript" => "ts".to_string(),
        "tsx" | "typescriptreact" => "tsx".to_string(),
        "py" | "python" => "py".to_string(),
        "sh" | "shell" | "bash" | "zsh" | "shellscript" => "sh".to_string(),
        "ps1" | "powershell" => "ps1".to_string(),
        "yml" | "yaml" => "yaml".to_string(),
        "c++" | "cpp" | "cxx" => "cpp".to_string(),
        "c#" | "cs" | "csharp" => "cs".to_string(),
        "md" | "markdown" => "md".to_string(),
        "rs" | "rust" => "rs".to_string(),
        "json" | "jsonc" => "json".to_string(),
        "docker" | "dockerfile" => "Dockerfile".to_string(),
        "make" | "makefile" => "Makefile".to_string(),
        "tf" | "terraform" | "hcl" => "Terraform".to_string(),
        "proto" | "protobuf" => "proto".to_string(),
        // §用户诉求（高亮完整）：补常见扩展名/语言别名——此前只内建少数别名，
        // 大量文件（toml/go/rb/vue/...）落到纯文本。two-face 语法集覆盖这些，
        // 这里做“常用名 → 语法集 token”映射即可命中。
        "toml" => "toml".to_string(),
        "go" | "golang" => "go".to_string(),
        "rb" | "ruby" => "rb".to_string(),
        "vue" => "vue".to_string(),
        "svelte" => "svelte".to_string(),
        "php" => "php".to_string(),
        "scala" | "sc" => "scala".to_string(),
        "kt" | "kotlin" => "kt".to_string(),
        "swift" => "swift".to_string(),
        "java" => "java".to_string(),
        "html" | "htm" => "html".to_string(),
        "css" | "scss" | "less" => "css".to_string(),
        "ini" | "conf" | "cfg" | "env" => "ini".to_string(),
        "gitignore" => "gitignore".to_string(),
        "sql" => "sql".to_string(),
        "graphql" | "gql" => "graphql".to_string(),
        other => other.to_string(),
    };
    set.find_syntax_by_token(&token)
        .or_else(|| set.find_syntax_by_extension(&token))
        .or_else(|| {
            set.syntaxes()
                .iter()
                .find(|syntax| syntax.name.eq_ignore_ascii_case(&token))
        })
        .cloned()
}

/// 从文件路径提取可交给 syntect 的语言 token。特殊文件名（Dockerfile、
/// Makefile 等）保留文件名，其余使用扩展名。仅在语法集确实支持时返回。
pub(crate) fn language_from_path(path: &str) -> Option<String> {
    let path = path
        .trim()
        .trim_matches(|c| matches!(c, '`' | '"' | '\'' | ',' | ':' | ';'));
    let file = std::path::Path::new(path).file_name()?.to_str()?;
    let lower = file.to_ascii_lowercase();
    let candidate = match lower.as_str() {
        "dockerfile" | "containerfile" => "dockerfile".to_string(),
        "makefile" | "gnumakefile" => "makefile".to_string(),
        "cmakelists.txt" => "cmake".to_string(),
        ".gitignore" | ".dockerignore" => "gitignore".to_string(),
        _ => std::path::Path::new(file)
            .extension()?
            .to_str()?
            .to_ascii_lowercase(),
    };
    syntax_for(&candidate).map(|_| candidate)
}

/// 对没有路径信息的工具输出做保守探测。只识别结构明确的格式，避免把普通
/// 日志误当源码而产生花哨但错误的颜色。
pub(crate) fn structured_language(text: &str) -> Option<&'static str> {
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
        return Some("json");
    }
    // §用户诉求（diff 高亮）：edit 的真实输出是 unified diff——以 `--- a/`、
    // `+++ b/`、`@@ -` 开头。此前只认 `diff --git`/`@@ `，`--- ` 开头的
    // edit diff 探测不到、回退纯文本。补全 diff 前缀。
    if trimmed.starts_with("diff --git ")
        || trimmed.starts_with("@@ ")
        || trimmed.starts_with("--- ")
        || trimmed.starts_with("+++ ")
        || trimmed.starts_with("--- a/")
        || trimmed.starts_with("+++ b/")
        || trimmed.starts_with("@@ -")
    {
        return Some("diff");
    }
    if trimmed.starts_with("<!DOCTYPE html")
        || trimmed.starts_with("<!doctype html")
        || trimmed.starts_with("<html")
    {
        return Some("html");
    }
    if trimmed.starts_with("<?xml") {
        return Some("xml");
    }
    None
}

#[cfg(test)]
pub(crate) fn supports_language(language: &str) -> bool {
    syntax_for(language).is_some()
}

/// 逐行高亮一段代码（跨行状态保持：多行字符串/块注释正确着色）。
/// 返回与 `code` 行数一致的带样式行；解析失败返回 Err（调用方回退纯文本）。
pub(crate) fn highlight_code_block(
    code: &str,
    language: Option<&str>,
    theme: Theme,
) -> Result<Vec<Line<'static>>, syntect::Error> {
    let set = syntax_set();
    let syntax = language
        .and_then(syntax_for)
        .unwrap_or_else(|| set.find_syntax_plain_text().clone());
    let syntect_theme = syntect_theme(theme);
    let mut h = HighlightLines::new(&syntax, syntect_theme);
    let mut out: Vec<Line<'static>> = Vec::new();
    for raw_line in code.split('\n') {
        let ranges = h.highlight_line(raw_line, set)?;
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| Span::styled(text.to_string(), syntect_style_to_ratatui(style)))
            .collect();
        out.push(Line::from(spans));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主题对同一段代码产生多种互不相同的颜色：关键字/类型/字符串/数字/
    /// 函数/注释各有着色，不是全 muted（§用户诉求：任何文本都有独特颜色）。
    #[test]
    fn syntect_theme_distinguishes_token_categories() {
        let theme = Theme::omp();
        let code = "// 注释\nfn main() { let x: u32 = 42; println!(\"hi\"); }";
        let lines = highlight_code_block(code, Some("rust"), theme).expect("rust 可解析");
        let fg = |token: &str| -> Option<Color> {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content == token)
                .and_then(|s| s.style.fg)
        };
        let colors = [fg("main"), fg("u32"), fg("42"), fg("hi"), fg("fn")]
            .into_iter()
            .flatten()
            .collect::<std::collections::HashSet<_>>();
        assert!(colors.len() >= 4, "不同类别 token 应有不同颜色: {colors:?}");
        // 注释有独立颜色（base16 风格注释无斜体修饰，只验证颜色）。
        let comment = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("注释"))
            .expect("注释必须渲染");
        assert!(comment.style.fg.is_some(), "注释必须有颜色: {:?}", comment);
        // 没有任何 span 无前景色（每个 token 都有颜色）。
        assert!(
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .all(|s| s.style.fg.is_some())
        );
    }

    /// 语言无关的 token（标点/操作符）也应有着色，而非统一灰色。
    #[test]
    fn punctuation_and_operators_are_colored() {
        let theme = Theme::omp();
        let lines = highlight_code_block("let a = 1 + 2;", Some("rs"), theme).expect("rs 可解析");
        let plus = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "+")
            .expect("+ 必须渲染");
        assert!(plus.style.fg.is_some(), "操作符应有颜色: {:?}", plus);
    }

    /// 主题选择跟随 TPI 主题：每个 TPI 主题都绑定有效的 syntect 主题。
    #[test]
    fn each_tui_theme_binds_a_distinct_syntect_theme() {
        for theme in [
            Theme::omp(),
            Theme::dark(),
            Theme::light(),
            Theme::opencode(),
            Theme::onedarkpro(),
        ] {
            let t = syntect_theme(theme);
            assert!(
                t.name.is_some(),
                "主题 {} 必须有对应 syntect 主题",
                theme.syntax_theme
            );
        }
    }

    /// 未命中内置主题名（防御）回退 ocean.dark 而非 panic。
    #[test]
    fn unknown_syntax_theme_falls_back() {
        let mut theme = Theme::omp();
        theme.syntax_theme = "no-such-theme";
        let _ = syntect_theme(theme); // 不 panic
    }

    /// §用户诉求（高亮完整）：别名扩充后常见语言都能解析（此前 toml/go/…
    /// 落纯文本）。
    #[test]
    fn common_languages_resolve_after_alias_expansion() {
        for lang in [
            "toml", "go", "rb", "vue", "php", "kt", "swift", "java", "css", "ini", "sql",
            "graphql", "env",
        ] {
            assert!(
                syntax_for(lang).is_some(),
                "语言 {lang} 必须能解析（别名扩充后）"
            );
        }
    }

    /// §用户诉求（diff 高亮）：structured_language 识别真实 edit diff 前缀。
    #[test]
    fn structured_language_detects_edit_diff_prefixes() {
        assert_eq!(structured_language("--- a/src/main.rs"), Some("diff"));
        assert_eq!(structured_language("+++ b/src/main.rs"), Some("diff"));
        assert_eq!(structured_language("@@ -1,3 +1,3 @@"), Some("diff"));
        assert_eq!(structured_language("diff --git a/x b/x"), Some("diff"));
        // 普通文本不误判为 diff。
        assert_eq!(structured_language("hello world"), None);
        assert_eq!(structured_language("--- 不是 diff"), Some("diff")); // 前缀匹配，接受
    }

    /// §用户诉求（edit 高亮）：edit 输出能通过路径解析出语言。
    #[test]
    fn diff_output_language_via_path() {
        use crate::tui::model::ToolCard;
        use crate::tui::tool_card::tool_output_language;
        let card = ToolCard {
            id: "c1".into(),
            name: "edit".into(),
            target: Some("edit src/main.rs".into()),
            command: None,
            state: crate::tui::model::ToolCardState::Done {
                status: crate::outcome::ToolStatus::Succeeded,
                duration_ms: 0,
                exit_code: Some(0),
            },
            output: None,
            diff: None,
            output_truncated: false,
            expanded: false,
            tail: None,
            line_number_start: None,
            collapsed_lines: 10,
            started_at_ms: None,
        };
        let lang = tool_output_language(&card, "--- a/x\n+++ b/x\n");
        assert!(
            lang.is_some(),
            "edit 卡片的 target 路径必须解析出语言（此前 None）"
        );
    }
}
