//! Markdown and table rendering for transcript messages.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme;

/// Markdown → 带样式的逻辑行（pulldown-cmark；流式增量下对不完整输入友好）。
///
/// 支持的子集：段落、加粗/斜体/删除线、行内代码、围栏代码块、
/// 无序/有序列表、块引用、链接（附 URL）、分隔线。
/// 标题在本实现中渲染为普通文本（样式化标题留待 API 确认后跟进）。
/// `pub(crate)`：model 的 canonical semantic text 生成也用它（③ 统一坐标系）。
pub(crate) fn render_markdown(
    text: &str,
    theme: theme::Theme,
    width: Option<usize>,
) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let parser = Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Modifier> = Vec::new();
    let mut link_urls: Vec<String> = Vec::new();
    let mut in_code_block = false;
    let mut list_ordered: Option<u64> = None;
    let mut item_count: u64 = 1;
    let mut item_pending = false;
    let mut quote_depth: usize = 0;
    // 标题层级（§16.2 增强）：Start(Heading) 时记录，End(Heading) flush 后
    // 对整行应用标题样式（h1-h3 分色，h4+ 归一）。
    let mut heading_level: Option<u8> = None;
    // 表格状态（§16.2 增强）：收集到完整表格后在 End(Table) 一次性渲染
    //（带边框 + 列宽对齐；流式下表格到达时才渲染，无逐 cell 闪烁）。
    let mut table: Option<TableState> = None;

    for event in parser {
        match event {
            Event::Text(t) => {
                if table.is_some() {
                    // 表格内：文本进当前 cell（不参与普通 current 渲染）。
                    table_push_text(&mut table, &t);
                    continue;
                }
                if in_code_block {
                    flush_line(&mut out, &mut current);
                    let content = t.trim_end_matches('\n');
                    out.push(Line::styled(
                        content.to_string(),
                        Style::default().fg(theme.muted).bg(theme.surface_subtle),
                    ));
                } else {
                    if item_pending {
                        item_pending = false;
                        let marker = match list_ordered {
                            Some(_) => {
                                let m = format!("{item_count}. ");
                                item_count += 1;
                                m
                            }
                            None => "• ".to_string(),
                        };
                        current.push(Span::styled(marker, Style::default().fg(theme.accent)));
                    }
                    if quote_depth > 0 && current.is_empty() {
                        current.push(Span::styled(
                            "│ ".repeat(quote_depth),
                            Style::default().fg(theme.muted),
                        ));
                    }
                    let modifier = style_stack
                        .iter()
                        .copied()
                        .fold(Modifier::empty(), |acc, m| acc | m);
                    let mut style = if link_urls.is_empty() {
                        Style::default().fg(theme.text)
                    } else {
                        Style::default().fg(theme.info)
                    }
                    .add_modifier(modifier);
                    if !link_urls.is_empty() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    current.push(Span::styled(t.to_string(), style));
                }
            }
            Event::Code(t) => {
                if table.is_some() {
                    // 表格内行内代码：以纯文本进 cell（表格统一文本渲染）。
                    table_push_text(&mut table, &t);
                    continue;
                }
                current.push(Span::styled(
                    t.to_string(),
                    Style::default().fg(theme.primary).bg(theme.surface_subtle),
                ));
            }
            Event::SoftBreak | Event::HardBreak => flush_line(&mut out, &mut current),
            Event::Rule => {
                flush_line(&mut out, &mut current);
                out.push(Line::styled("──", Style::default().fg(theme.muted)));
            }
            Event::Start(tag) => match tag {
                Tag::Emphasis => style_stack.push(Modifier::ITALIC),
                Tag::Strong => style_stack.push(Modifier::BOLD),
                Tag::Strikethrough => style_stack.push(Modifier::CROSSED_OUT),
                Tag::Heading { level, .. } => {
                    heading_level = Some(level as u8);
                    flush_line(&mut out, &mut current);
                }
                Tag::CodeBlock(_) => {
                    flush_line(&mut out, &mut current);
                    in_code_block = true;
                }
                Tag::List(start) => {
                    list_ordered = start;
                    item_count = start.unwrap_or(1);
                }
                Tag::Item => item_pending = true,
                Tag::Link { dest_url, .. } => {
                    link_urls.push(dest_url.to_string());
                    style_stack.push(Modifier::UNDERLINED);
                }
                Tag::BlockQuote(_) => quote_depth += 1,
                Tag::Table(alignments) => {
                    // 表格开始：flush 当前行，初始化表格收集状态。
                    flush_line(&mut out, &mut current);
                    let mut t = TableState::new();
                    t.alignments = alignments;
                    table = Some(t);
                }
                Tag::TableHead => {
                    // 表头行即第一行（与数据行同一收集机制）。
                }
                Tag::TableRow => {
                    if let Some(t) = &mut table {
                        t.current_row = Vec::new();
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = &mut table {
                        t.current_cell = Some(String::new());
                    }
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    style_stack.pop();
                }
                TagEnd::Heading(_) => {
                    // 把当前行内容应用标题样式（§16.2 增强：h1-h3 分色 BOLD）。
                    if let Some(level) = heading_level.take()
                        && !current.is_empty()
                    {
                        let style = heading_style(level, theme);
                        let spans: Vec<Span<'static>> = std::mem::take(&mut current)
                            .into_iter()
                            .map(|span| Span::styled(span.content, style))
                            .collect();
                        out.push(Line::from(spans));
                    } else {
                        flush_line(&mut out, &mut current);
                    }
                }
                TagEnd::Link => {
                    style_stack.pop();
                    if let Some(url) = link_urls.pop() {
                        current.push(Span::styled(
                            format!(" ({url})"),
                            Style::default().fg(theme.muted),
                        ));
                    }
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    flush_line(&mut out, &mut current);
                }
                TagEnd::Item => flush_line(&mut out, &mut current),
                TagEnd::List(_) => {
                    list_ordered = None;
                    item_count = 1;
                    flush_line(&mut out, &mut current);
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    flush_line(&mut out, &mut current);
                }
                TagEnd::TableCell => {
                    // 完成当前 cell：取走内容，push 进当前行。
                    if let Some(t) = &mut table {
                        if let Some(cell) = t.current_cell.take() {
                            t.current_row.push(cell.trim().to_string());
                        } else {
                            t.current_row.push(String::new());
                        }
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = &mut table
                        && !t.current_row.is_empty()
                    {
                        t.rows.push(std::mem::take(&mut t.current_row));
                    }
                }
                TagEnd::TableHead => {
                    // 表头没有 TableRow 包裹（pulldown 语义：TableHead 直接含
                    // TableCell）；cell 已收集进 current_row，作为第一行入 rows。
                    if let Some(t) = &mut table
                        && !t.current_row.is_empty()
                    {
                        t.rows.push(std::mem::take(&mut t.current_row));
                    }
                }
                TagEnd::Table => {
                    // 表格结束：一次性渲染（§codex：width-aware 列分配 + cell 换行）。
                    if let Some(t) = table.take() {
                        out.extend(render_table(&t, theme, width));
                    }
                    flush_line(&mut out, &mut current);
                }
                TagEnd::Paragraph => flush_line(&mut out, &mut current),
                _ => {}
            },
            _ => {}
        }
    }
    flush_line(&mut out, &mut current);
    out
}

fn flush_line(out: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>) {
    if !current.is_empty() {
        out.push(Line::from(std::mem::take(current)));
    }
}

/// 表格收集状态（§16.2 增强）：`rows[0]` 是表头行，其余为数据行。
struct TableState {
    rows: Vec<Vec<String>>,
    /// 当前正在收集的 cell（每个 cell 的 Text 事件拼接）。
    current_cell: Option<String>,
    /// 当前行（已收集完的 cells）。
    current_row: Vec<String>,
    /// 各列对齐（§codex：从 pulldown 表格分隔行收集）。
    alignments: Vec<pulldown_cmark::Alignment>,
}

impl TableState {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            current_cell: None,
            current_row: Vec::new(),
            alignments: Vec::new(),
        }
    }
}

/// 表格文本追加（cell 内可能含空格拼接；简单拼接 Text 事件）。
fn table_push_text(table: &mut Option<TableState>, text: &str) {
    if let Some(t) = table
        && let Some(cell) = &mut t.current_cell
    {
        cell.push_str(text);
    }
}

/// 收集完毕的表格 → 带边框的对齐行（§codex 移植：width-aware 列分配 +
/// cell 词级换行 + 多行拼接，每行分隔符都在；窄屏退化为 records）。
///
/// `width` 为可用内容宽度（None = 不收缩，按自然列宽）。表格行已在此完成
/// width-aware 布局，外层 `wrap_with_semantic` 不得再按字符切（§codex
/// `table_lines_prewrapped` 语义）。
fn render_table(
    table: &TableState,
    theme: theme::Theme,
    width: Option<usize>,
) -> Vec<Line<'static>> {
    use pulldown_cmark::Alignment;
    let rows = &table.rows;
    if rows.is_empty() {
        return Vec::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }
    // 第一行视为表头（pulldown：TableHead 直接含 cell，作为第一行入 rows）。
    let (header, body) = rows.split_first().unwrap_or((&rows[0], &rows[1..]));
    let normalize = |r: &[String]| -> Vec<String> {
        let mut v = r.to_vec();
        v.truncate(cols);
        v.resize(cols, String::new());
        v
    };
    let header = normalize(header);
    let body: Vec<Vec<String>> = body.iter().map(|r| normalize(r)).collect();
    let alignments: Vec<Alignment> = {
        let mut a = table.alignments.clone();
        a.resize(cols, Alignment::None);
        a
    };

    const GAP: usize = 2;
    const PADDING: usize = 1;
    const MIN_COL: usize = 3;
    let border = Style::default().fg(theme.muted);
    let header_style = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);

    // 列分类与指标（§codex collect_table_column_metrics）。
    let metrics: Vec<TableColMetric> = (0..cols)
        .map(|col| {
            let mut max_width = crate::tui::text::display_width(&header[col]);
            let mut body_token_w = 0usize;
            let mut total_words = 0usize;
            let mut total_cells = 0usize;
            let mut long_tokens = 0usize;
            let mut total_token_count = 0usize;
            for row in &body {
                let cell = &row[col];
                max_width = max_width.max(crate::tui::text::display_width(cell));
                let mut words = 0usize;
                for token in cell.split_whitespace() {
                    let w = crate::tui::text::display_width(token);
                    body_token_w = body_token_w.max(w);
                    long_tokens += usize::from(w >= 20);
                    words += 1;
                }
                if words > 0 {
                    total_words += words;
                    total_cells += 1;
                    total_token_count += words;
                }
            }
            let header_token = header[col]
                .split_whitespace()
                .map(crate::tui::text::display_width)
                .max()
                .unwrap_or(0);
            let kind = if long_tokens > 0
                && long_tokens >= total_token_count.saturating_sub(long_tokens)
            {
                TableColKind::TokenHeavy
            } else if total_cells == 0
                || (total_words as f64 / total_cells as f64) >= 4.0
                || max_width as f64 / total_cells.max(1) as f64 >= 28.0
            {
                TableColKind::Narrative
            } else {
                TableColKind::Compact
            };
            TableColMetric {
                max_width,
                header_token,
                body_token: body_token_w,
                kind,
            }
        })
        .collect();

    // 列宽分配（§codex compute_column_widths：收缩到 available_width 内）。
    let floor_for = |m: &TableColMetric| -> usize {
        let target = match m.kind {
            TableColKind::Narrative | TableColKind::TokenHeavy => 16,
            TableColKind::Compact => m.header_token.max(m.body_token.min(16)),
        };
        target.max(MIN_COL).min(m.max_width)
    };
    let available = width.map(|w| {
        let reserved = (cols + 1) + cols.saturating_sub(1) * GAP + cols * PADDING * 2;
        w.saturating_sub(reserved)
    });
    let mut widths: Vec<usize> = metrics.iter().map(|m| m.max_width.max(MIN_COL)).collect();
    if let Some(max_w) = available {
        let min_total = cols * MIN_COL;
        if max_w < min_total {
            // 窄屏：退化为 records（label: value，逐行）。§codex table_key_value。
            return render_table_records(&header, &body, theme);
        }
        let mut floors: Vec<usize> = metrics.iter().map(floor_for).collect();
        let floor_total: usize = floors.iter().sum();
        if floor_total > max_w {
            let mins = vec![MIN_COL; cols];
            shrink_columns(&mut floors, &mins, &metrics, floor_total - max_w);
        }
        let total: usize = widths.iter().sum();
        if total > max_w {
            let remaining = shrink_columns(&mut widths, &floors, &metrics, total - max_w);
            if remaining > 0 {
                return render_table_records(&header, &body, theme);
            }
        }
    }

    // 生成表格行（§codex render_table_separator / render_table_row）。
    let sep = |ch: char| -> Line<'static> {
        let text = widths
            .iter()
            .map(|w| ch.to_string().repeat(*w + PADDING * 2))
            .collect::<Vec<_>>()
            .join(&" ".repeat(GAP));
        Line::styled(text, border)
    };
    let row_line = |cells: &[String], style: Style| -> Vec<Line<'static>> {
        // 每 cell 在列宽内换行（词边界优先）。
        let wrapped: Vec<Vec<String>> = cells
            .iter()
            .zip(widths.iter())
            .map(|(cell, w)| wrap_cell_text(cell, *w))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        let mut out = Vec::with_capacity(height);
        for line in 0..height {
            let mut spans: Vec<Span<'static>> = vec![Span::styled("│", border)];
            for (col, w) in widths.iter().enumerate() {
                let cell_text = wrapped[col].get(line).cloned().unwrap_or_default();
                let pad = w.saturating_sub(crate::tui::text::display_width(&cell_text));
                let (left, right) = match alignments[col] {
                    Alignment::Left | Alignment::None => (0, pad),
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    Alignment::Right => (pad, 0),
                };
                spans.push(Span::raw(" ".repeat(PADDING + left)));
                spans.push(Span::styled(cell_text, style));
                spans.push(Span::raw(" ".repeat(right + PADDING)));
                if col + 1 < cols {
                    spans.push(Span::raw(" ".repeat(GAP)));
                }
                spans.push(Span::styled("│", border));
            }
            out.push(Line::from(spans));
        }
        out
    };

    let mut out: Vec<Line<'static>> = Vec::with_capacity(2 + body.len() * 2);
    out.push(sep('━'));
    out.extend(row_line(&header, header_style));
    out.push(sep('─'));
    for (i, row) in body.iter().enumerate() {
        out.extend(row_line(row, Style::default().fg(theme.text)));
        if i + 1 < body.len() {
            out.push(sep('─'));
        }
    }
    out.push(sep('━'));
    out
}

/// 表格列类型（§codex：宽度收缩优先级）。
#[derive(Clone, Copy, PartialEq)]
enum TableColKind {
    /// 长叙述文本（>=4 词/格 或 >=28 平均宽）。
    Narrative,
    /// 长 token（路径/URL/哈希），优先让位。
    TokenHeavy,
    /// 短值（计数/状态），抗拒换行。
    Compact,
}

/// 表格列指标（§codex collect_table_column_metrics）。
struct TableColMetric {
    max_width: usize,
    header_token: usize,
    body_token: usize,
    kind: TableColKind,
}

/// 按优先级收缩列宽直到总量 ≤ available（§codex shrink_columns）。
/// 返回无法收缩的剩余量。
fn shrink_columns(
    widths: &mut [usize],
    floors: &[usize],
    metrics: &[TableColMetric],
    mut amount: usize,
) -> usize {
    for kind in [
        TableColKind::TokenHeavy,
        TableColKind::Narrative,
        TableColKind::Compact,
    ] {
        let slack_total: usize = widths
            .iter()
            .enumerate()
            .filter(|(idx, _)| metrics[*idx].kind == kind)
            .map(|(idx, width)| width.saturating_sub(floors[idx]))
            .sum();
        let to_remove = amount.min(slack_total);
        if to_remove == 0 {
            continue;
        }
        // 二分 cap：同类列统一降到 cap，平衡 slack。
        let mut low = 0usize;
        let mut high = widths
            .iter()
            .enumerate()
            .filter(|(idx, _)| metrics[*idx].kind == kind)
            .map(|(idx, width)| width.saturating_sub(floors[idx]))
            .max()
            .unwrap_or(0);
        while low < high {
            let cap = low + (high - low) / 2;
            let removed: usize = widths
                .iter()
                .enumerate()
                .filter(|(idx, _)| metrics[*idx].kind == kind)
                .map(|(idx, width)| width.saturating_sub(floors[idx]).saturating_sub(cap))
                .sum();
            if removed > to_remove {
                low = cap + 1;
            } else {
                high = cap;
            }
        }
        let cap = low;
        let mut removed = 0usize;
        for (idx, width) in widths.iter_mut().enumerate() {
            if metrics[idx].kind != kind {
                continue;
            }
            let reduction = width.saturating_sub(floors[idx]).saturating_sub(cap);
            *width -= reduction;
            removed += reduction;
        }
        let mut remainder = to_remove - removed;
        for (idx, width) in widths.iter_mut().enumerate() {
            if remainder == 0 {
                break;
            }
            if metrics[idx].kind == kind && width.saturating_sub(floors[idx]) == cap {
                *width -= 1;
                remainder -= 1;
            }
        }
        amount -= to_remove;
        if amount == 0 {
            break;
        }
    }
    amount
}

/// 在列宽内换行 cell 文本（§codex wrap_cell：词边界优先，保留 CJK cell 宽度）。
fn wrap_cell_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in text.chars() {
        let w = crate::tui::text::char_cell_width(ch);
        if cur_w + w > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 窄屏回退：records（label: value，逐行）。§codex table_key_value。
fn render_table_records(
    header: &[String],
    body: &[Vec<String>],
    theme: theme::Theme,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for row in body {
        for (col, cell) in row.iter().enumerate() {
            let label = header.get(col).cloned().unwrap_or_default();
            let line = if label.is_empty() {
                cell.clone()
            } else {
                format!("{label}: {cell}")
            };
            out.push(Line::styled(line, Style::default().fg(theme.text)));
        }
        out.push(Line::styled(
            "─".repeat(20),
            Style::default().fg(theme.muted),
        ));
    }
    out
}

/// 标题样式（§16.2 增强）：h1=primary 加粗，h2=accent 加粗，h3=info 加粗，
/// h4+ 归一为 text 加粗（避免过多颜色噪声；正文与标题层级清晰区分）。
fn heading_style(level: u8, theme: theme::Theme) -> Style {
    let color = match level {
        1 => theme.primary,
        2 => theme.accent,
        3 => theme.info,
        _ => theme.text,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}
