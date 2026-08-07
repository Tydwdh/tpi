//! TUI 渲染层（文档 §16）。
//!
//! 只有 renderer 可以调用 Crossterm/Ratatui 或写 stdout；Agent、provider、tool
//! 和日志模块只能发送事件（§16.1、§3.2 不变量 11）。
//!
//! 对标成熟终端 Agent（Claude Code/OpenCode 式）：
//! - §16.1 inline viewport：保留终端 scrollback，闭合行经 `insert_before`
//!   提交到活动区上方，底部只重绘变化内容；不支持时降级为活动区内部滚动
//!   （状态栏显示兼容模式说明）。
//! - §16.2 信息层级：用户消息细紫红左 rail + `you` 标签；assistant 无填充卡片；
//!   thinking dim italic 可折叠（Alt+T）；工具调用单行卡片
//!   `icon name duration status`（运行中 spinner 动画，失败保留红色关键 tail）；
//!   plan 独立紧凑区域；footer 展示 workspace/model/usage/状态；编辑器硬件光标。
//! - §16.3 OMP 语义主题（theme.rs 与设计文档调色板逐项一致）。
//! - Markdown 渲染（pulldown-cmark）：assistant/用户消息的加粗、行内代码、
//!   代码块、列表、引用、链接；按条目版本缓存渲染结果，流式增量只失效最后一条。

pub mod editor;
pub mod model;
pub mod theme;

use std::collections::HashMap;
use std::io::{BufWriter, Stdout};
use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use model::{Entry, LineKind, StatusLine, ToolCard, ToolCardState, TranscriptLine, ViewModel};

/// 帧合并间隔（§16.1：100-500 deltas/s 时按 16 ms 合并，而不是 delta 数量等于 draw 次数）。
pub const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// spinner 动画帧（§16.1：动画时钟独立，活动时推进）。
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 斜杠命令单一来源（/help 与补全菜单共用；描述原生中文，§16.3）。
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("quit", "退出 TPI"),
    ("settings", "查看生效配置及来源"),
    ("model", "查看当前模型"),
    ("help", "显示帮助与快捷键"),
    ("session", "查看会话与成本"),
    ("new", "开始新会话"),
    ("cancel", "取消当前 run"),
    ("thinking", "查看推理设置"),
    ("diff", "查看最近文件 diff"),
    ("compact", "手动压缩上下文"),
];

/// stdout 的唯一所有者（§3.2 不变量 11、§16.1）。
///
/// M6+：内部持有 Ratatui `Terminal`（inline viewport，保留 scrollback）；
/// `draw` 是唯一写 stdout 的路径。帧合并由 [`should_draw`](Self::should_draw) 控制。
pub struct Renderer {
    terminal: Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    last_draw: Option<Instant>,
    /// 距上次 draw 的合并计数（诊断与测试用）。
    pub coalesced_events: u64,
    theme: theme::Theme,
    /// inline scrollback 是否可用（首次 `insert_before` 失败后降级为内部滚动）。
    scrollback: bool,
    /// 已提交到 scrollback 的（折行后）行数。
    committed_lines: usize,
    /// Markdown 渲染缓存：条目 version → 渲染后的逻辑行。
    md_cache: HashMap<u64, Vec<Line<'static>>>,
    /// 缓存有效时的终端宽度；宽度变化清空缓存并重置提交位置（§16.1）。
    cache_width: u16,
}

/// 一帧的布局结果：待提交到 scrollback 的行与新的提交位置。
struct FramePlan {
    /// 活动区窗口内容（已按宽度折行的逻辑行，直接渲染）。
    window: Vec<Line<'static>>,
    overflow: Vec<Line<'static>>,
    committed_after: usize,
}

impl Renderer {
    /// 初始化终端（raw mode + 隐藏光标 + inline viewport + 同步更新支持）。
    pub fn new() -> std::io::Result<Self> {
        ratatui::crossterm::terminal::enable_raw_mode()?;
        let stdout = BufWriter::new(std::io::stdout());
        let backend = CrosstermBackend::new(stdout);
        // §16.1：活动区高度在启动时按窗口计算一次（约 2/5 屏，夹在 12..=32）。
        let (_, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        let height = ((rows * 2) / 5).clamp(12, 32);
        let mut terminal = match Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(height),
            },
        ) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = ratatui::crossterm::terminal::disable_raw_mode();
                return Err(error);
            }
        };
        if let Err(error) = terminal.hide_cursor() {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
            return Err(error);
        }
        // inline TUI 不清除 scrollback 或整个屏幕；首帧只绘制自己的区域。
        Ok(Self {
            terminal,
            last_draw: None,
            coalesced_events: 0,
            theme: theme::Theme::omp(),
            scrollback: true,
            committed_lines: 0,
            md_cache: HashMap::new(),
            cache_width: 0,
        })
    }

    /// 距上次 draw 是否已过帧间隔（§16.1：16 ms 合并）。
    pub fn should_draw(&self) -> bool {
        match self.last_draw {
            Some(last) => last.elapsed() >= FRAME_INTERVAL,
            None => true,
        }
    }

    /// 渲染一帧（唯一的 stdout 写入路径；§16.1 以帧为单位合并模型增量）。
    pub fn draw(&mut self, view: &ViewModel) -> std::io::Result<()> {
        let theme = self.theme;
        let mut cache = std::mem::take(&mut self.md_cache);
        let mut cache_width = self.cache_width;
        let mut committed = self.committed_lines;
        let mut overflow: Vec<Line<'static>> = Vec::new();
        let mut new_committed = committed;
        let scrollback = self.scrollback;
        self.terminal.draw(|frame| {
            let plan = render_frame(
                frame,
                view,
                theme,
                &mut cache,
                &mut cache_width,
                &mut committed,
                scrollback,
            );
            overflow = plan.overflow;
            new_committed = plan.committed_after;
        })?;
        // §16.1：闭合且不再变化的行提交到 scrollback（活动区上方）。
        if scrollback && !overflow.is_empty() {
            let lines = std::mem::take(&mut overflow);
            let height = lines.len() as u16;
            if self
                .terminal
                .insert_before(height, |buf| {
                    let area = Rect::new(0, 0, buf.area().width, height);
                    ratatui::widgets::Paragraph::new(Text::from(lines.clone())).render(area, buf);
                })
                .is_err()
            {
                // 终端不支持 scrolling region：降级为活动区内部滚动（footer 提示）。
                self.scrollback = false;
            }
        }
        self.committed_lines = new_committed;
        self.md_cache = cache;
        self.cache_width = cache_width;
        self.last_draw = Some(Instant::now());
        self.coalesced_events = 0;
        Ok(())
    }

    /// 终端 resize 时重算布局（§16.1：不丢 transcript、不闪白）。
    pub fn autoresize(&mut self) -> std::io::Result<()> {
        self.terminal.autoresize()
    }

    /// 恢复终端（异常退出也能恢复，§21 M5 验收）。
    pub fn restore(&mut self) -> std::io::Result<()> {
        self.terminal.show_cursor()?;
        self.terminal.flush()?;
        ratatui::crossterm::terminal::disable_raw_mode()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // app 因错误提前返回时仍尽力还原终端。显式 restore 是正常路径；
        // 这里的重复调用安全且避免用户遗留在 raw mode。
        let _ = self.terminal.show_cursor();
        let _ = self.terminal.flush();
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new().expect("renderer init")
    }
}

/// 布局并渲染一帧（Renderer 与测试后端共用；纯布局逻辑集中在 plan_window）。
///
/// 自下而上：输入区（多行，≤4 行）→ footer（1 行）→ 计划条（0..4 行）→ 转录区。
#[allow(clippy::too_many_arguments)]
fn render_frame(
    frame: &mut ratatui::Frame,
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
    cache_width: &mut u16,
    committed: &mut usize,
    scrollback: bool,
) -> FramePlan {
    let area = frame.area();
    // 宽度变化 → Markdown 渲染缓存失效，提交位置重置为当前窗口起点
    // （§16.1：已提交到 scrollback 的历史 immutable，resize 后只重排活动区）。
    let reset_committed = *cache_width != area.width;
    if reset_committed {
        cache.clear();
        *cache_width = area.width;
    }

    let input_rows = input_area_rows(view, area.width);
    let plan_rows = plan_area_rows(view);

    let mut constraints: Vec<Constraint> = vec![Constraint::Min(1)];
    if plan_rows > 0 {
        constraints.push(Constraint::Length(plan_rows));
    }
    constraints.push(Constraint::Length(1)); // footer
    constraints.push(Constraint::Length(input_rows));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut idx = 0;
    let trans_area = chunks[idx];
    idx += 1;
    let plan_area = if plan_rows > 0 {
        let a = chunks[idx];
        idx += 1;
        Some(a)
    } else {
        None
    };
    let footer_area = chunks[idx];
    idx += 1;
    let input_area = chunks[idx];

    if let Some(pa) = plan_area {
        draw_plan(frame, pa, view, theme);
    }
    draw_footer(frame, footer_area, view, theme, scrollback);

    let plan = plan_window(
        view,
        theme,
        trans_area.width,
        trans_area.height,
        *committed,
        reset_committed,
        cache,
    );
    *committed = plan.committed_after;
    let overflow = plan.overflow;
    // 窗口已按宽度折行，无需再次 wrap。
    frame.render_widget(Paragraph::new(plan.window), trans_area);

    // 命令补全菜单浮层（覆盖在转录区上方，§16.2 之外的小浮层）。
    if let Some(menu) = &view.menu {
        let h = (menu.items.len() as u16 + 1).min(9);
        let w = area.width.min(48);
        let y = trans_area.y + trans_area.height.saturating_sub(h);
        draw_menu(frame, Rect::new(area.x, y, w, h), view, theme);
    }

    draw_input(frame, input_area, view, theme);
    FramePlan {
        window: Vec::new(),
        overflow,
        committed_after: *committed,
    }
}

/// 转录窗口规划（纯函数，可单测）：
/// - 窗口 = 距底部 `scroll` 行的最后 `area_h` 行（跟随模式 scroll=0 显示最新内容）；
/// - 跟随模式下，窗口之上的闭合行交给调用方提交到 scrollback（overflow）；
/// - `reset_committed`（resize）时不做提交，提交位置重置为窗口起点。
fn plan_window(
    view: &ViewModel,
    theme: theme::Theme,
    width: u16,
    area_h: u16,
    committed: usize,
    reset_committed: bool,
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
) -> FramePlan {
    let logical = build_transcript_text(view, theme, cache);
    let wrapped = wrap_lines(logical, width as usize);
    let total = wrapped.len();
    let area_h = area_h as usize;
    let scroll = view.transcript_scroll as usize;
    let window_bottom = total.saturating_sub(scroll);
    let window_start = window_bottom.saturating_sub(area_h);
    let window = wrapped[window_start..window_bottom].to_vec();
    let (overflow, committed_after) = if reset_committed {
        (Vec::new(), window_start)
    } else if scroll == 0 && window_start > committed {
        // 跟随模式：窗口之上的行已闭合，提交到 scrollback。
        (wrapped[committed..window_start].to_vec(), window_start)
    } else {
        (Vec::new(), committed)
    };
    FramePlan {
        window,
        overflow,
        committed_after,
    }
}

/// 把逻辑行按 width 折行（替代 ratatui 新版 `Text::wrap`；逐 span 保持样式）。
///
/// 折行规则与 Paragraph 渲染一致（同一 helper 同时服务布局计算与渲染）：
/// 按字符宽度累积，超宽即断行；空行保留为空白行。
fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for line in lines {
        if line.spans.is_empty() {
            if !cur.is_empty() {
                out.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
            }
            out.push(Line::default());
            continue;
        }
        for span in line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                let w = unicode_width::UnicodeWidthChar::width(ch)
                    .unwrap_or(0)
                    .max(1);
                if cur_w + w > width && !cur.is_empty() {
                    out.push(Line::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                }
                cur.push(Span::styled(ch.to_string(), style));
                cur_w += w;
            }
        }
        if !cur.is_empty() {
            out.push(Line::from(std::mem::take(&mut cur)));
            cur_w = 0;
        }
    }
    out
}

/// 把转录条目渲染为逻辑行（Message 按类型着色/加 rail；Tool 渲染为卡片）。
fn build_transcript_text(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in &view.transcript {
        match entry {
            Entry::Message(line) => match line.kind {
                // §16.2：用户消息细紫红左 rail + 小型 `you` 标签。
                LineKind::User => {
                    let rendered = cached_markdown(cache, line, theme);
                    for (i, rendered_line) in rendered.iter().enumerate() {
                        let mut spans = vec![Span::styled("│ ", Style::default().fg(theme.accent))];
                        if i == 0 {
                            spans.push(Span::styled(
                                "you ",
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        } else {
                            spans.push(Span::raw("    "));
                        }
                        spans.extend(rendered_line.spans.iter().cloned());
                        out.push(Line::from(spans));
                    }
                }
                // §16.2：assistant 无填充卡片，正文为主（Markdown 渲染）。
                LineKind::Assistant => {
                    let rendered = cached_markdown(cache, line, theme);
                    out.extend(rendered.iter().cloned());
                }
                // §16.2：thinking dim italic，可折叠（Alt+T）。
                LineKind::Reasoning => {
                    if view.reasoning_visible {
                        for (i, s) in line.text.split('\n').enumerate() {
                            let prefix = if i == 0 { "思考 " } else { "    " };
                            out.push(Line::styled(
                                format!("{prefix}{s}"),
                                Style::default()
                                    .fg(theme.muted)
                                    .add_modifier(Modifier::ITALIC),
                            ));
                        }
                    } else {
                        out.push(Line::styled(
                            "思考 · 已折叠（Alt+T 展开）",
                            Style::default()
                                .fg(theme.muted)
                                .add_modifier(Modifier::ITALIC),
                        ));
                    }
                }
                LineKind::Tool => {
                    for (i, s) in line.text.split('\n').enumerate() {
                        let prefix = if i == 0 { "工具 " } else { "    " };
                        out.push(Line::styled(
                            format!("{prefix}{s}"),
                            Style::default().fg(theme.info),
                        ));
                    }
                }
                LineKind::System => {
                    for (i, s) in line.text.split('\n').enumerate() {
                        let prefix = if i == 0 { "系统 " } else { "    " };
                        out.push(Line::styled(
                            format!("{prefix}{s}"),
                            Style::default().fg(theme.warning),
                        ));
                    }
                }
            },
            Entry::Tool(card) => {
                out.extend(tool_card_lines(card, view.anim_tick, theme));
            }
        }
    }
    out
}

/// 按条目版本缓存 Markdown 渲染（流式增量只重渲染变化条目，§16.1）。
fn cached_markdown(
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
    line: &TranscriptLine,
    theme: theme::Theme,
) -> Vec<Line<'static>> {
    if let Some(lines) = cache.get(&line.version) {
        return lines.clone();
    }
    let lines = render_markdown(&line.text, theme);
    if cache.len() > 2048 {
        cache.clear();
    }
    cache.insert(line.version, lines);
    cache[&line.version].clone()
}

/// Markdown → 带样式的逻辑行（pulldown-cmark；流式增量下对不完整输入友好）。
///
/// 支持的子集：段落、加粗/斜体/删除线、行内代码、围栏代码块、
/// 无序/有序列表、块引用、链接（附 URL）、分隔线。
/// 标题在本实现中渲染为普通文本（样式化标题留待 API 确认后跟进）。
fn render_markdown(text: &str, theme: theme::Theme) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let parser = Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Modifier> = Vec::new();
    let mut link_urls: Vec<String> = Vec::new();
    let mut in_code_block = false;
    let mut list_ordered: Option<u64> = None;
    let mut item_count: u64 = 1;
    let mut item_pending = false;
    let mut quote_depth: usize = 0;

    for event in parser {
        match event {
            Event::Text(t) => {
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
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    style_stack.pop();
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

/// 工具卡片（§16.2）：`icon name · duration · status`，运行中 spinner 动画；
/// 命令摘要（detail）缩进一行展示；失败时保留红色关键 tail（不自动展开几十屏）。
fn tool_card_lines(card: &ToolCard, anim_tick: u64, theme: theme::Theme) -> Vec<Line<'static>> {
    let (icon, status_style, status_text) = match &card.state {
        ToolCardState::Running => (
            SPINNER_FRAMES[anim_tick as usize % SPINNER_FRAMES.len()],
            theme.info,
            "运行中",
        ),
        ToolCardState::Done { status, .. } => match status {
            crate::tool::outcome::ToolStatus::Succeeded => ("✓", theme.success, ""),
            crate::tool::outcome::ToolStatus::Failed => ("✗", theme.error, "失败"),
            crate::tool::outcome::ToolStatus::TimedOut => ("⏱", theme.warning, "超时"),
            crate::tool::outcome::ToolStatus::Cancelled => ("−", theme.muted, "已取消"),
            crate::tool::outcome::ToolStatus::Interrupted => ("⏹", theme.warning, "中断"),
            crate::tool::outcome::ToolStatus::Rejected => ("⊘", theme.warning, "拒绝"),
        },
    };
    let mut spans = vec![Span::styled(
        format!("{icon} "),
        Style::default()
            .fg(status_style)
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        card.name.clone(),
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    ));
    if let ToolCardState::Done {
        duration_ms,
        exit_code,
        ..
    } = &card.state
    {
        spans.push(Span::styled(
            format!(" · {}", fmt_duration(*duration_ms)),
            Style::default().fg(theme.muted),
        ));
        if let Some(code) = exit_code {
            spans.push(Span::styled(
                format!(" · exit {code}"),
                Style::default().fg(theme.muted),
            ));
        }
    }
    if !status_text.is_empty() {
        spans.push(Span::styled(format!(" · {status_text}"), status_style));
    }
    let mut lines = vec![Line::from(spans)];
    // §16.2：实际命令摘要独立一行，缩进展示（运行中也可见，便于观察正在做什么）。
    if let Some(detail) = &card.detail
        && !detail.is_empty()
    {
        lines.push(Line::styled(
            format!("  {detail}"),
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        ));
    }
    if let Some(tail) = &card.tail {
        for s in tail.split('\n') {
            lines.push(Line::styled(
                format!("│ {s}"),
                Style::default().fg(theme.error).add_modifier(Modifier::DIM),
            ));
        }
    }
    lines
}

/// 计划条（§16.2：编辑器上方独立紧凑区域，不出现在 transcript）。
fn plan_area_rows(view: &ViewModel) -> u16 {
    match &view.plan {
        Some(plan) if !plan.items.is_empty() => (1 + plan.items.len().min(3)) as u16,
        _ => 0,
    }
}

fn draw_plan(frame: &mut ratatui::Frame, area: Rect, view: &ViewModel, theme: theme::Theme) {
    let Some(plan) = &view.plan else {
        return;
    };
    let mut lines = vec![Line::styled(
        "计划",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )];
    for item in plan.items.iter().take(3) {
        let (marker, style) = match item.status {
            crate::tool::plan::PlanStatus::Completed => (
                "[x]",
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ),
            crate::tool::plan::PlanStatus::InProgress => {
                ("[>]", Style::default().fg(theme.warning))
            }
            crate::tool::plan::PlanStatus::Pending => ("[ ]", Style::default().fg(theme.muted)),
        };
        lines.push(Line::styled(format!("{marker} {}", item.text), style));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );
}

/// 状态栏（§16.2：workspace、model、turn、token usage、运行状态）。
fn draw_footer(
    frame: &mut ratatui::Frame,
    area: Rect,
    view: &ViewModel,
    theme: theme::Theme,
    scrollback: bool,
) {
    let muted = Style::default().fg(theme.muted);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !view.workspace.is_empty() {
        spans.push(Span::styled(
            view.workspace.clone(),
            Style::default().fg(theme.text),
        ));
        spans.push(Span::styled(" · ", muted));
    }
    spans.push(Span::styled(
        view.model_name.clone(),
        Style::default().fg(theme.info),
    ));
    match &view.status {
        StatusLine::Idle => {
            spans.push(Span::styled(" · ", muted));
            spans.push(Span::styled("就绪", Style::default().fg(theme.success)));
        }
        StatusLine::Running { turn, tool } => {
            spans.push(Span::styled(" · ", muted));
            let spin = SPINNER_FRAMES[view.anim_tick as usize % SPINNER_FRAMES.len()];
            spans.push(Span::styled(
                format!("{spin} turn {turn} · {tool}"),
                Style::default().fg(theme.info),
            ));
        }
        StatusLine::Compacting => {
            spans.push(Span::styled(" · ", muted));
            spans.push(Span::styled(
                "compacting · 系统维护调用",
                Style::default().fg(theme.warning),
            ));
        }
    }
    if view.input_tokens > 0 || view.output_tokens > 0 {
        spans.push(Span::styled(
            format!(
                " · ↑{} ↓{}",
                fmt_tokens(view.input_tokens),
                fmt_tokens(view.output_tokens)
            ),
            muted,
        ));
    }
    if !scrollback {
        spans.push(Span::styled(
            " · 兼容模式（无滚动回退）",
            Style::default().fg(theme.warning),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 输入区（§16.2：硬件 cursor 放在真实输入位置，优先保证中文 IME）。
///
/// 支持多行（Alt+Enter）；行数与光标位置按 display width 折行计算。
fn draw_input(frame: &mut ratatui::Frame, area: Rect, view: &ViewModel, theme: theme::Theme) {
    let line = Line::from(vec![
        Span::styled("❯ ", Style::default().fg(theme.accent)),
        Span::raw(view.input.clone()),
    ]);
    let wrapped = wrap_lines(vec![line], area.width as usize);
    let rows = wrapped.len().max(1);
    let (cursor_row, cursor_col) = input_cursor_cell(&view.input, view.input_cursor, area.width);
    let scroll_rows = cursor_row.saturating_sub(rows as u16 - 1);
    frame.render_widget(Paragraph::new(wrapped).scroll((scroll_rows, 0)), area);
    let y = area.y + (cursor_row - scroll_rows);
    let x = area.x + cursor_col;
    frame.set_cursor_position((x, y));
}

/// 输入区所需行数（≤4 行；长输入内部滚动跟随光标）。
fn input_area_rows(view: &ViewModel, width: u16) -> u16 {
    if view.input.is_empty() {
        return 1;
    }
    let budget = width.saturating_sub(2).max(1) as usize;
    let wrapped = wrap_lines(vec![Line::from(Span::raw(view.input.clone()))], budget);
    wrapped.len().clamp(1, 4) as u16
}

/// 光标在输入区内的 (行, 列)（display-cell 坐标）。
/// 第 0 行含 `❯ ` 前缀宽度；折行后的续行从内容起点计列。
/// 折行规则与渲染一致（同一 `wrap_lines`），保证光标不漂移。
fn input_cursor_cell(input: &str, cursor: usize, width: u16) -> (u16, u16) {
    const PROMPT_WIDTH: u16 = 2; // "❯ "
    // 光标始终由编辑器维护在字符边界；此处防御性吸附到最近边界避免切分 UTF-8。
    let cursor = {
        let mut c = cursor.min(input.len());
        while c > 0 && !input.is_char_boundary(c) {
            c -= 1;
        }
        c
    };
    let budget = width.saturating_sub(PROMPT_WIDTH).max(1) as usize;
    let wrapped = wrap_lines(vec![Line::from(Span::raw(input.to_string()))], budget);
    let cursor_cells =
        PROMPT_WIDTH + unicode_width::UnicodeWidthStr::width(&input[..cursor]) as u16;
    let mut row = 0u16;
    let mut col = 0u16;
    let mut line_start = PROMPT_WIDTH;
    let line_count = wrapped.len();
    for (i, line) in wrapped.iter().enumerate() {
        let line_end = line_start + line.width() as u16;
        if cursor_cells <= line_end || i + 1 == line_count {
            row = i as u16;
            col = if i == 0 {
                // 第 0 行：光标位置含 prompt 前缀（draw_input 的 x = area.x + col）。
                cursor_cells
            } else {
                cursor_cells.saturating_sub(line_start)
            };
            break;
        }
        line_start = line_end;
    }
    (row, col)
}

/// 命令补全菜单（输入以 `/` 开头时弹出；↑/↓ 选择、Tab 补全、Enter 选中）。
fn draw_menu(frame: &mut ratatui::Frame, rect: Rect, view: &ViewModel, theme: theme::Theme) {
    let Some(menu) = &view.menu else {
        return;
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, (name, desc)) in menu.items.iter().enumerate() {
        let selected = i == menu.selected;
        let (glyph, style) = if selected {
            (
                "▸",
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD)
                    .bg(theme.surface),
            )
        } else {
            (" ", Style::default().fg(theme.text))
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} /{name}"), style),
            Span::styled(format!("  {desc}"), Style::default().fg(theme.muted)),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), rect);
}

fn fmt_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms / 1_000) % 60)
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// 用 TestBackend 渲染一帧（测试与录制用，§20.3）。
pub fn draw_to_test_backend(view: &ViewModel, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(height),
        },
    )
    .expect("test terminal");
    let theme = theme::Theme::omp();
    let mut cache: HashMap<u64, Vec<Line<'static>>> = HashMap::new();
    let mut cache_width = 0u16;
    let mut committed = 0usize;
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                view,
                theme,
                &mut cache,
                &mut cache_width,
                &mut committed,
                true,
            );
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// 捕获一次 draw 的 stdout 字节（§20.3：验证无全屏清除序列、单次 flush）。
pub fn draw_captured_bytes(view: &ViewModel) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    {
        let backend = CrosstermBackend::new(&mut out);
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(10),
            },
        )
        .expect("capture terminal");
        let theme = theme::Theme::omp();
        let mut cache: HashMap<u64, Vec<Line<'static>>> = HashMap::new();
        let mut cache_width = 0u16;
        let mut committed = 0usize;
        let mut overflow: Vec<Line<'static>> = Vec::new();
        terminal
            .draw(|frame| {
                let plan = render_frame(
                    frame,
                    view,
                    theme,
                    &mut cache,
                    &mut cache_width,
                    &mut committed,
                    true,
                );
                overflow = plan.overflow;
            })
            .expect("draw");
        if !overflow.is_empty() {
            let height = overflow.len() as u16;
            let lines = std::mem::take(&mut overflow);
            let _ = terminal.insert_before(height, |buf| {
                let area = Rect::new(0, 0, buf.area().width, height);
                Paragraph::new(Text::from(lines.clone())).render(area, buf);
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::LineKind;

    #[test]
    fn plan_window_follows_tail_and_commits_overflow() {
        let mut view = ViewModel::default();
        for i in 0..10 {
            view.push_line(LineKind::Assistant, format!("line {i}"));
        }
        let mut cache = HashMap::new();
        let plan = plan_window(&view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        // 跟随模式：窗口是最后 4 行，前 6 行提交到 scrollback。
        assert_eq!(plan.window.len(), 4);
        assert_eq!(plan.overflow.len(), 6);
        assert_eq!(plan.committed_after, 6);
        let window_text: String = plan
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(window_text.contains("line 9"), "窗口应包含最新行");
        assert!(!window_text.contains("line 0"), "窗口不应包含已提交行");

        // 第二次调用：无新 overflow。
        let plan2 = plan_window(
            &view,
            theme::Theme::omp(),
            80,
            4,
            plan.committed_after,
            false,
            &mut cache,
        );
        assert!(plan2.overflow.is_empty());
        assert_eq!(plan2.committed_after, 6);
    }

    #[test]
    fn plan_window_freezes_commits_when_scrolled() {
        let mut view = ViewModel::default();
        for i in 0..10 {
            view.push_line(LineKind::Assistant, format!("line {i}"));
        }
        view.scroll_up(2);
        let mut cache = HashMap::new();
        let plan = plan_window(&view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        // 翻页：窗口 = [10-2-4, 10-2) = 4..8；不提交。
        assert_eq!(plan.window.len(), 4);
        assert!(plan.overflow.is_empty());
        assert_eq!(plan.committed_after, 0);
        let window_text: String = plan
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(window_text.contains("line 5") && window_text.contains("line 7"));
        assert!(!window_text.contains("line 9"));
    }

    #[test]
    fn plan_window_resize_resets_committed_without_overflow() {
        let mut view = ViewModel::default();
        for i in 0..10 {
            view.push_line(LineKind::Assistant, format!("line {i}"));
        }
        let mut cache = HashMap::new();
        // 先提交 6 行。
        let plan = plan_window(&view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        assert_eq!(plan.committed_after, 6);
        // resize：不产生 overflow，提交位置重置为窗口起点。
        let plan2 = plan_window(
            &view,
            theme::Theme::omp(),
            40,
            4,
            plan.committed_after,
            true,
            &mut cache,
        );
        assert!(plan2.overflow.is_empty());
        // 40 宽度下每行不折行（"line N" 很短），窗口仍是最后 4 行。
        assert_eq!(plan2.committed_after, 6);
    }

    #[test]
    fn markdown_bold_code_and_code_block_render_styled() {
        let theme = theme::Theme::omp();
        let lines = render_markdown("**加粗** 和 `code`", theme);
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert!(
            spans
                .iter()
                .any(|s| s.content == "加粗" && s.style.add_modifier.contains(Modifier::BOLD)),
            "加粗必须带 BOLD 修饰: {spans:?}"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.content == "code" && s.style.fg == Some(theme.primary)),
            "行内代码必须使用 primary 色: {spans:?}"
        );

        let lines = render_markdown("```rust\nfn main() {}\n```", theme);
        assert_eq!(lines.len(), 1, "代码块渲染为一行");
        assert_eq!(lines[0].spans[0].content, "fn main() {}");
    }

    #[test]
    fn markdown_list_and_link_render() {
        let theme = theme::Theme::omp();
        let lines = render_markdown("- 第一项\n- 第二项\n\n[链接](https://example.com)", theme);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("• 第一项") && text.contains("• 第二项"));
        assert!(
            text.contains("链接 (https://example.com)"),
            "链接应附 URL: {text:?}"
        );
    }

    #[test]
    fn tool_card_running_shows_spinner_and_done_shows_status() {
        let theme = theme::Theme::omp();
        let card = ToolCard {
            id: "c1".into(),
            name: "run".into(),
            detail: Some("run: cargo test".into()),
            state: ToolCardState::Running,
            tail: None,
        };
        let lines = tool_card_lines(&card, 0, theme);
        assert!(
            lines[0].spans[0].content.starts_with('⠋'),
            "运行中显示 spinner"
        );
        assert!(lines[0].spans.iter().any(|s| s.content == "run"));
        // 运行中已展示命令摘要（独立缩进行）。
        assert!(
            lines
                .iter()
                .any(|l| l.to_string().contains("run: cargo test")),
            "运行中显示命令摘要: {lines:?}"
        );

        let card = ToolCard {
            id: "c1".into(),
            name: "run".into(),
            detail: Some("run: cargo test".into()),
            state: ToolCardState::Done {
                status: crate::tool::outcome::ToolStatus::Failed,
                duration_ms: 1234,
                exit_code: Some(2),
            },
            tail: Some("exit_code: 1".into()),
        };
        let lines = tool_card_lines(&card, 0, theme);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains('✗'), "失败卡片显示 ✗: {text:?}");
        assert!(text.contains("1.2s"), "耗时格式化: {text:?}");
        assert!(text.contains("exit 2"), "exit code 展示: {text:?}");
        assert!(text.contains("exit_code: 1"), "失败 tail 保留: {text:?}");
    }

    #[test]
    fn input_cursor_cell_tracks_wrapped_lines() {
        // 短输入：第 0 行，列 = 2（prompt）+ 宽度（"你好"=4 cell，字节 6）。
        assert_eq!(input_cursor_cell("你好", 6, 20), (0, 6));
        // 20 个 'a' 在宽度 12（预算 10）下折成两行；光标在末尾 → 第 1 行末尾。
        let s = "a".repeat(20);
        assert_eq!(input_cursor_cell(&s, 20, 12), (1, 10));
        // 光标在第 10 个字符后 → 第 0 行末尾（折行边界；列含 prompt = 2+10）。
        assert_eq!(input_cursor_cell(&s, 10, 12), (0, 12));
        // 中间位置 → 第 1 行第 5 列。
        assert_eq!(input_cursor_cell(&s, 15, 12), (1, 5));
    }

    #[test]
    fn duration_and_token_formatting() {
        assert_eq!(fmt_duration(42), "42ms");
        assert_eq!(fmt_duration(1234), "1.2s");
        assert_eq!(fmt_duration(125_000), "2m05s");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(2_000_000), "2.0M");
    }

    #[test]
    fn spinner_frames_advance_with_anim_tick() {
        let theme = theme::Theme::omp();
        let card = ToolCard {
            id: "c".into(),
            name: "run".into(),
            detail: None,
            state: ToolCardState::Running,
            tail: None,
        };
        let f0 = tool_card_lines(&card, 0, theme).remove(0);
        let f1 = tool_card_lines(&card, 1, theme).remove(0);
        assert_ne!(
            f0.spans[0].content, f1.spans[0].content,
            "动画帧应随 tick 变化"
        );
    }
}
