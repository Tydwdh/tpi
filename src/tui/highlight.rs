//! 代码块语法高亮（TPI 成熟化：基于 syntect 的 scope 着色）。
//!
//! 设计：
//! - syntect（`default-fancy`）加载内建语法集，纯 Rust（fancy-regex），
//!   无 oniguruma C 依赖；`SyntaxSet` 用 `OnceLock` 全局懒加载一次。
//! - 着色不直接用 syntect 主题色，而是把 scope 名称映射到 tpi 语义色
//!   （跟随用户主题：keyword→primary、string→success、comment→muted italic…）。
//! - 每个代码块渲染时新建 `ParseState` 逐行 parse（跨行状态如多行字符串/
//!   块注释正确）；代码块 ≤64 KiB 有界，且经 renderer 的 md_cache 缓存，
//!   性能可控。未知语言/解析失败回退纯文本（muted + 背景）。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::theme::Theme;

/// 全局语法集（内建 packdump，懒加载一次；线程安全只读）。
fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// 代码块语言 → 语法集 token（未知语言返回 None → 回退纯文本）。
fn syntax_for(language: &str) -> Option<syntect::parsing::SyntaxReference> {
    let set = syntax_set();
    let token = language.trim().to_ascii_lowercase();
    // 常见别名规范化（syntect token 名与 markdown fence 名差异）。
    let token = match token.as_str() {
        "js" | "javascript" => "js".to_string(),
        "ts" | "typescript" => "ts".to_string(),
        "py" | "python" => "py".to_string(),
        "sh" | "shell" | "bash" | "zsh" => "sh".to_string(),
        "yml" | "yaml" => "yaml".to_string(),
        "c++" | "cpp" | "cxx" => "cpp".to_string(),
        "c#" | "cs" | "csharp" => "cs".to_string(),
        "md" | "markdown" => "md".to_string(),
        "rs" | "rust" => "rs".to_string(),
        "json" | "jsonc" => "json".to_string(),
        other => other.to_string(),
    };
    set.find_syntax_by_token(&token).cloned()
}

/// scope 名 → tpi 语义色 + 修饰（后缀匹配，优先级从高到低）。
fn scope_style(scope: &Scope, theme: Theme) -> Style {
    let name = scope.build_string();
    let style = Style::default().fg(theme.muted);
    if name.contains("comment") {
        return style.add_modifier(Modifier::ITALIC);
    }
    let color = if name.contains("keyword") {
        theme.primary
    } else if name.contains("string") || name.contains("character") {
        theme.success
    } else if name.contains("constant.numeric") || name.contains("constant") {
        // §用户诉求：常量/数字用主题橙色（One Dark Pro #d19a66；
        // 其他主题 orange 与 warning 同值，观感不变）。
        theme.orange
    } else if name.contains("entity.name.function") || name.contains("support.function") {
        theme.accent
    } else if name.contains("entity.name.type")
        || name.contains("storage.type")
        || name.contains("support.type")
    {
        theme.info
    } else if name.contains("variable.parameter") {
        theme.warning
    } else {
        return style;
    };
    style.fg(color)
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
            push_segment(
                &mut spans,
                raw_line,
                prev,
                offset,
                stack.as_slice().last().copied(),
                theme,
            );
            stack.apply(&op).ok();
            prev = offset;
        }
        // 尾片段。
        push_segment(
            &mut spans,
            raw_line,
            prev,
            raw_line.len(),
            stack.as_slice().last().copied(),
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
    scope: Option<Scope>,
    theme: Theme,
) {
    if start >= end {
        return;
    }
    let text = &line[start..end];
    // §美化：代码块统一 surface_subtle 背景——与未知语言 fallback 一致，
    // 消除「语法高亮成功无背景、失败有背景」的分裂观感。
    let style = match scope {
        Some(s) => scope_style(&s, theme).bg(theme.surface_subtle),
        None => Style::default().fg(theme.muted).bg(theme.surface_subtle),
    };
    spans.push(Span::styled(text.to_string(), style));
}
