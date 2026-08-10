//! Tool-card rendering and semantic-text projection.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::model::{ToolCard, ToolCardState};
use super::{SPINNER_FRAMES, fmt_duration, theme};

/// 工具名语义色（§16.2 增强）：bash=info（执行）、edit/write=success（写文件）、
/// read/list/search=accent（读/检索）、web_*=warning（网络）、其余=text。
pub(super) fn tool_name_style(name: &str, theme: theme::Theme) -> Style {
    let color = match name {
        "bash" | "run" => theme.info,
        "edit" | "write" => theme.success,
        "read" | "list" | "search" => theme.accent,
        "web_search" | "web_fetch" => theme.warning,
        _ => theme.text,
    };
    Style::default().fg(color)
}

/// 工具卡片主行（整改 A2/A3）：`icon name target…  metadata` 恒为单个 visual line。
///
/// - icon/status 语义色；name 正常亮度 BOLD；target muted（display-width ellipsis）；
/// - metadata（duration · exit）最右，muted；
/// - 折叠态：失败 tail ≤4 行、运行中实时输出 ≤3 行；成功无正文；
/// - 展开态（active 卡片）：完整输出内联（scrollback 卡片走 overlay，Phase B）。
pub(super) fn tool_card_lines(
    card: &ToolCard,
    anim_tick: u64,
    theme: theme::Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let (icon, status_style) = match &card.state {
        ToolCardState::Running => (
            SPINNER_FRAMES[anim_tick as usize % SPINNER_FRAMES.len()],
            theme.info,
        ),
        ToolCardState::Done { status, .. } => match status {
            crate::tool::outcome::ToolStatus::Succeeded => ("✓", theme.success),
            crate::tool::outcome::ToolStatus::Failed => ("✗", theme.error),
            crate::tool::outcome::ToolStatus::TimedOut => ("⏱", theme.warning),
            crate::tool::outcome::ToolStatus::Cancelled => ("−", theme.muted),
            crate::tool::outcome::ToolStatus::Interrupted => ("⏹", theme.warning),
            crate::tool::outcome::ToolStatus::Rejected => ("⊘", theme.warning),
        },
    };
    // metadata（固定右侧区域）：duration [· exit code]。
    // PM：成功卡的 `exit 0` 是噪声且占宽度，只在非成功时显示退出码。
    let meta = tool_card_meta(card);
    let meta_w = unicode_width::UnicodeWidthStr::width(meta.as_str());
    let icon_w = 2; // "✓ " 等 icon + 空格
    // 预算分配（保证主行单行，§16.2/整改 A2）：
    // icon + name + target + meta（各 1 空格分隔）；name 超宽先截断，target 无余量则丢弃。
    let name_max = width.saturating_sub(icon_w + 1 + meta_w + 1);
    let name = truncate_display(card.name.as_str(), name_max.max(1));
    let name_w = unicode_width::UnicodeWidthStr::width(name.as_str());
    let target_budget = width.saturating_sub(icon_w + 1 + name_w + 1 + meta_w + 1);
    let target = if target_budget >= 1 {
        truncate_display(card.target.as_deref().unwrap_or(""), target_budget)
    } else {
        String::new()
    };

    let mut spans = vec![Span::styled(
        format!("{icon} "),
        Style::default()
            .fg(status_style)
            // §美化：主行首个 span 带 panel 背景 → plan_window 整行填满
            // panel（opencode 卡片"面"），主行与内容行同底成组。
            .bg(theme.panel)
            .add_modifier(Modifier::BOLD),
    )];
    // 工具名按类别着色（§16.2 增强：bash=info、edit/write=success、
    // read/list/search=accent、其余=text），帮助快速识别工具种类。
    // §修复：正文 span 也烙 panel——否则文字区落到终端底色，与卡片面分离。
    let name_style = tool_name_style(&card.name, theme)
        .add_modifier(Modifier::BOLD)
        .bg(theme.panel);
    spans.push(Span::styled(name.clone(), name_style));
    if !target.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(theme.panel)));
        spans.push(Span::styled(
            target,
            Style::default()
                .fg(theme.muted)
                .bg(theme.panel)
                .add_modifier(Modifier::DIM),
        ));
    }
    if !meta.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(theme.panel)));
        spans.push(Span::styled(
            meta,
            Style::default().fg(theme.muted).bg(theme.panel),
        ));
    }
    let mut lines = vec![Line::from(spans)];

    // §用户诉求：所有工具内容统一折叠（opencode 式）——
    // - 有 diff（edit/write）：渲染红绿 diff，未展开显示前 N 行 + "… 点击展开"；
    // - 无 diff（bash 等）：渲染输出文本，同样前 N 行 + "… 点击展开"；
    // - 已展开：显示全部内容。
    const COLLAPSED_LINES: usize = 10;
    // §PointerHit：失败/运行中折叠态用更小的尾部窗口——注释契约
    // （"失败 tail ≤4 行、运行中实时输出 ≤3 行"），避免错误诊断被 10 行窗口
    // 推到主行之后滚出视口。
    const FAILED_LINES: usize = 4;
    const RUNNING_LINES: usize = 3;

    // §PointerHit：失败卡片折叠态显示错误 tail（不是 output 开头）；
    // 运行中卡片折叠态显示实时尾部（最新进度）。
    let is_failed = matches!(
        &card.state,
        ToolCardState::Done { status, .. } if *status != crate::tool::outcome::ToolStatus::Succeeded
    );
    let is_running = matches!(&card.state, ToolCardState::Running);

    // 确定「内容行」：diff 优先；失败无 diff 用 tail；否则 output。
    let content_lines: Vec<Line<'static>> = if let Some(diff_text) = &card.diff {
        render_diff_lines(diff_text, theme)
    } else if let Some(tail) = card.tail.as_deref() {
        if is_failed {
            // 失败：红色关键 tail（错误诊断优先）。
            tail.lines()
                .map(|s| {
                    Line::styled(
                        s.to_string(),
                        Style::default().fg(theme.error).add_modifier(Modifier::DIM),
                    )
                })
                .collect()
        } else if let Some(body) = card.output.as_deref() {
            body.lines()
                .map(|s| Line::styled(s.to_string(), Style::default().fg(theme.text)))
                .collect()
        } else {
            tail.lines()
                .map(|s| {
                    Line::styled(
                        s.to_string(),
                        Style::default().fg(theme.error).add_modifier(Modifier::DIM),
                    )
                })
                .collect()
        }
    } else if let Some(body) = card.output.as_deref() {
        let diff_idx = find_diff_start(body);
        // 输出里含 `diff:` 段 → diff 部分着色，其余普通。
        if let Some(idx) = diff_idx {
            let mut out = Vec::new();
            for s in body[..idx].lines() {
                out.push(Line::styled(s.to_string(), Style::default().fg(theme.text)));
            }
            out.extend(render_diff_lines(&body[idx..], theme));
            out
        } else {
            body.lines()
                .map(|s| Line::styled(s.to_string(), Style::default().fg(theme.text)))
                .collect()
        }
    } else if let Some(tail) = card.tail.as_deref() {
        // 无完整 output（失败摘要）→ 显示 tail（错误诊断）。
        tail.lines()
            .map(|s| {
                Line::styled(
                    s.to_string(),
                    Style::default().fg(theme.error).add_modifier(Modifier::DIM),
                )
            })
            .collect()
    } else {
        Vec::new()
    };

    let total = content_lines.len();
    let overflow = total > COLLAPSED_LINES;
    let shown = if card.expanded || !overflow {
        content_lines.as_slice()
    } else if is_running {
        // §PointerHit：运行中显示实时尾部（最新进度）。
        &content_lines[content_lines.len().saturating_sub(RUNNING_LINES)..]
    } else if is_failed {
        // §PointerHit：失败显示错误尾部（tail 末尾，诊断优先）。
        &content_lines[content_lines.len().saturating_sub(FAILED_LINES)..]
    } else {
        &content_lines[..COLLAPSED_LINES]
    };
    for l in shown {
        // §美化：内容行「前缀带 panel 背景」→ plan_window 整行填满 panel，
        // 卡片主行+内容行同底成组（opencode 卡片"面"）。diff 行已自带
        // 红绿背景——**必须落到每个 span**（wrap_with_semantic 逐字符重建
        // Line 会丢 Line 级 style，红绿底将无法填充行尾 padding）。前缀
        // 也带同 bg，让红绿主导整行；非 diff 行走 panel。
        let line_bg = l.style.bg;
        let prefix_style = match line_bg {
            Some(bg) => Style::default().fg(theme.muted).bg(bg),
            None => Style::default().fg(theme.muted).bg(theme.panel),
        };
        let mut spans = vec![Span::styled("│ ", prefix_style)];
        // §修复：正文 span 烙背景——diff 行烙红绿，非 diff 行烙 panel
        //（fill_bg 保留已有背景：inline code 等不被覆盖）。否则正文文字区
        // 落到终端底色，与卡片面分离。
        for s in &l.spans {
            let style = match line_bg {
                Some(bg) => s.style.bg(bg),
                None => fill_bg(s.style, theme.panel),
            };
            spans.push(Span::styled(s.content.clone(), style));
        }
        // 非 diff 行保留 line fg（如失败 tail 的 error 色）；diff 行红绿已
        // 落到 span，Line 级 style 仅供 render_diff_lines 单测断言使用。
        lines.push(Line::from(spans).style(l.style));
    }
    // 统一折叠提示（opencode 式：溢出才显示，点击展开/收缩）。
    if overflow {
        let hint = if card.expanded {
            "点击折叠".to_string()
        } else {
            format!("… 点击展开（共 {} 行）", total)
        };
        // §美化：提示行与内容行同 panel 底（折叠卡尾不突兀）。
        lines.push(Line::styled(
            format!("│ {hint}"),
            Style::default()
                .fg(theme.info)
                .bg(theme.panel)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    lines
}

/// 给 span 烙上面板背景：span 已有背景（diff 红绿 / inline code 等）保留，
/// 否则用 `bg` 填充。修复「正文文字区背景与卡片面不同」的三明治问题。
fn fill_bg(style: Style, bg: Color) -> Style {
    if style.bg.is_some() {
        style
    } else {
        style.bg(bg)
    }
}

/// 工具卡片主行的语义文本（复制源）：`name target  meta`。
///
/// §PointerHit 修复：主行视觉 = `icon name target meta`，语义 = name+target+meta
/// （meta 并入语义而非丢弃），因此 decor = 仅 icon 宽度，语义与视觉 offset
/// 连续对应——不会出现「meta 被当前缀 decor 导致点击/选中错位」。
pub(super) fn card_semantic_header(card: &ToolCard) -> String {
    let mut out = card.name.clone();
    if let Some(target) = &card.target
        && !target.is_empty()
    {
        out.push(' ');
        out.push_str(target);
    }
    // 与 tool_card_lines 的 meta 一致（duration · exit code）。
    let meta = tool_card_meta(card);
    if !meta.is_empty() {
        out.push(' ');
        out.push_str(&meta);
    }
    out
}

/// 工具卡片主行的 metadata 文本（`duration · exit code`；与渲染一致）。
fn tool_card_meta(card: &ToolCard) -> String {
    let mut meta = String::new();
    if let ToolCardState::Done {
        status,
        duration_ms,
        exit_code,
        ..
    } = &card.state
    {
        meta.push_str(&fmt_duration(*duration_ms));
        if *status != crate::tool::outcome::ToolStatus::Succeeded
            && let Some(code) = exit_code
        {
            meta.push_str(&format!(" · exit {code}"));
        }
    }
    meta
}

/// 工具卡片内容行的语义文本（复制源）：diff 优先，否则 output/tail 对应行。
pub(super) fn card_semantic_content(card: &ToolCard, index: usize, raw: &str) -> String {
    // 内容区从第 1 行开始（第 0 行是主行）；折叠提示行（最后一行，含 "…"/"点击"）
    // 不是内容，返回空。
    let content_index = index.saturating_sub(1);
    let source = card
        .diff
        .as_deref()
        .or(card.output.as_deref())
        .or(card.tail.as_deref())
        .unwrap_or_default();
    let line_of_source = source.split('\n').nth(content_index);
    match line_of_source {
        Some(text) => text.trim_end().to_string(),
        None => {
            // 折叠提示行（如 "… 点击展开（共 12 行）"）不是可复制内容。
            if raw.contains("点击") || raw.contains("…") {
                String::new()
            } else {
                raw.to_string()
            }
        }
    }
}

/// 渲染 unified diff 文本为红绿着色行（§16.2 增强：opencode 式 diff 展示）。
///
/// 按行首判定（opencode 式红绿背景）：`+` → 绿底、`-` → 红底、`@@` → 主色、
/// 文件头 → muted BOLD、上下文 → 默认。输入是 edit/write 的 unified diff。
pub(super) fn render_diff_lines(diff_text: &str, theme: theme::Theme) -> Vec<Line<'static>> {
    // 深色前景（绿/红底上的可读文字）。
    let on_green = Style::default()
        .bg(theme.success)
        .fg(Color::Rgb(0x0d, 0x0f, 0x14));
    let on_red = Style::default()
        .bg(theme.error)
        .fg(Color::Rgb(0x0d, 0x0f, 0x14));
    diff_text
        .lines()
        .map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                // 文件头行：muted BOLD（标识文件，非增删内容）。
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD)
            } else if line.strip_prefix('+').is_some() {
                on_green
            } else if line.strip_prefix('-').is_some() {
                on_red
            } else if line.starts_with("@@") {
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::styled(line.to_string(), style)
        })
        .collect()
}

/// 定位 diff 段的起始（`\ndiff:\n` 之后的内容起点）。
/// edit/write 工具输出含 `diff:` 段；返回该段起始字节偏移。
fn find_diff_start(body: &str) -> Option<usize> {
    body.find("\ndiff:\n").map(|idx| idx + "\ndiff:\n".len())
}

/// 按 display width 截断（超宽加 …），保证主行不溢出。
fn truncate_display(text: &str, max_width: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = crate::tui::text::char_cell_width(ch);
        if w + cw + 1 > max_width {
            out.push('…');
            return out;
        }
        out.push(ch);
        w += cw;
    }
    out
}
