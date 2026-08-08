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
pub mod effect;
pub mod event;
pub mod model;
pub mod reducer;
pub mod scroll;
pub mod state;
pub mod terminal;
pub mod text;
pub mod theme;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use std::collections::HashMap;
use std::time::Instant;

use model::{Entry, LineKind, StatusLine, ToolCard, ToolCardState, TranscriptLine, ViewModel};
use scroll::{EntryId, ScrollMode};

/// 帧合并间隔（§16.1：100-500 deltas/s 时按 16 ms 合并，而不是 delta 数量等于 draw 次数）。
pub const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// 活动区高度：约 2/5 屏，随终端行数自适应。
///
/// - 下限 12 行（小终端仍可读）；
/// - 上限为 `rows - 12`（给 plan/footer/input 区域留空间），大终端自动拓展。
pub fn activity_height(rows: u16) -> u16 {
    let proportional = (rows * 2) / 5;
    proportional.clamp(12, rows.saturating_sub(12).max(12))
}

/// spinner 动画帧（§16.1：动画时钟独立，活动时推进）。
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 斜杠命令单一来源（/help 与补全菜单共用；描述原生中文，§16.3）。
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("quit", "退出 TPI"),
    ("settings", "查看生效配置及来源"),
    ("model", "查看当前模型"),
    ("help", "显示帮助与快捷键"),
    ("session", "查看会话与成本"),
    ("sessions", "浏览并恢复历史会话"),
    ("new", "开始新会话"),
    ("cancel", "取消当前 run"),
    ("thinking", "查看推理设置"),
    ("diff", "查看本轮全部文件 diff"),
    ("doctor", "环境检查（config/模型/API key/Git Bash）"),
    ("compact", "手动压缩上下文"),
];

/// 鼠标 hit 目标（点击转录行打开详情 Overlay）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    /// 工具卡片（按 call_id 打开详情）。
    Tool(String),
    /// 折叠的 reasoning 行（按稳定 EntryId 打开原文，§4.1）。
    Reasoning(EntryId),
}

/// stdout 的唯一所有者（§3.2 不变量 11、§16.1）。
///
/// M6+：内部持有 Ratatui `Terminal`（inline viewport，保留 scrollback）；
/// TUI v2（§29）：终端生命周期已抽到 [`terminal::TerminalDriver`]，
/// Renderer 不再直接触碰 crossterm。
/// `draw` 是唯一写 stdout 的路径。帧合并由 [`should_draw`](Self::should_draw) 控制。
struct FramePlan {
    /// 活动区窗口内容（已按宽度折行的逻辑行，直接渲染）。
    window: Vec<Line<'static>>,
    /// 窗口内每行对应的工具卡片 id（鼠标点击展开；消息行为 None）。
    row_hits: Vec<Option<HitTarget>>,
    /// 转录区屏幕矩形（鼠标 hit-test）。
    transcript_rect: Rect,
    /// scrollbar 屏幕矩形（fullscreen 1 列；§24）。
    scrollbar_rect: Option<Rect>,
    /// 当前窗口起始全局行（scrollbar 比例用，§24）。
    window_start: usize,
    /// 内容总 visual 行数（scrollbar 比例用，§24）。
    total_rows: usize,
    overflow: Vec<Line<'static>>,
    committed_after: usize,
}
pub struct Renderer {
    driver: terminal::TerminalDriver,
    last_draw: Option<Instant>,
    /// 距上次 draw 的合并计数（诊断与测试用）。
    pub coalesced_events: u64,
    theme: theme::Theme,
    /// inline scrollback 是否可用（首次 `insert_before` 失败后降级为内部滚动；
    /// fullscreen 模式下恒为 false——全屏直接绘制，无 scrollback 概念）。
    scrollback: bool,
    /// 已提交到 scrollback 的（折行后）行数（inline 专用）。
    committed_lines: usize,
    /// Markdown 渲染缓存：条目 version → 渲染后的逻辑行。
    md_cache: HashMap<u64, Vec<Line<'static>>>,
    /// 缓存有效时的终端宽度；宽度变化清空缓存并重置提交位置（§16.1）。
    cache_width: u16,
    /// 最近一帧的转录区矩形（鼠标 hit-test 用）。
    last_transcript_rect: Option<Rect>,
    /// 最近一帧窗口内每行对应的工具卡片 id（鼠标点击展开用）。
    last_row_hits: Vec<Option<HitTarget>>,
    /// 已提交到 scrollback 的行数（折叠/展开状态变化时 hit 失效，置空）。
    hits_valid: bool,
    /// 最近一帧 scrollbar 矩形（§24 鼠标点击/拖拽 hit-test）。
    last_scrollbar_rect: Option<Rect>,
}
impl Renderer {
    /// 初始化终端（§29-30：raw mode + alternate screen（fullscreen）+ bracketed
    /// paste + mouse capture + hide cursor）。P2：主题由配置注入（`[ui] theme`）。
    /// 默认全屏（§1）；inline 仅兼容模式。
    pub fn new(theme: theme::Theme, mode: terminal::ViewMode) -> std::io::Result<Self> {
        let driver = terminal::TerminalDriver::new(mode)?;
        Ok(Self {
            driver,
            last_draw: None,
            coalesced_events: 0,
            theme,
            // inline 模式 scrollback 可用；fullscreen 直接绘制整个终端。
            scrollback: mode == terminal::ViewMode::Inline,
            committed_lines: 0,
            md_cache: HashMap::new(),
            cache_width: 0,
            last_transcript_rect: None,
            last_row_hits: Vec::new(),
            hits_valid: false,
            last_scrollbar_rect: None,
        })
    }

    /// 鼠标点击 hit-test：命中工具卡片/reasoning 行返回目标（Overlay 用）。
    pub fn hit_target(&self, column: u16, row: u16) -> Option<HitTarget> {
        if !self.hits_valid {
            return None;
        }
        let rect = self.last_transcript_rect?;
        if column < rect.x || row < rect.y || row >= rect.y + rect.height {
            return None;
        }
        let index = (row - rect.y) as usize;
        self.last_row_hits.get(index).cloned().flatten()
    }
    /// §24：最近一帧 scrollbar 矩形（鼠标点击/拖拽 hit-test；app 层优先判断）。
    pub fn scrollbar_rect(&self) -> Option<Rect> {
        self.last_scrollbar_rect
    }

    /// 距上次 draw 是否已过帧间隔（§16.1：16 ms 合并）。
    pub fn should_draw(&self) -> bool {
        match self.last_draw {
            Some(last) => last.elapsed() >= FRAME_INTERVAL,
            None => true,
        }
    }

    /// 渲染一帧（唯一的 stdout 写入路径；§16.1 以帧为单位合并模型增量）。
    pub fn draw(&mut self, view: &mut ViewModel) -> std::io::Result<()> {
        let theme = self.theme;
        let mode = self.driver.mode();
        let mut cache = std::mem::take(&mut self.md_cache);
        let mut cache_width = self.cache_width;
        let mut committed = self.committed_lines;
        let mut overflow: Vec<Line<'static>> = Vec::new();
        let mut new_committed = committed;
        let scrollback = self.scrollback;
        let mut plan_out: Option<FramePlan> = None;
        self.driver.draw(|frame| {
            let FramePlan {
                window,
                row_hits,
                transcript_rect,
                scrollbar_rect,
                overflow: frame_overflow,
                committed_after,
                ..
            } = render_frame(
                frame,
                view,
                theme,
                &mut cache,
                &mut cache_width,
                &mut committed,
                scrollback,
                mode,
            );
            overflow = frame_overflow;
            new_committed = committed_after;
            plan_out = Some(FramePlan {
                window,
                row_hits,
                transcript_rect,
                scrollbar_rect,
                window_start: 0,
                total_rows: 0,
                overflow: Vec::new(),
                committed_after,
            });
        })?;
        // 鼠标 hit-test 数据（卡片展开状态变化时由调用方置 invalid）。
        if let Some(plan) = plan_out {
            self.last_transcript_rect = Some(plan.transcript_rect);
            self.last_row_hits = plan.row_hits;
            self.hits_valid = true;
            self.last_scrollbar_rect = plan.scrollbar_rect;
        }
        // §16.1：闭合且不再变化的行提交到 scrollback（活动区上方；仅 inline）。
        // fullscreen：alternate screen 内直接绘制整个终端，无 scrollback。
        if scrollback && !overflow.is_empty() {
            let lines = std::mem::take(&mut overflow);
            let height = lines.len() as u16;
            if self
                .driver
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
    ///
    /// fullscreen：Viewport::Fullscreen 自动跟随终端大小；inline：Viewport::Inline
    /// 高度在 resize 时保持不变（只重算原点），行数变化时重建 viewport。
    pub fn autoresize(&mut self) -> std::io::Result<()> {
        self.driver.autoresize()
    }

    /// 恢复终端（异常退出也能恢复，§21 M5 验收；委托 TerminalDriver 逆序恢复）。
    pub fn restore(&mut self) -> std::io::Result<()> {
        self.driver.restore()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // app 因错误提前返回时仍尽力还原终端。显式 restore 是正常路径；
        // TerminalDriver 的 Drop 同样尽力恢复（§31），重复调用安全。
        let _ = self.driver.restore();
    }
}

/// 布局并渲染一帧（Renderer 与测试后端共用；纯布局逻辑集中在 plan_window）。
///
/// 自下而上：输入区（多行，≤4 行）→ footer（1 行）→ 计划条（0..4 行）→ 转录区。
#[allow(clippy::too_many_arguments)]
fn render_frame(
    frame: &mut ratatui::Frame,
    view: &mut ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
    cache_width: &mut u16,
    committed: &mut usize,
    scrollback: bool,
    mode: terminal::ViewMode,
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

    // §8.1：自下而上 = footer(1) → input(1..8) → plan(0..N) → transcript(Min)。
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(1)];
    if plan_rows > 0 {
        constraints.push(Constraint::Length(plan_rows));
    }
    constraints.push(Constraint::Length(input_rows));
    constraints.push(Constraint::Length(1)); // footer
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
    let input_area = chunks[idx];
    idx += 1;
    let footer_area = chunks[idx];

    // §24：fullscreen 在转录区右侧预留 1 列 scrollbar（inline 兼容模式不预留）。
    let scrollbar_enabled = mode == terminal::ViewMode::Fullscreen && trans_area.width >= 2;
    let transcript_width = if scrollbar_enabled {
        trans_area.width - 1
    } else {
        trans_area.width
    };

    if let Some(pa) = plan_area {
        draw_plan(frame, pa, view, theme);
    }
    draw_footer(frame, footer_area, view, theme, scrollback, mode);

    let plan = plan_window(
        view,
        theme,
        transcript_width,
        trans_area.height,
        *committed,
        reset_committed,
        cache,
    );
    *committed = plan.committed_after;
    let overflow = plan.overflow;
    // 窗口已按宽度折行，无需再次 wrap。
    frame.render_widget(Paragraph::new(plan.window), trans_area);

    // §24：全屏历史垂直 scrollbar（1 列；比例按 visual 行数）。
    let scrollbar_rect = if scrollbar_enabled {
        let rect = Rect::new(
            trans_area.x + trans_area.width - 1,
            trans_area.y,
            1,
            trans_area.height,
        );
        draw_scrollbar(frame, rect, theme, plan.window_start, plan.total_rows);
        Some(rect)
    } else {
        None
    };

    // 命令补全菜单浮层（覆盖在转录区上方，§16.2 之外的小浮层）。
    if let Some(menu) = &view.menu {
        let h = (menu.items.len() as u16 + 1).min(9);
        let w = area.width.min(48);
        let y = trans_area.y + trans_area.height.saturating_sub(h);
        draw_menu(frame, Rect::new(area.x, y, w, h), view, theme);
    }

    // 操作型 Modal（§42：/help /settings /doctor 等；覆盖显示，Esc 关闭）。
    if view.modal.is_some() {
        let w = modal_width(area.width);
        let h = modal_height(trans_area.height);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = trans_area.y + trans_area.height.saturating_sub(h) / 2;
        draw_modal(frame, Rect::new(x, y, w, h), view, theme);
    }

    // 搜索框（§14：Ctrl+F；悬浮在转录区顶部）。
    if let Some(search) = &view.search {
        let w = area.width.min(56).max(1);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = trans_area.y;
        let hit_info = if search.query.is_empty() {
            String::from("输入关键词搜索 transcript")
        } else if search.hits.is_empty() {
            String::from("无命中")
        } else {
            format!(
                "{}/{} · Enter/F3 下一个 · Shift+Enter 上一个",
                search.index + 1,
                search.hits.len()
            )
        };
        let line = Line::from(vec![
            Span::styled("🔍 ", Style::default().fg(theme.info)),
            Span::styled(search.query.clone(), Style::default().fg(theme.text)),
            Span::styled("  ", Style::default()),
            Span::styled(hit_info, Style::default().fg(theme.muted)),
        ]);
        frame.render_widget(ratatui::widgets::Clear, Rect::new(x, y, w, 1));
        frame.render_widget(Paragraph::new(line), Rect::new(x, y, w, 1));
    }

    // 详情 Overlay（整改 B：覆盖显示，不重写 scrollback；Esc 关闭）。
    if view.overlay.is_some() {
        let w = modal_width(area.width);
        let h = modal_height(trans_area.height);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = trans_area.y + trans_area.height.saturating_sub(h) / 2;
        draw_overlay(frame, Rect::new(x, y, w, h), view, theme);
    }

    draw_input(frame, input_area, view, theme);
    FramePlan {
        window: Vec::new(),
        row_hits: plan.row_hits,
        scrollbar_rect,
        transcript_rect: trans_area,
        window_start: plan.window_start,
        total_rows: plan.total_rows,
        overflow,
        committed_after: *committed,
    }
}
/// 转录窗口规划（TUI v2 §3-4、§57）：
/// - Follow：窗口 = 尾部 `area_h` 行；跟随模式下窗口之上的闭合行交给调用方
///   提交到 scrollback（overflow，仅 inline）；
/// - Locked(anchor)：窗口顶部 = 锚点行（§3.2：新输出不动视口）；锚点 entry
///   被 trim 时回退最早现存 entry（§68）；窗口越界时 clamp；
/// - 布局后写回 layout_top / entry_heights / transcript_rows（renderer 写回，
///   滚动操作的基础，§4.3 resize 保持语义位置）。
fn plan_window(
    view: &mut ViewModel,
    theme: theme::Theme,
    width: u16,
    area_h: u16,
    committed: usize,
    reset_committed: bool,
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
) -> FramePlan {
    let width = width.max(1) as usize;
    // 按 entry 构建逻辑行（entry → wrapped 行 + hits）。
    // ids/heights 必须以 wrapped_by_entry 为准（含 live 哨兵 group，§7.2）。
    let per_entry = build_transcript_text(view, theme, cache, width);
    let mut wrapped_by_entry: Vec<EntryGroup> = Vec::with_capacity(per_entry.len());
    for (id, logical, hits) in per_entry {
        let (wrapped, wrapped_hits) = wrap_lines_with_hits(logical, hits, width);
        wrapped_by_entry.push((id, wrapped, wrapped_hits));
    }
    let ids: Vec<EntryId> = wrapped_by_entry.iter().map(|(id, _, _)| *id).collect();
    let heights: Vec<usize> = wrapped_by_entry.iter().map(|(_, w, _)| w.len()).collect();
    // 写回高度表（滚动跨 entry 定位用；§4）。
    for (id, height) in ids.iter().zip(heights.iter()) {
        view.entry_heights.insert(*id, *height);
    }

    let area_h = area_h.max(1) as usize;
    let window_start =
        crate::tui::scroll::window_start_row(&ids, &heights, &view.scroll_mode, area_h);
    // 写回布局结果：视口顶部行 + 视口高度（renderer 写回，滚动基础）。
    view.layout_top = Some(crate::tui::scroll::locate_row(&ids, &heights, window_start));
    view.transcript_rows = area_h as u16;

    // 按全局行切片：逐 entry 取窗口内的行。
    let mut window: Vec<Line<'static>> = Vec::new();
    let mut row_hits: Vec<Option<HitTarget>> = Vec::new();
    let mut cursor = 0usize;
    for (_, wrapped, wrapped_hits) in &wrapped_by_entry {
        let start = cursor;
        let end = cursor + wrapped.len();
        cursor = end;
        if end <= window_start || start >= window_start + area_h {
            continue;
        }
        let from = window_start.saturating_sub(start);
        let to = (window_start + area_h - start).min(wrapped.len());
        window.extend(wrapped[from..to].iter().cloned());
        row_hits.extend(wrapped_hits[from..to].iter().cloned());
    }
    let (overflow, committed_after) = if reset_committed {
        (Vec::new(), window_start)
    } else if view.scroll_mode == ScrollMode::Follow && window_start > committed {
        // 跟随模式：窗口之上的行已闭合，提交到 scrollback。
        let mut overflow_lines: Vec<Line<'static>> = Vec::new();
        let mut collected = 0usize;
        for (_, wrapped, _) in &wrapped_by_entry {
            let start = collected;
            let end = collected + wrapped.len();
            collected = end;
            if end <= committed || start >= window_start {
                continue;
            }
            let from = committed.saturating_sub(start);
            let to = (window_start - start).min(wrapped.len());
            overflow_lines.extend(wrapped[from..to].iter().cloned());
        }
        (overflow_lines, window_start)
    } else {
        (Vec::new(), committed)
    };
    FramePlan {
        window,
        row_hits,
        // plan_window 不感知屏幕坐标；transcript_rect 由 render_frame 覆盖。
        transcript_rect: Rect::default(),
        // §24：scrollbar 矩形由 render_frame 计算（需要屏幕坐标）。
        scrollbar_rect: None,
        window_start,
        total_rows: heights.iter().sum(),
        overflow,
        committed_after,
    }
}

fn wrap_lines_with_hits(
    lines: Vec<Line<'static>>,
    hits: Vec<Option<HitTarget>>,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Option<HitTarget>>) {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut out_hits: Vec<Option<HitTarget>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut cur_hit: Option<HitTarget> = None;
    let flush = |out: &mut Vec<Line<'static>>,
                 out_hits: &mut Vec<Option<HitTarget>>,
                 cur: &mut Vec<Span<'static>>,
                 cur_w: &mut usize,
                 cur_hit: &mut Option<HitTarget>| {
        if !cur.is_empty() {
            out.push(Line::from(std::mem::take(cur)));
            out_hits.push(cur_hit.clone());
            *cur_w = 0;
            *cur_hit = None;
        }
    };
    let mut hit_iter = hits.into_iter();
    for line in lines {
        let hit = hit_iter.next().flatten();
        if line.spans.is_empty() {
            if !cur.is_empty() {
                flush(&mut out, &mut out_hits, &mut cur, &mut cur_w, &mut cur_hit);
            }
            out.push(Line::default());
            out_hits.push(hit);
            continue;
        }
        if cur_hit.is_none() {
            cur_hit = hit.clone();
        }
        for span in line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                let w = crate::tui::text::char_cell_width(ch);
                if cur_w + w > width && !cur.is_empty() {
                    flush(&mut out, &mut out_hits, &mut cur, &mut cur_w, &mut cur_hit);
                    cur_hit = hit.clone();
                }
                cur.push(Span::styled(ch.to_string(), style));
                cur_w += w;
            }
        }
        if !cur.is_empty() {
            flush(&mut out, &mut out_hits, &mut cur, &mut cur_w, &mut cur_hit);
        }
    }
    (out, out_hits)
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
                let w = crate::tui::text::char_cell_width(ch);
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

/// 按 entry 分组的渲染结果：(EntryId, 逻辑行, 逐行 hits)。
/// hits 与 lines 等长，工具卡片的行对应卡片 id（鼠标点击展开）。
type EntryGroup = (EntryId, Vec<Line<'static>>, Vec<Option<HitTarget>>);

/// 把转录条目渲染为逻辑行（Message 按类型着色/加 rail；Tool 渲染为卡片）。
///
/// 返回按 entry 分组的结果（含 live 哨兵组，§7.2）。
fn build_transcript_text(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
    width: usize,
) -> Vec<EntryGroup> {
    let mut groups: Vec<EntryGroup> = Vec::with_capacity(view.transcript.len());
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<Option<HitTarget>> = Vec::new();
    let push_hit = |lines: &mut Vec<Line<'static>>,
                    hits: &mut Vec<Option<HitTarget>>,
                    line: Line<'static>,
                    hit: Option<HitTarget>| {
        lines.push(line);
        hits.push(hit);
    };
    for entry in view.transcript.iter() {
        let entry_id = entry.id();
        out.clear();
        hits.clear();
        match entry {
            Entry::Message { line, .. } => match line.kind {
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
                        push_hit(&mut out, &mut hits, Line::from(spans), None);
                    }
                }
                // §16.2：assistant 无填充卡片，正文为主（Markdown 渲染）。
                LineKind::Assistant => {
                    let rendered = cached_markdown(cache, line, theme);
                    out.extend(rendered.iter().cloned());
                    hits.extend(rendered.iter().map(|_| None));
                }
                // §16.2：thinking dim italic，可折叠（Alt+T）；点击打开原文 Overlay。
                LineKind::Reasoning => {
                    if view.reasoning_visible {
                        for (i, s) in line.text.split('\n').enumerate() {
                            let prefix = if i == 0 { "思考 " } else { "    " };
                            push_hit(
                                &mut out,
                                &mut hits,
                                Line::styled(
                                    format!("{prefix}{s}"),
                                    Style::default()
                                        .fg(theme.muted)
                                        .add_modifier(Modifier::ITALIC),
                                ),
                                Some(HitTarget::Reasoning(entry_id)),
                            );
                        }
                    } else {
                        push_hit(
                            &mut out,
                            &mut hits,
                            Line::styled(
                                "◇ 思考 · 已折叠（Alt+T 展开 · 点击查看）",
                                Style::default()
                                    .fg(theme.muted)
                                    .add_modifier(Modifier::ITALIC),
                            ),
                            Some(HitTarget::Reasoning(entry_id)),
                        );
                    }
                }
                LineKind::Tool => {
                    for (i, s) in line.text.split('\n').enumerate() {
                        let prefix = if i == 0 { "工具 " } else { "    " };
                        push_hit(
                            &mut out,
                            &mut hits,
                            Line::styled(format!("{prefix}{s}"), Style::default().fg(theme.info)),
                            None,
                        );
                    }
                }
                LineKind::System => {
                    // PM：纯分隔线按终端宽度铺满（此前固定 40 个 ─，窄屏折行、宽屏过短）。
                    if !line.text.is_empty() && line.text.chars().all(|c| c == '─') {
                        push_hit(
                            &mut out,
                            &mut hits,
                            Line::styled("─".repeat(width), Style::default().fg(theme.warning)),
                            None,
                        );
                    } else {
                        for (i, s) in line.text.split('\n').enumerate() {
                            let prefix = if i == 0 { "系统 " } else { "    " };
                            push_hit(
                                &mut out,
                                &mut hits,
                                Line::styled(
                                    format!("{prefix}{s}"),
                                    Style::default().fg(theme.warning),
                                ),
                                None,
                            );
                        }
                    }
                }
            },
            Entry::Tool { card, .. } => {
                let card_id = card.id.clone();
                for line in tool_card_lines(card, view.anim_tick, theme, width) {
                    push_hit(
                        &mut out,
                        &mut hits,
                        line,
                        Some(HitTarget::Tool(card_id.clone())),
                    );
                }
            }
        }
        groups.push((
            entry_id,
            std::mem::take(&mut out),
            std::mem::take(&mut hits),
        ));
    }
    // TUI v2 §7.2：live 区（流式 assistant/reasoning + 运行中工具）作为
    // 最后一个 group（哨兵 id；Follow 时显示在尾部，Locked 锚定不到它）。
    build_live_group(view, theme, cache, width, &mut out, &mut hits, &mut groups);
    groups
}

/// 渲染 live 区（§7.2）：reasoning（折叠策略同历史）→ assistant（Markdown）
/// → 运行中工具卡片（按启动顺序）。
fn build_live_group(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<u64, Vec<Line<'static>>>,
    width: usize,
    out: &mut Vec<Line<'static>>,
    hits: &mut Vec<Option<HitTarget>>,
    groups: &mut Vec<EntryGroup>,
) {
    let live = &view.live;
    if live.reasoning.is_none() && live.assistant.is_none() && live.tools.is_empty() {
        return;
    }
    out.clear();
    hits.clear();
    // 流式 reasoning（折叠策略与历史一致：Alt+T 展开 / 点击打开 Overlay）。
    if let Some(msg) = &live.reasoning
        && !msg.text.is_empty()
    {
        if view.reasoning_visible {
            for (i, s) in msg.text.split('\n').enumerate() {
                let prefix = if i == 0 { "思考 " } else { "    " };
                out.push(Line::styled(
                    format!("{prefix}{s}"),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                ));
                hits.push(None);
            }
        } else {
            out.push(Line::styled(
                "◇ 思考 · 流式中…（Alt+T 展开 · 点击查看）",
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::ITALIC),
            ));
            hits.push(None);
        }
    }
    // 流式 assistant（Markdown，按 version 缓存）。
    if let Some(msg) = &live.assistant
        && !msg.text.is_empty()
    {
        let rendered = if let Some(lines) = cache.get(&msg.version) {
            lines.clone()
        } else {
            let lines = render_markdown(&msg.text, theme);
            if cache.len() > 2048 {
                cache.clear();
            }
            cache.insert(msg.version, lines);
            cache[&msg.version].clone()
        };
        out.extend(rendered.iter().cloned());
        hits.extend(rendered.iter().map(|_| None));
    }
    // 运行中工具卡片（按启动顺序）。
    for call_id in &live.tool_order {
        if let Some(card) = live.tools.get(call_id) {
            let card_id = card.id.clone();
            for line in tool_card_lines(card, view.anim_tick, theme, width) {
                out.push(line);
                hits.push(Some(HitTarget::Tool(card_id.clone())));
            }
        }
    }
    groups.push((
        // 哨兵 id：Locked 锚定不到 live 区（§16：live 只在 Follow 尾部可见）。
        EntryId(u64::MAX),
        std::mem::take(out),
        std::mem::take(hits),
    ));
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

/// 工具卡片主行（整改 A2/A3）：`icon name target…  metadata` 恒为单个 visual line。
///
/// - icon/status 语义色；name 正常亮度 BOLD；target muted（display-width ellipsis）；
/// - metadata（duration · exit）最右，muted；
/// - 折叠态：失败 tail ≤4 行、运行中实时输出 ≤3 行；成功无正文；
/// - 展开态（active 卡片）：完整输出内联（scrollback 卡片走 overlay，Phase B）。
fn tool_card_lines(
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
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        name,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    ));
    if !target.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            target,
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        ));
    }
    if !meta.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(meta, Style::default().fg(theme.muted)));
    }
    let mut lines = vec![Line::from(spans)];

    // 正文（有严格行数预算）：展开态完整输出 / 运行中实时尾注 / 失败 tail。
    let show_body = if card.expanded {
        card.output.as_deref()
    } else if let ToolCardState::Running = card.state {
        card.output.as_deref() // 运行中的实时输出（折叠态显示，最多 3 行）
    } else {
        None
    };
    if let Some(body) = show_body {
        let body_lines: Vec<&str> = body.split('\n').collect();
        let shown: Vec<&str> = if card.expanded {
            body_lines
        } else {
            // 整改 A3：折叠态实时输出只保留最后 3 行。
            body_lines.iter().rev().take(3).copied().collect::<Vec<_>>()
        };
        for s in shown {
            let style = if card.expanded {
                Style::default().fg(theme.text)
            } else {
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM)
            };
            lines.push(Line::styled(format!("│ {s}"), style));
        }
        if card.output_truncated && card.expanded {
            lines.push(Line::styled(
                "│ …（输出超预算被截断；完整内容可通过 read @artifact 读取）",
                Style::default().fg(theme.muted),
            ));
        }
    }
    if !card.expanded
        && let Some(tail) = &card.tail
    {
        // 整改 A3：失败 tail 最多 4 行（取尾部——错误诊断在输出末尾）。
        let tail_lines: Vec<&str> = tail.split('\n').collect();
        let last: Vec<&str> = tail_lines.iter().rev().take(4).copied().collect();
        for s in last.into_iter().rev() {
            lines.push(Line::styled(
                format!("│ {s}"),
                Style::default().fg(theme.error).add_modifier(Modifier::DIM),
            ));
        }
    }
    lines
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
    mode: terminal::ViewMode,
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

    // 优先级：状态 → 排队/新内容提示 → ctx → tokens → 兼容模式（窄屏先保提示，不被 ctx/tokens 挤掉）。
    if view.pending_queue_len > 0 {
        spans.push(Span::styled(
            format!(" · 已排队 {} 条消息", view.pending_queue_len),
            Style::default().fg(theme.warning),
        ));
    }
    if view.pending_below > 0 {
        spans.push(Span::styled(
            format!(" · ↓{} 条新内容 · Ctrl+End 返回最新", view.pending_below),
            Style::default().fg(theme.warning),
        ));
    }
    // 整改 D：footer 固定顺序 workspace · model · state · ctx · tokens · 提示。
    // 上下文用量条（§对比：gemini-cli ContextUsageDisplay；projected/usable）。
    if let Some((projected, usable)) = view.context_usage
        && usable > 0
    {
        let ratio = projected as f64 / usable as f64;
        let filled = ((ratio * 20.0) as usize).clamp(0, 20);
        let bar: String = "█".repeat(filled) + &"░".repeat(20 - filled);
        let style = if ratio >= 0.9 {
            theme.error
        } else if ratio >= 0.7 {
            theme.warning
        } else {
            theme.info
        };
        spans.push(Span::styled(
            format!(" · ctx {}% {bar}", (ratio * 100.0) as u64),
            style,
        ));
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
    // 兼容模式提示：仅 inline 且 scrollback 不可用时显示（fullscreen 正常模式）。
    if !scrollback && mode == terminal::ViewMode::Inline {
        spans.push(Span::styled(
            " · 兼容模式（无滚动回退）",
            Style::default().fg(theme.warning),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// §24：全屏历史垂直 scrollbar——1 列，thumb 比例按 visual 行数（不是 entry 数）。
/// 内容不超一屏时画空轨道（布局稳定，不随内容增长跳变）。
fn draw_scrollbar(
    frame: &mut ratatui::Frame,
    rect: Rect,
    theme: theme::Theme,
    window_start: usize,
    total_rows: usize,
) {
    let area_h = rect.height.max(1) as usize;
    let mut glyphs: Vec<&'static str> = vec!["│"; area_h];
    if total_rows > area_h {
        let thumb_h =
            (((area_h as u64 * area_h as u64) / total_rows.max(1) as u64).max(1)) as usize;
        let max_start = total_rows - area_h;
        let top = if max_start == 0 {
            0
        } else {
            ((window_start as u64 * (area_h - thumb_h) as u64) / max_start as u64) as usize
        };
        for y in top..(top + thumb_h).min(area_h) {
            glyphs[y] = "▐";
        }
    }
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area_h);
    for g in glyphs {
        let style = if g == "▐" {
            Style::default().fg(theme.info)
        } else {
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM)
        };
        lines.push(Line::styled(g, style));
    }
    frame.render_widget(Paragraph::new(lines), rect);
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
    let rows = wrapped.len().max(1) as u16;
    let (cursor_row, cursor_col) = input_cursor_cell(&view.input, view.input_cursor, area.width);
    // BUG-008：滚动基准必须是可见区域高度（area.height），而不是全部折行行数
    // （rows）——否则长输入（>8 行）时光标会被放到输入区之外、不可见。
    let scroll_rows = input_scroll_offset(cursor_row, rows, area.height);
    frame.render_widget(Paragraph::new(wrapped).scroll((scroll_rows, 0)), area);
    let y = area.y + (cursor_row - scroll_rows);
    let x = area.x + cursor_col;
    // 光标可见性：ratatui 每帧 set_cursor_position 都会 show_cursor，导致模型输出期间光标一直闪烁。
    // 运行中且输入为空时隐藏（用户主要在看输出；一旦开始输入（排队）就恢复显示）。
    if should_show_input_cursor(
        &view.status,
        view.input.is_empty(),
        view.modal.is_some() || view.overlay.is_some(),
    ) {
        frame.set_cursor_position((x, y));
    }
}

/// 输入光标可见性策略（产品规则，可单测）：空闲恒显示；运行中仅在已有输入时显示
/// （正在排队输入需要光标；否则隐藏，避免模型输出期间光标一直闪烁）。
fn should_show_input_cursor(
    status: &StatusLine,
    input_empty: bool,
    overlay_or_modal_open: bool,
) -> bool {
    (matches!(status, StatusLine::Idle) || !input_empty) && !overlay_or_modal_open
}

/// BUG-008：输入区内部滚动的可见基准——保证光标行落在 `area` 内。
/// `total_rows` 是输入全部折行行数（可能大于区域高度，内部滚动）。
fn input_scroll_offset(cursor_row: u16, total_rows: u16, area_height: u16) -> u16 {
    let area_height = area_height.max(1);
    let max_scroll = total_rows.saturating_sub(area_height);
    cursor_row.saturating_sub(area_height - 1).min(max_scroll)
}

/// 输入区所需行数（≤4 行；长输入内部滚动跟随光标）。
fn input_area_rows(view: &ViewModel, width: u16) -> u16 {
    if view.input.is_empty() {
        return 1;
    }
    let budget = width.saturating_sub(2).max(1) as usize;
    let wrapped = wrap_lines(vec![Line::from(Span::raw(view.input.clone()))], budget);
    // §8.2：composer 动态 1..8 行，超过 8 行内部滚动（draw_input 跟随光标）。
    wrapped.len().clamp(1, 8) as u16
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
    // 空输入：wrap 结果为空，循环不会执行——光标必须停在 prompt 右侧（修复“首次打开光标在 ❯ 左边”）。
    if wrapped.is_empty() {
        return (0, PROMPT_WIDTH);
    }
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

/// 详情 Overlay（整改 B）：带边框对话框，展示 command/output/status；
/// Esc 关闭、PgUp/PgDn 内部滚动；不修改 scrollback。
/// 操作型 Modal（§42）：标题 + 正文（内部滚动，Esc 关闭，PgUp/PgDn 翻页）。

/// Modal/Overlay 宽度：优先 ≤88-4，但绝不超过终端宽度-2（窄屏不溢出）。
fn modal_width(area_width: u16) -> u16 {
    let max_w = area_width.saturating_sub(2).max(1);
    let pref = area_width.min(88).saturating_sub(4);
    pref.min(max_w).max(max_w.min(10))
}

/// Modal/Overlay 高度：优先 ≤28-2，但绝不超过转录区高度-2（小终端不溢出）。
fn modal_height(area_height: u16) -> u16 {
    let max_h = area_height.saturating_sub(2).max(1);
    let pref = area_height.min(28).saturating_sub(2);
    pref.min(max_h).max(max_h.min(10))
}
fn draw_modal(frame: &mut ratatui::Frame, rect: Rect, view: &ViewModel, theme: theme::Theme) {
    let Some(modal) = &view.modal else {
        return;
    };
    let inner_w = rect.width.saturating_sub(2).max(1) as usize;
    let inner_h = rect.height.saturating_sub(2).max(1) as usize;

    let mut content: Vec<Line<'static>> = Vec::new();
    content.push(Line::styled(
        format!("{}（Esc 关闭 · ↑/↓ 滚动）", modal.title),
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ));
    content.push(Line::default());
    for line in modal.body.lines() {
        content.push(Line::styled(
            line.to_string(),
            Style::default().fg(theme.text),
        ));
    }
    let wrapped = wrap_lines(content, inner_w);
    let total = wrapped.len();
    let scroll = modal.scroll.min(total.saturating_sub(inner_h) as u16);
    let start = scroll as usize;
    let window = wrapped[start..start + inner_h.min(total)].to_vec();

    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_style(Style::default().fg(theme.info)),
        rect,
    );
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    );
    frame.render_widget(Paragraph::new(window).scroll((0, 0)), inner);
}

fn draw_overlay(frame: &mut ratatui::Frame, rect: Rect, view: &ViewModel, theme: theme::Theme) {
    let Some(overlay) = &view.overlay else {
        return;
    };
    let inner_w = rect.width.saturating_sub(2).max(1) as usize;
    let inner_h = rect.height.saturating_sub(2).max(1) as usize;

    let mut content: Vec<Line<'static>> = Vec::new();
    content.push(Line::styled(
        overlay.title.clone(),
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ));
    content.push(Line::default());
    if let Some(command) = &overlay.command {
        content.push(Line::styled(
            "Command",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ));
        for line in command.split('\n') {
            content.push(Line::styled(
                line.to_string(),
                Style::default().fg(theme.text),
            ));
        }
        content.push(Line::default());
    }
    if !overlay.body.is_empty() {
        content.push(Line::styled(
            "Output",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ));
        for line in overlay.body.split('\n') {
            content.push(Line::styled(
                line.to_string(),
                Style::default().fg(theme.text),
            ));
        }
    } else {
        content.push(Line::styled("（无输出）", Style::default().fg(theme.muted)));
    }
    if overlay.body_truncated {
        content.push(Line::styled(
            "…（输出超预算被截断；完整内容可通过 read @artifact 读取）",
            Style::default().fg(theme.warning),
        ));
    }
    content.push(Line::default());
    content.push(Line::styled(
        "[Esc] 关闭 · [PgUp/PgDn] 滚动",
        Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
    ));

    // 内部滚动（逻辑行按 inner_w 折行后窗口化）。
    let (wrapped, _) = wrap_lines_with_hits(content, Vec::new(), inner_w);
    let total = wrapped.len() as u16;
    let scroll = overlay.scroll.min(total.saturating_sub(inner_h as u16));
    let start = scroll as usize;
    let end = (start + inner_h).min(wrapped.len());
    let window = wrapped[start..end].to_vec();

    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(" Tool details ");
    // 先清空 Overlay 覆盖区域，否则底层 transcript 文字会透过内容间隙显示（用户反馈“思考悬浮窗被其他文字干扰背景”）。
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(window)
            .block(block)
            .scroll((0, 0)),
        rect,
    );
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
        let label = match menu.kind {
            model::MenuKind::SlashCommand => format!("/{name}"),
            model::MenuKind::File | model::MenuKind::Session => name.clone(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} {label}"), style),
            Span::styled(format!("  {desc}"), Style::default().fg(theme.muted)),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), rect);
}

fn fmt_duration(ms: u64) -> String {
    crate::tui::model::fmt_duration(ms)
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
pub fn draw_to_test_backend(
    view: &mut ViewModel,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    draw_to_test_backend_mode(view, width, height, terminal::ViewMode::Inline)
}

/// 用 TestBackend 按指定视口模式渲染一帧（§20.3；fullscreen 验收测试用）。
pub fn draw_to_test_backend_mode(
    view: &mut ViewModel,
    width: u16,
    height: u16,
    mode: terminal::ViewMode,
) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let viewport = match mode {
        terminal::ViewMode::Fullscreen => ratatui::Viewport::Fullscreen,
        terminal::ViewMode::Inline => ratatui::Viewport::Inline(height),
    };
    let mut terminal = match Terminal::with_options(backend, ratatui::TerminalOptions { viewport })
    {
        Ok(terminal) => terminal,
        Err(error) => {
            // 测试基建失败：显式报错退出（不 panic、不 unwrap）。
            tracing::error!(error = %error, "draw_into_buffer: TestBackend 终端初始化失败");
            eprintln!("tpi test helper: terminal init failed: {error}");
            std::process::exit(2);
        }
    };
    let theme = theme::Theme::omp();
    let mut cache: HashMap<u64, Vec<Line<'static>>> = HashMap::new();
    let mut cache_width = 0u16;
    let mut committed = 0usize;
    let scrollback = mode == terminal::ViewMode::Inline;
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                view,
                theme,
                &mut cache,
                &mut cache_width,
                &mut committed,
                scrollback,
                mode,
            );
        })
        .map_err(|error| {
            tracing::error!(error = %error, "draw_into_buffer: 渲染失败（测试基建）");
            eprintln!("tpi test helper: draw failed: {error}");
            std::process::exit(2);
        })
        .ok();
    terminal.backend().buffer().clone()
}

/// 捕获一次 draw 的 stdout 字节（§20.3：验证无全屏清除序列、单次 flush）。
pub fn draw_captured_bytes(view: &mut ViewModel) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    {
        let backend = CrosstermBackend::new(&mut out);
        let mut terminal = match Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(10),
            },
        ) {
            Ok(terminal) => terminal,
            Err(error) => {
                tracing::error!(error = %error, "draw_captured_bytes: 终端初始化失败（测试基建）");
                eprintln!("tpi test helper: terminal init failed: {error}");
                std::process::exit(2);
            }
        };
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
                    terminal::ViewMode::Inline,
                );
                overflow = plan.overflow;
            })
            .map_err(|error| {
                tracing::error!(error = %error, "draw_captured_bytes: 渲染失败（测试基建）");
                eprintln!("tpi test helper: draw failed: {error}");
                std::process::exit(2);
            })
            .ok();
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
        let plan = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
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
            &mut view,
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
        let mut cache = HashMap::new();
        // 先布局一次（Follow 建立 layout_top = 视口顶部行）。
        let _ = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        // TUI v2：翻页 = 锚点（视口顶部）上移 2 行 → 窗口 [4, 8)。
        view.scroll_up(2);
        let plan = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        // Locked：不提交到 scrollback。
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
        // Locked 保持：新内容不移动视口（§58 场景 A）。
        view.push_line(LineKind::Assistant, "line 10 new".to_string());
        let plan3 = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        let window_text: String = plan3
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            !window_text.contains("line 10 new"),
            "Locked 时新输出不得移动视口"
        );
        assert!(view.pending_below >= 1, "Locked 时新内容必须计数");
    }

    #[test]
    fn plan_window_resize_resets_committed_without_overflow() {
        let mut view = ViewModel::default();
        for i in 0..10 {
            view.push_line(LineKind::Assistant, format!("line {i}"));
        }
        let mut cache = HashMap::new();
        // 先提交 6 行。
        let plan = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        assert_eq!(plan.committed_after, 6);
        // resize：不产生 overflow，提交位置重置为窗口起点。
        let plan2 = plan_window(
            &mut view,
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
            name: "bash".into(),
            target: Some("bash: cargo test".into()),
            command: None,
            state: ToolCardState::Running,
            output: None,
            output_truncated: false,
            expanded: false,
            tail: None,
        };
        let lines = tool_card_lines(&card, 0, theme, 100);
        assert!(
            lines[0].spans[0].content.starts_with('⠋'),
            "运行中显示 spinner"
        );
        assert!(lines[0].spans.iter().any(|s| s.content == "bash"));
        // 运行中已展示命令摘要（独立缩进行）。
        assert!(
            lines
                .iter()
                .any(|l| l.to_string().contains("bash: cargo test")),
            "运行中显示命令摘要: {lines:?}"
        );

        let card = ToolCard {
            id: "c1".into(),
            name: "bash".into(),
            target: Some("bash: cargo test".into()),
            command: None,
            state: ToolCardState::Done {
                status: crate::tool::outcome::ToolStatus::Failed,
                duration_ms: 1234,
                exit_code: Some(2),
            },
            output: None,
            output_truncated: false,
            expanded: false,
            tail: Some("exit_code: 1".into()),
        };
        let lines = tool_card_lines(&card, 0, theme, 100);
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

    /// BUG-008：长输入（折行数 > 输入区高度）时光标行必须留在输入区内。
    #[test]
    fn input_scroll_keeps_cursor_inside_visible_area() {
        // 10 行输入、4 行区域：光标在最后一行（row 9）→ 滚动 6 行，
        // 光标 y 落在区域第 4 行（area.y + 3），而不是区域外。
        let offset = input_scroll_offset(9, 10, 4);
        assert_eq!(offset, 6);
        assert!(9 - offset < 4, "光标必须落在 4 行区域内");
        // 光标在顶部时不滚动。
        assert_eq!(input_scroll_offset(0, 10, 4), 0);
        assert_eq!(input_scroll_offset(2, 10, 4), 0);
        // 输入行数小于区域高度：不滚动。
        assert_eq!(input_scroll_offset(2, 3, 8), 0);
        // 极端：区域高度 0 不 panic（clamp 到 1）。
        assert_eq!(input_scroll_offset(5, 10, 0), 5);
    }

    /// PM：Modal/Overlay 尺寸在窄屏/小终端下不溢出（此前 max(40)/max(10) 会超屏）。
    #[test]
    fn modal_size_clamps_to_terminal() {
        // 常规：优先 88-4=84，但受 width-2 限制。
        assert_eq!(modal_width(80), 76);
        assert_eq!(modal_width(120), 84);
        // 窄屏：不超过 width-2。
        assert_eq!(modal_width(30), 26);
        assert_eq!(modal_width(20), 16);
        assert_eq!(modal_width(10), 8);
        assert!(modal_width(40) <= 38);
        // 高度：不超过 trans_area.height-2。
        assert_eq!(modal_height(24), 22);
        assert_eq!(modal_height(10), 8);
        assert_eq!(modal_height(40), 26);
    }
    #[test]
    fn activity_height_scales_with_terminal_rows() {
        // 小终端：2/5 屏不足下限 → 12 行。
        assert_eq!(activity_height(24), 12);
        // 常规终端：2/5 屏。
        assert_eq!(activity_height(50), 20);
        assert_eq!(activity_height(80), 32);
        // 大终端：不再被 32 上限卡住，自动拓展（上限 rows-12）。
        assert_eq!(activity_height(100), 40);
        assert_eq!(activity_height(120), 48);
        // 极端小终端：保底 12（不 panic）。
        assert_eq!(activity_height(12), 12);
        assert_eq!(activity_height(15), 12);
    }

    #[test]
    fn spinner_frames_advance_with_anim_tick() {
        let theme = theme::Theme::omp();
        let card = ToolCard {
            id: "c".into(),
            name: "bash".into(),
            target: None,
            command: None,
            state: ToolCardState::Running,
            output: None,
            output_truncated: false,
            expanded: false,
            tail: None,
        };
        let f0 = tool_card_lines(&card, 0, theme, 100).remove(0);
        let f1 = tool_card_lines(&card, 1, theme, 100).remove(0);
        assert_ne!(
            f0.spans[0].content, f1.spans[0].content,
            "动画帧应随 tick 变化"
        );
    }

    #[test]
    fn running_card_shows_live_output_tail() {
        let theme = theme::Theme::omp();
        let card = ToolCard {
            id: "c".into(),
            name: "bash".into(),
            target: Some("bash: cargo test".into()),
            command: None,
            state: ToolCardState::Running,
            output: Some(
                "progress 1\nprogress 2\nprogress 3\nprogress 4\nprogress 5\nprogress 6\nprogress 7\n"
                    .into(),
            ),
            output_truncated: false,
            expanded: false,
            tail: None,
        };
        let lines = tool_card_lines(&card, 0, theme, 100);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!text.contains("progress 1"), "折叠态只显示最近输出: {text}");
        assert!(text.contains("progress 7"), "最新输出可见: {text}");
        assert!(
            !text.contains("progress 5") || !text.contains("progress 4"),
            "折叠态实时输出最多 3 行（progress 1-4 不应全部可见）: {text}"
        );
    }

    #[test]
    fn expanded_card_shows_full_output() {
        let theme = theme::Theme::omp();
        let card = ToolCard {
            id: "c".into(),
            name: "bash".into(),
            target: None,
            command: None,
            state: ToolCardState::Done {
                status: crate::tool::outcome::ToolStatus::Succeeded,
                duration_ms: 10,
                exit_code: Some(0),
            },
            output: Some("第一行\n第二行\n第三行\n".into()),
            output_truncated: false,
            expanded: true,
            tail: None,
        };
        let lines = tool_card_lines(&card, 0, theme, 100);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            text.contains("第一行") && text.contains("第三行"),
            "展开显示全部输出: {text}"
        );
        assert!(text.contains("✓"), "成功状态图标");
    }
}

/// 修复：空输入时光标必须停在 prompt 右侧（此前 wrap 为空 → (0,0)，光标跑到 ❯ 左边）。
#[test]
fn input_cursor_empty_input_sits_right_of_prompt() {
    assert_eq!(input_cursor_cell("", 0, 80), (0, 2));
    assert_eq!(input_cursor_cell("", 0, 10), (0, 2));
    assert_eq!(input_cursor_cell("", 0, 1), (0, 2));
}

/// 修复：Overlay 必须先 Clear 覆盖区，否则底层 transcript 文字透出（背景干扰）。
#[test]
fn overlay_clears_background_before_rendering() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::Assistant, "背景文字X".repeat(4));
    }
    view.begin_tool("c", "bash", Some("cmd".into()), None);
    view.finish_tool(
        "c",
        "bash",
        crate::tool::outcome::ToolStatus::Failed,
        1,
        Some(1),
        "err",
    );
    view.open_tool_overlay(String::from("c"));
    let buf = draw_to_test_backend(&mut view, 80, 24);
    // overlay 居中 (2,1,76,20)；内部底部行 (40,19) 不在内容上 → 必须被清空为空格。
    assert_eq!(
        buf[(40, 19)].symbol(),
        " ",
        "Overlay 内部未覆盖区域必须清空，不得透出背景文字"
    );
}

/// 修复：模型输出（Running + 空输入）期间隐藏光标，避免一直闪烁；空闲时显示。
#[test]
fn cursor_hidden_during_run_when_input_empty() {
    // 产品规则：空闲恒显示；运行中仅输入非空时显示；Modal/Overlay 打开时隐藏。
    assert!(should_show_input_cursor(&StatusLine::Idle, true, false));
    assert!(should_show_input_cursor(&StatusLine::Idle, false, false));
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            turn: 1,
            tool: "x".into()
        },
        true,
        false
    ));
    assert!(should_show_input_cursor(
        &StatusLine::Running {
            turn: 1,
            tool: "x".into()
        },
        false,
        false
    ));
    assert!(
        !should_show_input_cursor(&StatusLine::Idle, true, true),
        "Modal/Overlay 打开时隐藏输入光标"
    );
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            turn: 1,
            tool: "x".into()
        },
        false,
        true
    ));

    // 字节级：运行中 + 空输入的帧必须发出隐藏序列（绘制期间不得 set_cursor_position）。
    let mut view = ViewModel::default();
    view.status = StatusLine::Running {
        turn: 1,
        tool: "模型生成中".into(),
    };
    view.input = String::new();
    let s = String::from_utf8_lossy(&draw_captured_bytes(&mut view)).into_owned();
    assert!(s.contains("\x1b[?25l"), "运行中必须隐藏光标: {s:?}");

    // 空闲帧必须显示光标。
    let mut view2 = ViewModel::default();
    view2.status = StatusLine::Idle;
    let s2 = String::from_utf8_lossy(&draw_captured_bytes(&mut view2)).into_owned();
    assert!(s2.contains("\x1b[?25h"), "空闲必须显示光标: {s2:?}");
}
