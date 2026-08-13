//! 代码块语法高亮（TPI 成熟化：基于 syntect 的 scope 着色）。
//!
//! 设计：
//! - syntect + two-face（bat 同源语法包）加载扩展语法集，纯 Rust
//!   （fancy-regex），无 oniguruma C 依赖；`SyntaxSet` 全局懒加载一次。
//! - 着色不直接用 syntect 主题色，而是把 scope 名映射到 tpi 语义角色
//!   （跟随用户主题：keyword→primary、string→success、constant→orange、
//!   function→accent、type→info、comment→muted italic…）。匹配用完整
//!   scope 链 + 精确片段规则表 `SCOPE_RULES`，覆盖变量、成员属性、宏、
//!   标签、markup 等类别。
//! - 每个代码块渲染时新建 `ParseState` 逐行 parse（跨行状态如多行字符串/
//!   块注释正确）；代码块 ≤64 KiB 有界，且经 renderer 的 md_cache 缓存，
//!   性能可控。未知语言/解析失败回退纯文本（muted + 背景）。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::theme::Theme;

/// 全局语法集（内建 packdump，懒加载一次；线程安全只读）。
fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// 代码块语言 → 语法集 token（未知语言返回 None → 回退纯文本）。
fn syntax_for(language: &str) -> Option<syntect::parsing::SyntaxReference> {
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
    if trimmed.starts_with("diff --git ") || trimmed.starts_with("@@ ") {
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

/// scope 类别 → 语义角色（TextMate scope 片段；顺序即优先级，先具体后一般）。
///
/// 匹配语义：栈中任一 scope 名等于该片段、或以 `片段.` 开头（精确匹配，
/// 避免 `meta.function-call` 命中 `function` 这类子串误伤）。基于 syntect
/// 内置语法包（Sublime/TextMate 命名）的常见类别：keyword→primary、
/// string→success、constant→orange、function→accent、type→info、
/// parameter/attribute→warning、comment→muted italic，并补充 markup
/// （bold/italic/heading/link）、invalid、macro、成员属性、标签等。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// keyword / 标签 / 标题。
    Primary,
    /// 函数 / 宏。
    Accent,
    /// 类型 / 类。
    Info,
    /// 字符串 / 字符。
    Success,
    /// 数字 / 常量 / 成员属性 / JSON key。
    Orange,
    /// 参数 / 属性名。
    Warning,
    /// 无效代码。
    Error,
    /// 注释（斜体）。
    Comment,
    /// bold 文本。
    Bold,
    /// italic 文本。
    Italic,
    /// 链接文字（下划线）。
    Link,
    /// 标题（粗体）。
    Heading,
}

impl Role {
    fn color(self, theme: Theme) -> Color {
        match self {
            Role::Primary => theme.primary,
            Role::Accent => theme.accent,
            Role::Info => theme.info,
            Role::Success => theme.success,
            Role::Orange => theme.orange,
            Role::Warning => theme.warning,
            Role::Error => theme.error,
            Role::Comment | Role::Bold | Role::Italic | Role::Link => theme.muted,
            Role::Heading => theme.primary,
        }
    }

    fn modifier(self) -> Modifier {
        match self {
            Role::Comment | Role::Italic => Modifier::ITALIC,
            Role::Bold | Role::Heading => Modifier::BOLD,
            Role::Link => Modifier::UNDERLINED,
            _ => Modifier::empty(),
        }
    }
}

const SCOPE_RULES: &[(&str, Role)] = &[
    ("comment", Role::Comment),
    ("markup.bold", Role::Bold),
    ("markup.italic", Role::Italic),
    ("markup.heading", Role::Heading),
    ("markup.underline.link", Role::Link),
    ("string.other.link", Role::Link),
    ("invalid", Role::Error),
    ("entity.name.function", Role::Accent),
    ("support.function", Role::Accent),
    ("variable.function", Role::Accent),
    ("support.macro", Role::Accent),
    ("meta.preprocessor", Role::Accent),
    ("entity.name.type", Role::Info),
    ("entity.other.inherited-class", Role::Info),
    ("support.type", Role::Info),
    ("support.class", Role::Info),
    ("storage.type", Role::Info),
    ("keyword", Role::Primary),
    ("storage.modifier", Role::Primary),
    ("storage.control", Role::Primary),
    // JSON 的 key 与 value 同为 string.quoted.double.json，靠容器 scope
    // `meta.mapping.key` 区分：key 橙色、value 保持 success（One Dark 风格）。
    ("meta.mapping.key", Role::Orange),
    ("string", Role::Success),
    ("character", Role::Success),
    ("constant.numeric", Role::Orange),
    ("constant.language", Role::Orange),
    ("constant.other", Role::Orange),
    ("support.constant", Role::Orange),
    ("variable.other.member", Role::Orange),
    ("variable.other.property", Role::Orange),
    ("support.type.property-name", Role::Orange),
    ("entity.name.tag", Role::Primary),
    ("entity.other.attribute-name", Role::Warning),
    ("variable.parameter", Role::Warning),
];

/// scope 栈 → tpi 语义色 + 修饰。用完整 scope 链匹配（叶子与容器都参与），
/// 命中 `SCOPE_RULES` 首条精确片段；未命中回退 muted。
fn scope_style(stack: &[Scope], theme: Theme) -> Style {
    let names: Vec<String> = stack.iter().map(|s| s.build_string()).collect();
    let mut style = Style::default().fg(theme.muted);
    for (needle, role) in SCOPE_RULES {
        let hit = names.iter().any(|name| {
            name == *needle
                || name
                    .strip_prefix(needle)
                    .is_some_and(|r| r.starts_with('.'))
        });
        if hit {
            style = style.fg(role.color(theme)).add_modifier(role.modifier());
            break;
        }
    }
    style
}

/// 逐行高亮一段代码（跨行状态保持：多行字符串/块注释正确着色）。
/// 返回与 `code` 行数一致的带样式行；解析失败返回 Err（调用方回退纯文本）。
pub(crate) fn highlight_code_block(
    code: &str,
    language: Option<&str>,
    theme: Theme,
) -> Result<Vec<Line<'static>>, syntect::parsing::ParsingError> {
    let set = syntax_set();
    let syntax = language
        .and_then(syntax_for)
        .unwrap_or_else(|| set.find_syntax_plain_text().clone());
    let mut parse_state = ParseState::new(&syntax);
    let mut out: Vec<Line<'static>> = Vec::new();
    for raw_line in code.split('\n') {
        let ops = parse_state.parse_line(raw_line, set)?;
        let mut stack = ScopeStack::new();
        let mut prev = 0usize;
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (offset, op) in ops {
            // 片段 [prev, offset) 用当前栈顶 scope。
            push_segment(&mut spans, raw_line, prev, offset, stack.as_slice(), theme);
            stack.apply(&op).ok();
            prev = offset;
        }
        // 尾片段。
        push_segment(
            &mut spans,
            raw_line,
            prev,
            raw_line.len(),
            stack.as_slice(),
            theme,
        );
        out.push(Line::from(spans));
    }
    Ok(out)
}

fn push_segment(
    spans: &mut Vec<Span<'static>>,
    line: &str,
    start: usize,
    end: usize,
    stack: &[Scope],
    theme: Theme,
) {
    if start >= end {
        return;
    }
    let text = &line[start..end];
    // §美化：代码块统一 surface_subtle 背景——与未知语言 fallback 一致，
    // 消除「语法高亮成功无背景、失败有背景」的分裂观感。
    let style = scope_style(stack, theme).bg(theme.surface_subtle);
    spans.push(Span::styled(text.to_string(), style));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 语法高亮类别映射：不止关键字——函数/宏/参数/属性/JSON key/标签/字符串
    /// 都有语义色（基于实际 syntect scope 命名的断言）。
    #[test]
    fn scope_rules_colorize_beyond_keywords() {
        let theme = Theme::omp();
        let fg = |lines: &[Line<'static>], token: &str| -> Option<Color> {
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content == token)
                .and_then(|s| s.style.fg)
        };
        // Rust：函数名/宏 accent、字符串 success、数字 orange、类型 info。
        let rust = highlight_code_block(
            "fn main() { let x: u32 = 42; println!(\"{}\", x); }",
            Some("rust"),
            theme,
        )
        .expect("rust 可解析");
        assert_eq!(fg(&rust, "main"), Some(theme.accent), "函数名 accent");
        assert_eq!(fg(&rust, "println!"), Some(theme.accent), "宏 accent");
        assert_eq!(fg(&rust, "u32"), Some(theme.info), "类型 info");
        assert_eq!(fg(&rust, "{}"), Some(theme.success), "字符串 success");
        assert_eq!(fg(&rust, "42"), Some(theme.orange), "数字 orange");
        // Python：函数定义 accent、参数 warning、关键字 primary。
        let py = highlight_code_block("def greet(name):\n    return name", Some("python"), theme)
            .expect("python 可解析");
        assert_eq!(fg(&py, "greet"), Some(theme.accent), "函数定义 accent");
        assert_eq!(fg(&py, "name"), Some(theme.warning), "参数 warning");
        assert_eq!(fg(&py, "return"), Some(theme.primary), "关键字 primary");
        // JS：成员函数调用 accent（variable.function）。
        let js = highlight_code_block("obj.method()", Some("js"), theme).expect("js 可解析");
        assert_eq!(fg(&js, "method"), Some(theme.accent), "函数调用 accent");
        // JSON：key 橙色（meta.mapping.key）、数字/常量 orange。
        let json = highlight_code_block("{\"key\": 42, \"ok\": true}", Some("json"), theme)
            .expect("json 可解析");
        assert_eq!(fg(&json, "key"), Some(theme.orange), "JSON key orange");
        assert_eq!(fg(&json, "ok"), Some(theme.orange), "JSON key orange");
        assert_eq!(fg(&json, "42"), Some(theme.orange), "JSON 数字 orange");
        assert_eq!(fg(&json, "true"), Some(theme.orange), "JSON 常量 orange");
        // HTML：标签名 primary、属性名 warning、属性值 success。
        let html = highlight_code_block("<div class=\"box\">hi</div>", Some("html"), theme)
            .expect("html 可解析");
        assert_eq!(fg(&html, "div"), Some(theme.primary), "标签 primary");
        assert_eq!(fg(&html, "class"), Some(theme.warning), "属性名 warning");
        assert_eq!(fg(&html, "box"), Some(theme.success), "属性值 success");
    }

    /// 精确片段匹配：`meta.function-call` 不得误命中 `function` 类别
    /// （容器 scope 不被当成函数名着色）。
    #[test]
    fn scope_matching_avoids_substring_collisions() {
        // `variable.function` 规则不应把 `variable.function-call` 命中：
        // Rust 函数调用括号内的 token 不应被整体染成 accent。
        let theme = Theme::omp();
        let lines = highlight_code_block("foo()", Some("rust"), theme).expect("rust 可解析");
        // foo 是函数调用 → accent；括号仍是 muted。
        let fg = |token: &str| {
            lines[0]
                .spans
                .iter()
                .find(|s| s.content == token)
                .and_then(|s| s.style.fg)
        };
        assert_eq!(fg("foo"), Some(theme.accent));
        assert_eq!(fg("("), Some(theme.muted));
        assert_eq!(fg(")"), Some(theme.muted));
    }
}
