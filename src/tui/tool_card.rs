//! Tool-card rendering and semantic-text projection.

use ratatui::style::{Modifier, Style};
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
    // N = collapsed_lines（[ui] collapsed_lines；0 = 折叠态只显示主行摘要）。
    let collapsed_lines = card.collapsed_lines;
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
            // §用户诉求：read 卡片正文显示真实文件行号（纯视觉装饰，语义文本
            // 仍来自 card.output 原文——复制/选中不携带行号；decor 由 push_hit
            // 按视觉宽-语义宽自动计算，hit 映射精确）。
            let numbered = card.line_number_start.map(|start| {
                let total = body.lines().count();
                let last = start + total.saturating_sub(1);
                (start, last.to_string().len())
            });
            body.lines()
                .enumerate()
                .map(|(i, s)| {
                    if let Some((start, width)) = numbered {
                        Line::from(vec![
                            Span::styled(
                                format!("{:>width$} │ ", start + i),
                                Style::default().fg(theme.muted),
                            ),
                            Span::styled(s.to_string(), Style::default().fg(theme.text)),
                        ])
                    } else {
                        Line::styled(s.to_string(), Style::default().fg(theme.text))
                    }
                })
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
    // 折叠态：collapsed_lines==0 → 只显示主行，不显示任何正文（overflow=true）；
    // 否则正文超折叠线才折叠。空正文（total==0）不折叠、不显示提示行。
    let overflow = total > 0 && (collapsed_lines == 0 || total > collapsed_lines);
    let shown = if card.expanded || !overflow {
        content_lines.as_slice()
    } else if is_running {
        // §PointerHit：运行中显示实时尾部（最新进度）。
        &content_lines[content_lines.len().saturating_sub(RUNNING_LINES)..]
    } else if is_failed {
        // §PointerHit：失败显示错误尾部（tail 末尾，诊断优先）。
        &content_lines[content_lines.len().saturating_sub(FAILED_LINES)..]
    } else {
        &content_lines[..collapsed_lines.min(total)]
    };
    for l in shown {
        // §美化：内容行「前缀带 panel 背景」→ plan_window 整行填满 panel，
        // 卡片主行+内容行同底成组（opencode 卡片"面"）。diff 行只带
        // 前景色（§用户诉求：红绿文字、不改背景）——下方烙 span 时统一
        // 用 panel 兜底，diff 的红绿 fg 与卡片面板共存；非 diff 行走 panel。
        let line_bg = l.style.bg;
        let prefix_style = match line_bg {
            Some(bg) => Style::default().fg(theme.muted).bg(bg),
            None => Style::default().fg(theme.muted).bg(theme.panel),
        };
        let mut spans = vec![Span::styled("│ ", prefix_style)];
        // §修复：正文 span 烙上 Line 级 style 的 fg/bg（diff 红绿、失败 tail
        // 的 error 色等都在 Line 级——wrap_with_semantic 逐字符重建 span
        // 只取 span 级 style，不烙就会丢失颜色）。已有 span 级 style 优先；
        // 背景兜底 panel（卡片面），前景兜底 Line 级 fg。
        for s in &l.spans {
            let mut style = s.style;
            if style.bg.is_none() {
                style = style.bg(line_bg.unwrap_or(theme.panel));
            }
            if style.fg.is_none()
                && let Some(fg) = l.style.fg
            {
                style = style.fg(fg);
            }
            spans.push(Span::styled(s.content.clone(), style));
        }
        // Line 级 style 保留（render_diff_lines 等单测按 line.style 断言）。
        lines.push(Line::from(spans).style(l.style));
    }
    // 统一折叠提示（opencode 式：溢出才显示，点击展开/收缩）。
    // 失败/运行中卡不显示提示——tail 本就是诊断摘要（tail 优先），
    // 提示行会成噪音；整卡仍可点击展开。
    // §修复：提示行每个 span 烙 panel 底——wrap 逐字符重建 span 只保留
    // span 级 style，Line 级 bg 会丢失，导致提示文字落在终端底色上。
    // §用户诉求：collapsed_lines==0 折叠态只显示主行，不显示「点击展开」
    // 提示（干净的主行摘要）；提示只在展开态（「点击折叠」）或
    // collapsed_lines>0 折叠（显示前 N 行）时给出。
    if overflow && !is_failed && !is_running {
        let hint = if card.expanded {
            Some("点击折叠".to_string())
        } else if collapsed_lines > 0 {
            Some(format!("… 点击展开（共 {} 行）", total))
        } else {
            None
        };
        if let Some(hint) = hint {
            let hint_style = Style::default()
                .fg(theme.info)
                .bg(theme.panel)
                .add_modifier(Modifier::ITALIC);
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(theme.muted).bg(theme.panel)),
                Span::styled(hint, hint_style),
            ]));
        }
    }
    lines
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
    } else if let ToolCardState::Running = &card.state
        && let Some(started) = card.started_at_ms
    {
        // §用户诉求：运行中显示已运行秒数（不只 spinner）。
        let now = super::model::now_epoch_ms();
        let elapsed = now.saturating_sub(started);
        meta.push_str(&fmt_duration(elapsed));
    }
    meta
}

/// 工具卡片内容行的语义文本（复制源）：diff 优先，否则 output/tail 对应行。
///
/// diff 卡片：渲染是解析式（文件头隐藏、@@ 变分隔行），语义行必须与渲染
/// 行一一对应（`parse_diff` 同源），否则复制/选中会错位。
pub(super) fn card_semantic_content(card: &ToolCard, index: usize, raw: &str) -> String {
    // 内容区从第 1 行开始（第 0 行是主行）；折叠提示行（最后一行，含 "…"/"点击"）
    // 不是内容，返回空。
    let content_index = index.saturating_sub(1);
    if raw.contains("点击") || raw.contains("…") {
        return String::new(); // 折叠提示行不是可复制内容。
    }
    if let Some(diff) = &card.diff {
        return match parse_diff(diff).get(content_index) {
            Some(row) if !matches!(row.kind, DiffKind::Hunk) => {
                let marker = match row.kind {
                    DiffKind::Minus => "-",
                    DiffKind::Plus => "+",
                    _ => "",
                };
                format!("{marker}{}", row.content.trim_end())
            }
            // 分隔行（⋯ ···）/越界：无可复制内容。
            _ => String::new(),
        };
    }
    let source = card
        .output
        .as_deref()
        .or(card.tail.as_deref())
        .unwrap_or_default();
    let line_of_source = source.split('\n').nth(content_index);
    match line_of_source {
        Some(text) => text.trim_end().to_string(),
        None => raw.to_string(),
    }
}

/// unified diff 解析行：kind（Hunk=区块分隔 / Minus / Plus / Context）+ 真实行号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Hunk,
    Minus,
    Plus,
    Context,
}

struct DiffRow {
    line_no: usize,
    kind: DiffKind,
    content: String,
}

/// 解析 unified diff 为与渲染一一对应的行序列：
/// - `---`/`+++` 文件头：跳过（隐藏）；
/// - `@@ -a,b +c,d @@`：记录真实行号，产出 Hunk 行（渲染为分隔行）；
/// - 内容行：算真实行号（`-` 旧行号、`+` 新行号、上下文新行号），content 去标记。
fn parse_diff(diff_text: &str) -> Vec<DiffRow> {
    let mut rows: Vec<DiffRow> = Vec::new();
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    for line in diff_text.lines() {
        if line.starts_with("@@") {
            if let Some((o, n)) = parse_hunk(line) {
                old_line = o;
                new_line = n;
            }
            rows.push(DiffRow {
                line_no: 0,
                kind: DiffKind::Hunk,
                content: String::new(),
            });
            continue;
        }
        // 文件头（`--- before` / `+++ after`）：隐藏。带空格区分 `--foo` 内容行。
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        let (kind, content) = match line.as_bytes().first() {
            Some(b'-') => (DiffKind::Minus, line[1..].to_string()),
            Some(b'+') => (DiffKind::Plus, line[1..].to_string()),
            _ => (
                DiffKind::Context,
                line.strip_prefix(' ').unwrap_or(line).to_string(),
            ),
        };
        let line_no = match kind {
            DiffKind::Minus => old_line,
            _ => new_line,
        };
        rows.push(DiffRow {
            line_no,
            kind,
            content,
        });
        match kind {
            DiffKind::Minus => old_line = old_line.saturating_add(1),
            DiffKind::Plus | DiffKind::Context => new_line = new_line.saturating_add(1),
            DiffKind::Hunk => {}
        }
    }
    rows
}

/// 渲染 unified diff 为用户友好形式（§用户诉求）：
/// - `---`/`+++` 文件头：隐藏（卡片主行 target 已显示路径）；
/// - `@@` hunk 头：渲染为区块分隔行 `…`（与全项目省略号统一）；
/// - 内容行：带真实文件行号（`-` 旧行号、`+` 新行号、上下文新行号），
///   行号 muted 右对齐；`-` 红、`+` 绿，只改前景色（背景由卡片面板承担）。
pub(super) fn render_diff_lines(diff_text: &str, theme: theme::Theme) -> Vec<Line<'static>> {
    let rows = parse_diff(diff_text);
    let width = rows
        .iter()
        .filter(|r| !matches!(r.kind, DiffKind::Hunk))
        .map(|r| r.line_no.to_string().len())
        .max()
        .unwrap_or(1);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(rows.len());
    for row in rows {
        let line = match row.kind {
            // 区块分隔：行号在此跳变（多 hunk diff 的分割点）。
            // §用户诉求：统一用 `…`（U+2026）——此前 `⋯ ···` 混用居中省略号
            // 与 3 个 ASCII 点（视觉 6 点），与其它省略样式不一致。
            DiffKind::Hunk => Line::styled(
                "…",
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ),
            DiffKind::Minus => Line::from(vec![
                Span::styled(
                    format!("{:>width$} ", row.line_no),
                    Style::default().fg(theme.muted),
                ),
                Span::styled("-", Style::default().fg(theme.error)),
                Span::styled(
                    format!(" {}", row.content),
                    Style::default().fg(theme.error),
                ),
            ]),
            DiffKind::Plus => Line::from(vec![
                Span::styled(
                    format!("{:>width$} ", row.line_no),
                    Style::default().fg(theme.muted),
                ),
                Span::styled("+", Style::default().fg(theme.success)),
                Span::styled(
                    format!(" {}", row.content),
                    Style::default().fg(theme.success),
                ),
            ]),
            DiffKind::Context => Line::from(vec![
                Span::styled(
                    format!("{:>width$}  ", row.line_no),
                    Style::default().fg(theme.muted),
                ),
                Span::styled(row.content, Style::default()),
            ]),
        };
        out.push(line);
    }
    out
}

/// 解析 `@@ -a,b +c,d @@`（count 可省略，缺省 1）→ (old_start, new_start)。
fn parse_hunk(line: &str) -> Option<(usize, usize)> {
    let inner = line.trim_start_matches("@@").trim_end_matches("@@").trim();
    let mut parts = inner.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    let side = |s: &str| -> Option<usize> {
        let s = s.strip_prefix('-').or_else(|| s.strip_prefix('+'))?;
        s.split(',').next()?.parse().ok()
    };
    Some((side(old)?, side(new)?))
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
