//! TUI 渲染层。
//!
//! 只有 renderer 可以调用 Crossterm/Ratatui 或写 stdout；Agent、provider、tool
//! 和日志模块只能发送事件。
//!
//! 对标成熟终端 Agent（Claude Code/OpenCode 式）：
//! - inline viewport 保留终端 scrollback，闭合行经 `insert_before`
//!   提交到活动区上方，底部只重绘变化内容；不支持时降级为活动区内部滚动
//!   （状态栏显示兼容模式说明）。
//! - 信息层级：用户消息细紫红左 rail + `you` 标签；assistant 无填充卡片；
//!   thinking dim italic 可折叠（Alt+T）；工具调用单行卡片
//!   `icon name duration status`（运行中 spinner 动画，失败保留红色关键 tail）；
//!   plan 独立紧凑区域；footer 展示 workspace/model/usage/状态；编辑器硬件光标。
//! - OMP 语义主题集中在 `theme` 模块。
//! - Markdown 渲染（pulldown-cmark）：assistant/用户消息的加粗、行内代码、
//!   代码块、列表、引用、链接；按条目版本缓存渲染结果，流式增量只失效最后一条。

pub mod editor;
pub mod effect;
pub mod event;
pub mod interaction;
mod markdown;
pub mod model;
pub mod reducer;
pub mod scroll;
pub mod state;
pub mod terminal;
pub mod text;
pub mod theme;
mod tool_card;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use std::collections::HashMap;
use std::time::Instant;

use markdown::render_markdown;
use model::{Entry, LineKind, StatusLine, TranscriptLine, ViewModel};
#[cfg(test)]
use model::{ToolCard, ToolCardState};
use scroll::{EntryId, ScrollMode};
use tool_card::{card_semantic_content, card_semantic_header, tool_card_lines};
#[cfg(test)]
use tool_card::{render_diff_lines, tool_name_style};

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
    // “/ + 回车”默认选中第一项：首项必须是安全命令（help），
    // 不能是 quit，否则探索菜单时输“/”再回车会直接退出。
    ("help", "显示帮助与快捷键"),
    ("settings", "查看生效配置及来源"),
    ("model", "查看当前模型"),
    ("session", "查看会话与成本"),
    ("sessions", "浏览并恢复历史会话"),
    ("new", "开始新会话"),
    ("cancel", "取消当前 run"),
    ("thinking", "查看推理设置"),
    ("diff", "查看本轮全部文件 diff"),
    ("doctor", "环境检查（config/模型/API key/Git Bash）"),
    ("compact", "手动压缩上下文"),
    ("retry", "重试上一次失败/中断的 turn"),
    ("quit", "退出 TPI"),
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
    /// 窗口内每行对应的可点击目标 + 列上限（鼠标点击展开；消息行为 None）。
    row_hits: Vec<Option<(HitTarget, u16)>>,
    /// 转录区屏幕矩形（鼠标 hit-test）。
    transcript_rect: Rect,
    /// scrollbar 屏幕矩形（fullscreen 1 列；§24）。
    scrollbar_rect: Option<Rect>,
    /// 当前窗口起始全局行（scrollbar 比例用，§24）。
    window_start: usize,
    /// 内容总 visual 行数（scrollbar 比例用，§24）。
    total_rows: usize,
    /// 窗口内每行的语义文本（复制源；与 window 等长）。§InteractionRefactor：
    /// copy 从语义文本提取，不反推渲染结果。每行记录 (entry, 该行在 entry
    /// 语义流中的起始 char 偏移, 语义文本)——hit-test 与复制共用。
    semantic_rows: Vec<Option<(EntryId, usize, String, usize)>>,
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
    md_cache: HashMap<(u64, u16), Vec<Line<'static>>>,
    /// 缓存有效时的终端宽度；宽度变化清空缓存并重置提交位置（§16.1）。
    cache_width: u16,
    /// 最近一帧的转录区矩形（鼠标 hit-test 用）。
    last_transcript_rect: Option<Rect>,
    /// 最近一帧窗口内每行对应的可点击目标 + 可点击列上限
    /// （鼠标点击展开用；主行限制在图标+工具名区域，避免整行误触）。
    last_row_hits: Vec<Option<(HitTarget, u16)>>,
    /// 已提交到 scrollback 的行数（折叠/展开状态变化时 hit 失效，置空）。
    hits_valid: bool,
    /// 最近一帧 scrollbar 矩形（§24 鼠标点击/拖拽 hit-test）。
    last_scrollbar_rect: Option<Rect>,
    /// 最近一帧窗口内每行的语义文本（§InteractionRefactor：Ctrl+C 复制源，
    /// 从语义文本提取，不反推渲染结果）。与窗口行一一对应；每行记录
    /// (entry, 起始 char 偏移, 语义文本)（语义位置 hit-test 用）。
    semantic_rows: Vec<Option<(EntryId, usize, String, usize)>>,
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
            semantic_rows: Vec::new(),
        })
    }

    /// 鼠标点击 hit-test：命中工具卡片/reasoning 行返回目标（Overlay 用）。
    /// 列必须落在该行的可点击范围内（§24：主行限图标+工具名区域，避免误触）。
    pub fn hit_target(&self, column: u16, row: u16) -> Option<HitTarget> {
        if !self.hits_valid {
            return None;
        }
        let rect = self.last_transcript_rect?;
        if column < rect.x || row < rect.y || row >= rect.y + rect.height {
            return None;
        }
        let index = (row - rect.y) as usize;
        let (target, end_col) = self.last_row_hits.get(index).cloned().flatten()?;
        // 列必须是可点击区域内（含 0..=end_col；end_col==0 表示该行不可点）。
        let col = column.saturating_sub(rect.x);
        if end_col == 0 || col > end_col {
            return None;
        }
        Some(target)
    }
    /// §24：最近一帧 scrollbar 矩形（鼠标点击/拖拽 hit-test；app 层优先判断）。
    pub fn scrollbar_rect(&self) -> Option<Rect> {
        self.last_scrollbar_rect
    }

    /// 最近一帧转录区矩形（§用户诉求：拖动选择基于转录区行号）。
    pub fn transcript_rect(&self) -> Option<Rect> {
        self.last_transcript_rect
    }

    /// 屏幕坐标 → 语义位置（§PointerHit：hit-test 从布局映射得到
    /// entry + 逻辑 char 偏移；CJK 按 cell 宽度精确定位，扣 rail 前缀）。
    pub fn hit_text(&self, column: u16, row: u16) -> Option<crate::tui::interaction::TextPosition> {
        let rect = self.last_transcript_rect?;
        if column < rect.x || row < rect.y || row >= rect.y + rect.height {
            return None;
        }
        let index = (row - rect.y) as usize;
        let (entry_id, char_start, text, decor) = self.semantic_rows.get(index)?.as_ref()?;
        // 屏幕列 → 语义内 cell 列：先扣 rail/icon 前缀宽度（§PointerHit：`│ AI`
        // 等前缀不参与语义偏移），再按 cell 宽度映射 char（CJK/emoji 精确）。
        let col = column.saturating_sub(rect.x) as usize;
        let semantic_col = col.saturating_sub(*decor);
        let char_off = crate::tui::interaction::cell_to_char(text, semantic_col);
        Some(crate::tui::interaction::TextPosition {
            entry_id: *entry_id,
            offset: *char_start + char_off,
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
                semantic_rows,
                overflow: frame_overflow,
                committed_after,
                ..
            } = render_frame(
                frame,
                view,
                theme,
                RenderContext {
                    markdown_cache: &mut cache,
                    cache_width: &mut cache_width,
                    committed: &mut committed,
                    scrollback,
                    mode,
                },
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
                semantic_rows,
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
            // §InteractionRefactor：保存每窗口行的语义文本（复制源）。
            // 从语义文本提取复制，不反推渲染结果（无 padding/装饰污染）。
            self.semantic_rows = plan.semantic_rows;
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
struct RenderContext<'a> {
    markdown_cache: &'a mut HashMap<(u64, u16), Vec<Line<'static>>>,
    cache_width: &'a mut u16,
    committed: &'a mut usize,
    scrollback: bool,
    mode: terminal::ViewMode,
}

fn render_frame(
    frame: &mut ratatui::Frame,
    view: &mut ViewModel,
    theme: theme::Theme,
    context: RenderContext<'_>,
) -> FramePlan {
    let RenderContext {
        markdown_cache: cache,
        cache_width,
        committed,
        scrollback,
        mode,
    } = context;
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

    // §视觉瘦身：去掉常驻 Header（信息与 footer 重复），transcript 上移到顶部。
    // 自下而上 = footer(1) → input(1..8) → plan(0..N) → transcript(Min)。
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

    // §视觉瘦身：scrollbar 预留 1 列（inline 兼容模式不预留）。
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
        let w = area.width.clamp(1, 56);
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
        semantic_rows: plan.semantic_rows,
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
    cache: &mut HashMap<(u64, u16), Vec<Line<'static>>>,
) -> FramePlan {
    let width = width.max(1) as usize;
    // 按 entry 构建逻辑行（entry → wrapped 行 + hits + 语义文本）。
    // ids/heights 必须以 wrapped_by_entry 为准（含 live 哨兵 group，§7.2）。
    let per_entry = build_transcript_text(view, theme, cache, width);
    let mut wrapped_by_entry: Vec<(EntryId, Vec<WrappedRow>)> = Vec::with_capacity(per_entry.len());
    for (id, logical, hits, semantic_lines) in per_entry {
        let rows = wrap_with_semantic(logical, hits, &semantic_lines, width, id);
        wrapped_by_entry.push((id, rows));
    }
    let ids: Vec<EntryId> = wrapped_by_entry.iter().map(|(id, _)| *id).collect();
    let heights: Vec<usize> = wrapped_by_entry
        .iter()
        .map(|(_, rows)| rows.len())
        .collect();
    // 写回高度表（滚动跨 entry 定位用；§4）。
    for (id, height) in ids.iter().zip(heights.iter()) {
        view.entry_heights.insert(*id, *height);
    }

    let area_h = area_h.max(1) as usize;
    let mut window_start =
        crate::tui::scroll::window_start_row(&ids, &heights, &view.scroll_mode, area_h);
    // §PointerHit：Follow 模式、无 live 流式内容时，若最后一个 transcript entry
    // 是完成态工具卡片且高度超过视口，锚定到卡片主行（状态/exit code 可见），
    // 避免主行被 `total - area_h` 的底部对齐滚出视口。
    if view.scroll_mode == ScrollMode::Follow
        && view.live.reasoning.is_none()
        && view.live.assistant.is_none()
        && ids.last().is_some_and(|last_id| {
            matches!(view.transcript.last(), Some(Entry::Tool { .. }))
                && *last_id == view.transcript.last().map(Entry::id).unwrap_or(EntryId(0))
        })
    {
        let last_h = heights.last().copied().unwrap_or(0);
        if last_h > area_h {
            let total: usize = heights.iter().sum();
            window_start = total.saturating_sub(last_h);
        }
    }
    // 写回布局结果：视口顶部行 + 视口高度（renderer 写回，滚动基础）。
    view.layout_top = Some(crate::tui::scroll::locate_row(&ids, &heights, window_start));
    view.transcript_rows = area_h as u16;

    // 按全局行切片：逐 entry 取窗口内的行。
    let mut window: Vec<Line<'static>> = Vec::new();
    let mut row_hits: Vec<Option<HitRange>> = Vec::new();
    // 每窗口行对应的语义信息（复制源；与 window 等长）。§PointerHit：
    // 一次 layout 同步折行语义（wrap_with_semantic 已产出），直接收集。
    let mut semantic_rows: Vec<Option<(EntryId, usize, String, usize)>> = Vec::new();
    let mut cursor = 0usize;
    for (_entry_id, rows) in &wrapped_by_entry {
        let start = cursor;
        let end = cursor + rows.len();
        cursor = end;
        if end <= window_start || start >= window_start + area_h {
            continue;
        }
        let from = window_start.saturating_sub(start);
        let to = (window_start + area_h - start).min(rows.len());
        for row in &rows[from..to] {
            window.push(row.line.clone());
            row_hits.push(row.hit.clone());
            semantic_rows.push(row.semantic.clone());
        }
    }
    // §用户诉求：卡片整行背景填满——有背景色（卡片/surface 或 diff 红绿）的
    // 行补空格到满宽，避免「只有文字处有背景」的碎片感。
    for line in window.iter_mut() {
        let bg = line.spans.iter().find_map(|s| s.style.bg);
        if let Some(color) = bg {
            let cur = unicode_width::UnicodeWidthStr::width(
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .as_str(),
            );
            if cur < width {
                let pad = width - cur;
                line.spans
                    .push(Span::styled(" ".repeat(pad), Style::default().bg(color)));
            }
        }
    }
    let (overflow, committed_after) = if reset_committed {
        (Vec::new(), window_start)
    } else if view.scroll_mode == ScrollMode::Follow && window_start > committed {
        // 跟随模式：窗口之上的行已闭合，提交到 scrollback。
        let mut overflow_lines: Vec<Line<'static>> = Vec::new();
        let mut collected = 0usize;
        for (_, rows) in &wrapped_by_entry {
            let start = collected;
            let end = collected + rows.len();
            collected = end;
            if end <= committed || start >= window_start {
                continue;
            }
            let from = committed.saturating_sub(start);
            let to = (window_start - start).min(rows.len());
            for row in &rows[from..to] {
                overflow_lines.push(row.line.clone());
            }
        }
        (overflow_lines, window_start)
    } else {
        (Vec::new(), committed)
    };
    // §PointerHit：应用内选择高亮——语义选区（entry + 逻辑偏移）投影回视觉
    // 行的精确 cell 范围（不再整行 REVERSED，修复「选中区域视觉比复制内容宽」）。
    if let Some(sel) = &view.selection {
        let selected = Style::default()
            .bg(theme.surface)
            .add_modifier(Modifier::REVERSED);
        let (lo, hi) = sel.normalized();
        for (i, row) in semantic_rows.iter().enumerate() {
            let Some((entry_id, char_start, text, decor)) = row else {
                continue;
            };
            let row_lo = *char_start;
            let row_hi = *char_start + text.chars().count();
            let row_entry = *entry_id;
            if row_entry < lo.entry_id || row_entry > hi.entry_id {
                continue;
            }
            // 行语义范围与选区偏移的交集（char 范围）。
            let (sel_lo, sel_hi) = if row_entry == lo.entry_id && row_entry == hi.entry_id {
                (lo.offset, hi.offset)
            } else if row_entry == lo.entry_id {
                (lo.offset, usize::MAX)
            } else if row_entry == hi.entry_id {
                (usize::MIN, hi.offset)
            } else {
                (usize::MIN, usize::MAX)
            };
            let from_char = row_lo.max(sel_lo).min(row_hi);
            let to_char = row_hi.min(sel_hi).min(row_hi);
            if from_char >= to_char {
                continue;
            }
            // char 交集 → 语义 cell 范围 → 加装饰前缀得到视觉 cell 范围。
            let from_sel = from_char - row_lo;
            let to_sel = to_char - row_lo;
            let cell_from = crate::tui::interaction::chars_to_cells(text, from_sel) + *decor;
            let cell_to = crate::tui::interaction::chars_to_cells(text, to_sel) + *decor;
            if let Some(line) = window.get_mut(i) {
                // §PointerHit invariant：selection 只能改 style，绝不能改文本/
                // 宽度/换行/hit 区域/semantic mapping。逐 span 拆三段
                // before/selected/after——不丢任何字符。
                let mut out_spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
                let mut cell_cursor = 0usize;
                for span in &line.spans {
                    let span_w = unicode_width::UnicodeWidthStr::width(span.content.as_ref());
                    let span_from = cell_cursor;
                    let span_to = cell_cursor + span_w;
                    cell_cursor = span_to;
                    if span_to <= cell_from || span_from >= cell_to {
                        // 完全不相交：原样保留。
                        out_spans.push(span.clone());
                        continue;
                    }
                    // 计算选中 cell 区间与 span 的交集。
                    let hit_from = cell_from.saturating_sub(span_from);
                    let hit_to = cell_to.saturating_sub(span_from).min(span_w);
                    // 拆三段：before（未选中前缀）、selected、after（未选中后缀）。
                    let mut before = String::new();
                    let mut sel = String::new();
                    let mut after = String::new();
                    let mut w = 0usize;
                    for ch in span.content.chars() {
                        let cw = crate::tui::text::char_cell_width(ch);
                        let start = w;
                        w += cw;
                        if start < hit_from {
                            before.push(ch);
                        } else if start < hit_to {
                            sel.push(ch);
                        } else {
                            after.push(ch);
                        }
                    }
                    if !before.is_empty() {
                        out_spans.push(Span::styled(before, span.style));
                    }
                    if !sel.is_empty() {
                        out_spans.push(Span::styled(sel, selected));
                    }
                    if !after.is_empty() {
                        out_spans.push(Span::styled(after, span.style));
                    }
                }
                *line = Line::from(out_spans);
            }
        }
    }
    FramePlan {
        window,
        row_hits,
        // plan_window 不感知屏幕坐标；transcript_rect 由 render_frame 覆盖。
        transcript_rect: Rect::default(),
        // §24：scrollbar 矩形由 render_frame 计算（需要屏幕坐标）。
        scrollbar_rect: None,
        window_start,
        total_rows: heights.iter().sum(),
        semantic_rows,
        overflow,
        committed_after,
    }
}

/// 一次 wrap 的视觉行：line + hit + 语义映射（§PointerHit：视觉换行处
/// 即语义映射断点，不再进行第二次 semantic wrap）。
#[derive(Debug, Clone)]
struct WrappedRow {
    line: Line<'static>,
    hit: Option<HitRange>,
    /// 该视觉行的语义信息：None = 纯装饰/空行（不可选）。
    semantic: Option<(EntryId, usize, String, usize)>,
}

/// 一次折行：视觉行 + 语义映射同步产出（§PointerHit ④）。
///
/// 对每个逻辑行：decor 前缀（前 decor_cells cell）不产生语义内容；内容字符
/// 折行时同时推进 semantic 文本。视觉行首行带 decor，续行 decor=0。
/// 语义行内 char 偏移 = 该逻辑行在 entry 语义流中的累计起始偏移 + 行内游标。
///
/// 关键不变量：视觉行数 == semantic 行数（同一 layout 的产物，绝不二次折行）。
fn wrap_with_semantic(
    lines: Vec<Line<'static>>,
    hits: Vec<Option<HitRange>>,
    semantic_lines: &[SemanticLine],
    width: usize,
    entry_id: EntryId,
) -> Vec<WrappedRow> {
    let width = width.max(1);
    let mut out: Vec<WrappedRow> = Vec::new();
    // 当前累积的视觉行（span 与 cell 宽度）。
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut cur_hit: Option<HitRange> = None;
    // 当前视觉行的语义累积。
    let mut cur_sem_start: Option<usize> = None;
    let mut cur_sem_text = String::new();
    let mut cur_decor = 0usize;
    // entry 语义流内累计 char 偏移。
    let mut entry_offset = 0usize;

    let mut hit_iter = hits.into_iter();
    for (line_idx, line) in lines.into_iter().enumerate() {
        let hit = hit_iter.next().flatten();
        let semantic = semantic_lines.get(line_idx);
        let decor = semantic.map(|s| s.decor_cells).unwrap_or(0);
        let line_start = entry_offset;
        let mut line_consumed = 0usize;
        // 逻辑行内 cell 位置（判断是否越过 decor；只在逻辑行首段有效）。
        let mut line_cell = 0usize;
        // 首段含 decor，换行后为续行（无 decor）。
        let mut is_line_start = true;

        if line.spans.is_empty() {
            // 空行：不进入 cur，直接产出一行。
            if !cur.is_empty() {
                out.push(WrappedRow {
                    line: Line::from(std::mem::take(&mut cur)),
                    hit: cur_hit.take(),
                    semantic: cur_sem_start.map(|start| {
                        (
                            entry_id,
                            start,
                            std::mem::take(&mut cur_sem_text),
                            cur_decor,
                        )
                    }),
                });
                cur_w = 0;
            }
            out.push(WrappedRow {
                line: Line::default(),
                hit,
                semantic: semantic.map(|s| (entry_id, line_start, s.text.clone(), decor)),
            });
            entry_offset += 1;
            continue;
        }
        if cur_hit.is_none() {
            cur_hit = hit.clone();
        }
        // 逻辑行首段带 decor 前缀；换行/折行后的续行 decor=0。
        cur_decor = decor;
        for span in line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                if ch == '\n' {
                    // 显式换行结束当前视觉行。
                    if !cur.is_empty() {
                        out.push(WrappedRow {
                            line: Line::from(std::mem::take(&mut cur)),
                            hit: cur_hit.take(),
                            semantic: cur_sem_start.map(|start| {
                                (
                                    entry_id,
                                    start,
                                    std::mem::take(&mut cur_sem_text),
                                    cur_decor,
                                )
                            }),
                        });
                        cur_w = 0;
                        cur_decor = 0;
                    } else {
                        out.push(WrappedRow {
                            line: Line::default(),
                            hit: hit.clone(),
                            semantic: None,
                        });
                    }
                    cur_sem_start = None;
                    line_cell = 0;
                    is_line_start = false;
                    continue;
                }
                let w = crate::tui::text::char_cell_width(ch);
                let is_decor = is_line_start && line_cell < decor;
                // 超宽折行：先 flush 当前视觉行。
                if cur_w + w > width && !cur.is_empty() {
                    out.push(WrappedRow {
                        line: Line::from(std::mem::take(&mut cur)),
                        hit: cur_hit.take(),
                        semantic: cur_sem_start.map(|start| {
                            (
                                entry_id,
                                start,
                                std::mem::take(&mut cur_sem_text),
                                cur_decor,
                            )
                        }),
                    });
                    cur_w = 0;
                    cur_sem_start = None;
                    cur_decor = 0; // 续行无 decor
                    cur_hit = hit.clone();
                    is_line_start = false;
                }
                if !is_decor {
                    // 内容字符：推进语义累积。
                    if cur_sem_start.is_none() {
                        cur_sem_start = Some(line_start + line_consumed);
                    }
                    if let Some(text) = semantic.map(|s| s.text.as_str())
                        && let Some(ch_sem) = text.chars().nth(line_consumed)
                    {
                        cur_sem_text.push(ch_sem);
                    }
                    line_consumed += 1;
                }
                cur.push(Span::styled(ch.to_string(), style));
                cur_w += w;
                line_cell += w;
            }
        }
        if !cur.is_empty() {
            out.push(WrappedRow {
                line: Line::from(std::mem::take(&mut cur)),
                hit: cur_hit.take(),
                semantic: cur_sem_start.map(|start| {
                    (
                        entry_id,
                        start,
                        std::mem::take(&mut cur_sem_text),
                        cur_decor,
                    )
                }),
            });
            cur_w = 0;
            cur_sem_start = None;
        }
        // 逻辑行结束：entry 语义流推进 = 内容字符数 + 换行位。
        entry_offset += line_consumed + 1;
    }
    if !cur.is_empty() {
        out.push(WrappedRow {
            line: Line::from(std::mem::take(&mut cur)),
            hit: cur_hit.take(),
            semantic: cur_sem_start.map(|start| {
                (
                    entry_id,
                    start,
                    std::mem::take(&mut cur_sem_text),
                    cur_decor,
                )
            }),
        });
    }
    out
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
                if ch == '\n' {
                    // §22：显式换行符结束当前行（多行输入/粘贴的 \n 必须换行，
                    // 否则被当宽度 0 字符拼进一行，区域高度与光标计算全错位）。
                    if !cur.is_empty() {
                        out.push(Line::from(std::mem::take(&mut cur)));
                        cur_w = 0;
                    } else {
                        out.push(Line::default());
                    }
                    continue;
                }
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

/// 可点击目标 + 可点击列上限（§24）：`end_col` 为该行可点击的最大列
/// （0 表示该行不可点击）。主行（工具图标+工具名）记录实际宽度；
/// 正文/tail 行不可点（避免整行误触）。
pub type HitRange = (HitTarget, u16);

/// 按 entry 分组的渲染结果：(EntryId, 逻辑行, 逐行 hits, 逐行语义文本)。
/// 单个逻辑行的语义信息（复制/选中/高亮的事实源）。
///
/// - `text`：该逻辑行的**无装饰语义文本**（复制源，不含 rail/icon 等前缀）。
/// - `decor_cells`：该逻辑行视觉渲染中前缀装饰占用的 cell 宽度。语义文本
///   在视觉行中的起始 cell 列 = decor_cells；精确的 cell↔char 映射依赖它
///   （§PointerHit：`│ AI` 等前缀不参与语义偏移）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticLine {
    text: String,
    decor_cells: usize,
}

/// 按 entry 分组的渲染结果：(EntryId, 逻辑行, 逐行 hits, 逐行语义信息)。
/// hits 与 lines 等长；semantic 与 lines 等长，每行是逻辑行的无装饰语义。
/// 工具卡片的行对应卡片 id（鼠标点击展开）。
type EntryGroup = (
    EntryId,
    Vec<Line<'static>>,
    Vec<Option<HitRange>>,
    Vec<SemanticLine>,
);

/// Scratch buffers populated while rendering one or more transcript groups.
/// Bundling them keeps the rendering boundary cohesive and prevents call sites
/// from accidentally swapping parallel vectors.
struct GroupBuffers<'a> {
    lines: &'a mut Vec<Line<'static>>,
    hits: &'a mut Vec<Option<HitRange>>,
    semantic: &'a mut Vec<SemanticLine>,
    groups: &'a mut Vec<EntryGroup>,
}

/// entry 是否命中当前 active_hit（§24 点击高亮）：hits 中任意一行与
/// active_hit 相等即为命中（工具卡片所有行共享同一 hit）。
fn entry_matches_active_hit(
    _entry_id: &EntryId,
    hits: &[Option<HitRange>],
    active: &Option<HitTarget>,
) -> bool {
    match active {
        Some(target) => hits
            .iter()
            .any(|h| h.as_ref().map(|(t, _)| t) == Some(target)),
        None => false,
    }
}

/// 构建一个"整行可点"的 hit（reasoning 折叠行）。
fn full_line_hit(target: HitTarget, width: u16) -> Option<HitRange> {
    Some((target, width))
}

/// 把转录条目渲染为逻辑行（Message 按类型着色/加 rail；Tool 渲染为卡片）。
///
/// 返回按 entry 分组的结果（含 live 哨兵组，§7.2）。
fn build_transcript_text(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<(u64, u16), Vec<Line<'static>>>,
    width: usize,
) -> Vec<EntryGroup> {
    let mut groups: Vec<EntryGroup> = Vec::with_capacity(view.transcript.len());
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<Option<HitRange>> = Vec::new();
    let mut semantic: Vec<SemanticLine> = Vec::new();
    let push_hit = |lines: &mut Vec<Line<'static>>,
                    hits: &mut Vec<Option<HitRange>>,
                    semantic: &mut Vec<SemanticLine>,
                    line: Line<'static>,
                    hit: Option<HitRange>,
                    semantic_text: String| {
        // decor_cells = 视觉行总宽 - 语义正文宽（rail/icon 前缀占位）。
        // 语义文本是纯内容，视觉行含装饰前缀；差值即前缀 cell 宽度。
        let line_width = unicode_width::UnicodeWidthStr::width(
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .as_str(),
        );
        let text_width = unicode_width::UnicodeWidthStr::width(semantic_text.as_str());
        let decor_cells = line_width.saturating_sub(text_width);
        lines.push(line);
        hits.push(hit);
        semantic.push(SemanticLine {
            text: semantic_text,
            decor_cells,
        });
    };
    // 搜索命中集合（高亮用，§14）：只作用于 transcript，live 区不参与。
    let hit_ids: std::collections::HashSet<EntryId> = view
        .search
        .as_ref()
        .map(|s| s.hits.iter().copied().collect())
        .unwrap_or_default();
    for entry in view.transcript.iter() {
        let entry_id = entry.id();
        out.clear();
        hits.clear();
        match entry {
            Entry::Message { line, .. } => match line.kind {
                // §16.2：用户消息细紫红左 rail + 小型 `you` 标签。
                LineKind::User => {
                    // §codex：表格在 rail 前缀之后的内容宽度内布局（防止加 rail 后超宽）。
                    let content_width = width.saturating_sub(RAIL_WIDTH);
                    let rendered = cached_markdown(cache, line, theme, content_width);
                    for (i, rendered_line) in rendered.iter().enumerate() {
                        let mut spans = vec![Span::styled(
                            "│ ",
                            Style::default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        )];
                        if i == 0 {
                            spans.push(Span::styled(
                                "you  ",
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        } else {
                            spans.push(Span::raw("     "));
                        }
                        spans.extend(rendered_line.spans.iter().cloned());
                        let semantic_text = rendered_line
                            .spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>();
                        push_hit(
                            &mut out,
                            &mut hits,
                            &mut semantic,
                            Line::from(spans),
                            None,
                            semantic_text,
                        );
                    }
                }
                // §16.2：assistant 消息带左 rail + `AI` 标签（与用户消息呼应，
                // 形成清晰的双角色层次；正文 Markdown 渲染）。
                LineKind::Assistant => {
                    // §codex：表格在 rail 前缀之后的内容宽度内布局。
                    let content_width = width.saturating_sub(RAIL_WIDTH);
                    let rendered = cached_markdown(cache, line, theme, content_width);
                    for (i, rendered_line) in rendered.iter().enumerate() {
                        let mut spans = vec![Span::styled(
                            "│ ",
                            Style::default()
                                .fg(theme.primary)
                                .add_modifier(Modifier::BOLD),
                        )];
                        if i == 0 {
                            spans.push(Span::styled(
                                "AI   ",
                                Style::default()
                                    .fg(theme.primary)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        } else {
                            spans.push(Span::raw("     "));
                        }
                        spans.extend(rendered_line.spans.iter().cloned());
                        let semantic_text = rendered_line
                            .spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>();
                        push_hit(
                            &mut out,
                            &mut hits,
                            &mut semantic,
                            Line::from(spans),
                            None,
                            semantic_text,
                        );
                    }
                }
                // §用户诉求：thinking 与工具统一折叠——未展开显示前 N 行 +
                // "… 点击展开"，展开显示全文（点击行切换）。
                LineKind::Reasoning => {
                    const THINKING_COLLAPSED: usize = 6;
                    let all_lines: Vec<&str> = line.text.split('\n').collect();
                    let overflow = all_lines.len() > THINKING_COLLAPSED;
                    let expanded = view.is_reasoning_expanded(entry_id);
                    let shown = if expanded || !overflow {
                        &all_lines[..]
                    } else {
                        &all_lines[..THINKING_COLLAPSED]
                    };
                    let clickable = overflow; // 只有溢出才可点击展开
                    for (i, s) in shown.iter().enumerate() {
                        let prefix = if i == 0 { "思考 " } else { "    " };
                        let hit = if clickable && i == 0 {
                            full_line_hit(HitTarget::Reasoning(entry_id), width as u16)
                        } else {
                            None
                        };
                        push_hit(
                            &mut out,
                            &mut hits,
                            &mut semantic,
                            Line::styled(
                                format!("{prefix}{s}"),
                                Style::default()
                                    .fg(theme.muted)
                                    .add_modifier(Modifier::ITALIC),
                            ),
                            hit,
                            s.to_string(),
                        );
                    }
                    if overflow {
                        let hint = if expanded {
                            "思考 · 点击折叠".to_string()
                        } else {
                            format!("… 点击展开思考（共 {} 行）", all_lines.len())
                        };
                        push_hit(
                            &mut out,
                            &mut hits,
                            &mut semantic,
                            Line::styled(
                                hint,
                                Style::default()
                                    .fg(theme.muted)
                                    .add_modifier(Modifier::ITALIC),
                            ),
                            full_line_hit(HitTarget::Reasoning(entry_id), width as u16),
                            String::new(), // 折叠提示不是可复制内容
                        );
                    }
                }
                LineKind::Tool => {
                    for (i, s) in line.text.split('\n').enumerate() {
                        let prefix = if i == 0 { "工具 " } else { "    " };
                        push_hit(
                            &mut out,
                            &mut hits,
                            &mut semantic,
                            Line::styled(format!("{prefix}{s}"), Style::default().fg(theme.info)),
                            None,
                            s.to_string(),
                        );
                    }
                }
                LineKind::System => {
                    // PM：纯分隔线按终端宽度铺满（此前固定 40 个 ─，窄屏折行、宽屏过短）。
                    if !line.text.is_empty() && line.text.chars().all(|c| c == '─') {
                        push_hit(
                            &mut out,
                            &mut hits,
                            &mut semantic,
                            Line::styled("─".repeat(width), Style::default().fg(theme.warning)),
                            None,
                            String::new(), // 分隔线不是可复制内容
                        );
                    } else {
                        for (i, s) in line.text.split('\n').enumerate() {
                            let prefix = if i == 0 { "系统 " } else { "    " };
                            push_hit(
                                &mut out,
                                &mut hits,
                                &mut semantic,
                                Line::styled(
                                    format!("{prefix}{s}"),
                                    Style::default().fg(theme.warning),
                                ),
                                None,
                                s.to_string(),
                            );
                        }
                    }
                }
            },
            Entry::Tool { card, .. } => {
                let card_id = card.id.clone();
                let card_lines = tool_card_lines(card, view.anim_tick, theme, width);
                for (i, line) in card_lines.into_iter().enumerate() {
                    // §视觉瘦身：只主行可点击（icon+name 区域），内容行留给文本选择。
                    let hit = if i == 0 {
                        Some((HitTarget::Tool(card_id.clone()), width as u16))
                    } else {
                        None
                    };
                    // 语义文本：主行去 icon 前缀（首个 "✓ " 等），内容行原样。
                    let semantic_text = if i == 0 {
                        card_semantic_header(card)
                    } else {
                        let raw = line
                            .spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>();
                        card_semantic_content(card, i, &raw)
                    };
                    // §视觉瘦身：卡片扁平化——无 surface 背景；diff 行本身红绿。
                    push_hit(&mut out, &mut hits, &mut semantic, line, hit, semantic_text);
                }
            }
        }
        // §14 高亮：命中条目整段加下划线（保留原有 fg 样式）。
        if hit_ids.contains(&entry_id) {
            let highlighted = std::mem::take(&mut out)
                .into_iter()
                .map(|line| {
                    Line::from(
                        line.spans
                            .into_iter()
                            .map(|span| {
                                Span::styled(
                                    span.content,
                                    span.style.add_modifier(Modifier::UNDERLINED),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            out = highlighted;
        }
        // §24：鼠标点击展开的工具卡片/reasoning 行高亮（Overlay 打开期间，
        // 反馈"点到了哪一行"）。
        if entry_matches_active_hit(&entry_id, &hits, &view.active_hit) {
            let active_bg = theme.surface;
            let highlighted = std::mem::take(&mut out)
                .into_iter()
                .map(|line| {
                    Line::from(
                        line.spans
                            .into_iter()
                            .map(|span| {
                                Span::styled(
                                    span.content,
                                    span.style.bg(active_bg).add_modifier(Modifier::BOLD),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            out = highlighted;
        }
        groups.push((
            entry_id,
            std::mem::take(&mut out),
            std::mem::take(&mut hits),
            std::mem::take(&mut semantic),
        ));
    }
    // TUI v2 §7.2：live 区（流式 assistant/reasoning + 运行中工具）作为
    // 最后一个 group（哨兵 id；Follow 时显示在尾部，Locked 锚定不到它）。
    build_live_group(
        view,
        theme,
        cache,
        width,
        GroupBuffers {
            lines: &mut out,
            hits: &mut hits,
            semantic: &mut semantic,
            groups: &mut groups,
        },
    );
    groups
}

/// 渲染 live 区（§7.2）：reasoning（折叠策略同历史）→ assistant（Markdown）
/// → 运行中工具卡片（按启动顺序）。
fn build_live_group(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<(u64, u16), Vec<Line<'static>>>,
    width: usize,
    buffers: GroupBuffers<'_>,
) {
    let GroupBuffers {
        lines: out,
        hits,
        semantic,
        groups,
    } = buffers;
    let live = &view.live;
    if live.reasoning.is_none() && live.assistant.is_none() && live.tools.is_empty() {
        return;
    }
    out.clear();
    hits.clear();
    semantic.clear();
    // 流式 reasoning（折叠策略与历史一致：Alt+T 展开 / 点击打开 Overlay）。
    // §PointerHit ⑤：reasoning 是独立 group（自己的稳定 EntryId）。
    if let Some(msg) = &live.reasoning
        && !msg.text.is_empty()
    {
        if view.reasoning_visible {
            for s in msg.text.split('\n') {
                let rendered = format!("思考 {s}");
                let decor = unicode_width::UnicodeWidthStr::width("思考 ");
                out.push(Line::styled(
                    rendered.clone(),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                ));
                hits.push(None);
                semantic.push(SemanticLine {
                    text: s.to_string(),
                    decor_cells: decor,
                });
            }
        } else {
            out.push(Line::styled(
                "◇ 思考 · 流式中…（Alt+T 展开 · 点击查看）",
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::ITALIC),
            ));
            // §PointerHit：live reasoning 折叠行可点击（与历史一致），
            // 打开 reasoning overlay（msg.entry_id 是稳定 id）。
            hits.push(Some((HitTarget::Reasoning(msg.entry_id), 0)));
            semantic.push(SemanticLine {
                text: String::new(),
                decor_cells: 0,
            });
        }
        groups.push((
            msg.entry_id,
            std::mem::take(out),
            std::mem::take(hits),
            std::mem::take(semantic),
        ));
        out.clear();
        hits.clear();
        semantic.clear();
    }
    // 流式 assistant（Markdown，按 version 缓存）。带左 rail + AI 标签，
    // 与历史 assistant 消息一致（§16.2 层次感）。独立 group（稳定 id）。
    if let Some(msg) = &live.assistant
        && !msg.text.is_empty()
    {
        let rendered = if let Some(lines) = cache.get(&(msg.version, width as u16)) {
            lines.clone()
        } else {
            let lines = render_markdown(&msg.text, theme, Some(width));
            if cache.len() > 2048 {
                cache.clear();
            }
            cache.insert((msg.version, width as u16), lines);
            cache[&(msg.version, width as u16)].clone()
        };
        for (i, rendered_line) in rendered.iter().enumerate() {
            let mut spans = vec![Span::styled(
                "│ ",
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD),
            )];
            if i == 0 {
                spans.push(Span::styled(
                    "AI   ",
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw("     "));
            }
            spans.extend(rendered_line.spans.iter().cloned());
            let semantic_text = rendered_line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            let line = Line::from(spans);
            let line_width = unicode_width::UnicodeWidthStr::width(
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .as_str(),
            );
            let text_width = unicode_width::UnicodeWidthStr::width(semantic_text.as_str());
            out.push(line);
            hits.push(None);
            semantic.push(SemanticLine {
                text: semantic_text,
                decor_cells: line_width.saturating_sub(text_width),
            });
        }
        groups.push((
            msg.entry_id,
            std::mem::take(out),
            std::mem::take(hits),
            std::mem::take(semantic),
        ));
        out.clear();
        hits.clear();
        semantic.clear();
    }
    // 运行中工具卡片（按启动顺序）。§PointerHit ⑤：每张卡片独立 group/稳定 id。
    for call_id in &live.tool_order {
        if let Some(tool) = live.tools.get(call_id) {
            let card = &tool.card;
            let card_id = card.id.clone();
            let card_lines = tool_card_lines(card, view.anim_tick, theme, width);
            for (i, line) in card_lines.into_iter().enumerate() {
                // §24：只有主行可点击（图标+工具名区域），正文行不可点。
                let hit = if i == 0 {
                    Some((HitTarget::Tool(card_id.clone()), line.width() as u16))
                } else {
                    None
                };
                // §视觉瘦身：卡片扁平化——主行无 surface 背景；diff 行保留红绿。
                let raw_text = line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                let semantic_text = if i == 0 {
                    card_semantic_header(card)
                } else {
                    card_semantic_content(card, i, &raw_text)
                };
                let line_width = unicode_width::UnicodeWidthStr::width(raw_text.as_str());
                let text_width = unicode_width::UnicodeWidthStr::width(semantic_text.as_str());
                out.push(line);
                hits.push(hit);
                semantic.push(SemanticLine {
                    text: semantic_text,
                    decor_cells: line_width.saturating_sub(text_width),
                });
            }
            groups.push((
                tool.entry_id,
                std::mem::take(out),
                std::mem::take(hits),
                std::mem::take(semantic),
            ));
            out.clear();
            hits.clear();
            semantic.clear();
        }
    }
}

/// §codex 移植：User/Assistant 消息 rail 前缀宽度（`│ you  ` / `│ AI   ` = 7 cell）。
/// 表格在内容宽度（width - rail）内布局，防止加 rail 后超宽被二次 wrap。
const RAIL_WIDTH: usize = 7;

/// 按条目版本缓存 Markdown 渲染（流式增量只重渲染变化条目，§16.1）。
fn cached_markdown(
    cache: &mut HashMap<(u64, u16), Vec<Line<'static>>>,
    line: &TranscriptLine,
    theme: theme::Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let key = (line.version, width as u16);
    if let Some(lines) = cache.get(&key) {
        return lines.clone();
    }
    let lines = render_markdown(&line.text, theme, Some(width));
    if cache.len() > 2048 {
        cache.clear();
    }
    cache.insert(key, lines);
    cache[&key].clone()
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

    // 多行输入提示（人体工学）：输入含换行时显示当前行数（>8 行内部滚动也可见）。
    let input_lines = view.input.matches('\n').count() + 1;
    if input_lines > 1 {
        spans.push(Span::styled(format!(" · 输入 {input_lines} 行"), muted));
    }
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
    // 历史浏览（Locked）状态提示：没有新内容时用户仍需要知道自己不在 Follow 模式。
    if view.scroll_mode != ScrollMode::Follow && view.pending_below == 0 {
        spans.push(Span::styled(
            " · 历史浏览中 · Ctrl+End 返回最新",
            Style::default().fg(theme.warning),
        ));
    }
    // 整改 D：footer 固定顺序 workspace · model · state · ctx · tokens · 提示。
    // 上下文用量条（§对比：gemini-cli ContextUsageDisplay；projected/usable）。
    if let Some((projected, usable)) = view.context_usage
        && usable > 0
    {
        let ratio = projected as f64 / usable as f64;
        // §PointerHit 22：bar 长度随终端宽度变化（窄屏减小，避免占比过大）。
        let bar_cells = if area.width >= 100 {
            20
        } else if area.width >= 80 {
            14
        } else {
            8
        };
        let filled = ((ratio * bar_cells as f64) as usize).clamp(0, bar_cells);
        let bar: String = "█".repeat(filled) + &"░".repeat(bar_cells - filled);
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
    // §16.2：缓存命中的输入 token（⇄ 标记；减少真实计费输入的直观反馈）。
    if view.cache_read_tokens > 0 {
        spans.push(Span::styled(
            format!(" · ⇄{}", fmt_tokens(view.cache_read_tokens)),
            Style::default().fg(theme.success),
        ));
    }
    // §16.2：配置单价后显示本会话花费。
    if view.cost_usd > 0.0 {
        spans.push(Span::styled(
            format!(" · ${:.4}", view.cost_usd),
            Style::default().fg(theme.warning),
        ));
    }
    // 过渡提示（如“没有用户消息可跳转”）：下一次键盘/鼠标操作后消失。
    if let Some(hint) = &view.transient_hint {
        spans.push(Span::styled(
            format!(" · {hint}"),
            Style::default().fg(theme.warning),
        ));
    }
    // 兼容模式提示：仅 inline 且 scrollback 不可用时显示（fullscreen 正常模式）。
    if !scrollback && mode == terminal::ViewMode::Inline {
        spans.push(Span::styled(
            " · 兼容模式（无滚动回退）",
            Style::default().fg(theme.warning),
        ));
    }
    // §PointerHit 21：footer 按可用宽度裁剪——保留前面高优先级（workspace/
    // model/status），窄屏丢弃尾部（ctx bar/tokens/cost 等由拼序体现）。
    let available = area.width as usize;
    let mut clipped: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let w = crate::tui::text::display_width(span.content.as_ref());
        if used + w > available {
            break;
        }
        clipped.push(span);
        used += w;
    }
    frame.render_widget(Paragraph::new(Line::from(clipped)), area);
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
        for glyph in glyphs
            .iter_mut()
            .take((top + thumb_h).min(area_h))
            .skip(top)
        {
            *glyph = "▐";
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
        view.search.is_some(),
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
    search_open: bool,
) -> bool {
    (matches!(status, StatusLine::Idle) || !input_empty) && !overlay_or_modal_open && !search_open
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
    if input.is_empty() {
        // 空输入：光标必须停在 prompt 右侧（修复“首次打开光标在 ❯ 左边”）。
        return (0, PROMPT_WIDTH);
    }
    let budget = width.saturating_sub(PROMPT_WIDTH).max(1) as usize;
    // 光标前的文本按 `\n` + 宽度折行 → 折成逻辑行；定位光标所在逻辑行。
    let before = &input[..cursor];
    let wrapped_before = wrap_lines(vec![Line::from(Span::raw(before.to_string()))], budget);
    // 光标后的内容决定光标所在行的列：该行已折行的最后一行宽度 = 光标列。
    let row = wrapped_before.len().saturating_sub(1) as u16;
    let col = wrapped_before.last().map(|l| l.width()).unwrap_or(0) as u16;
    // 第 0 行含 prompt；续行从内容起点（无 prompt）。
    let col = if row == 0 { PROMPT_WIDTH + col } else { col };
    (row, col)
}

/// 详情 Overlay（整改 B）：带边框对话框，展示 command/output/status；
/// Esc 关闭、PgUp/PgDn 内部滚动；不修改 scrollback。
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
    // §16.2 增强：/diff Modal 的 diff 行红绿着色；其他 Modal 保持纯文本
    // （避免 settings/help 里以 +/- 开头的行被误着色）。
    let diff_colored = modal.title == "/diff";
    for line in modal.body.lines() {
        let styled: Line<'static> =
            if diff_colored && line.starts_with('+') && !line.starts_with("+++") {
                Line::styled(line.to_string(), Style::default().fg(theme.success))
            } else if diff_colored && line.starts_with('-') && !line.starts_with("---") {
                Line::styled(line.to_string(), Style::default().fg(theme.error))
            } else if diff_colored && line.starts_with("@@") {
                Line::styled(line.to_string(), Style::default().fg(theme.primary))
            } else if diff_colored && (line.starts_with("+++") || line.starts_with("---")) {
                Line::styled(
                    line.to_string(),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Line::styled(line.to_string(), Style::default().fg(theme.text))
            };
        content.push(styled);
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
    let rows = wrap_with_semantic(content, Vec::new(), &[], inner_w, EntryId(0));
    let wrapped: Vec<Line<'static>> = rows.into_iter().map(|r| r.line).collect();
    let total = wrapped.len() as u16;
    let scroll = overlay.scroll.min(total.saturating_sub(inner_h as u16));
    let start = scroll as usize;
    let end = (start + inner_h).min(wrapped.len());
    let window = wrapped[start..end].to_vec();

    // Border title follows overlay type: tool details vs thinking (reasoning) window.
    let border_title = if overlay.tool_id.is_some() {
        " Tool details "
    } else {
        " 思考（reasoning） "
    };
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(border_title);
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
    let total = menu.items.len();
    let visible = (rect.height as usize).max(1);
    // 长菜单（如 /sessions 多会话）：可视窗口跟随选中项，上下用 … 表示有更多；
    // 否则选中项超出可视区时用户看不到当前选择。
    let top_ellipsis = total > visible;
    let bottom_ellipsis = total > visible;
    let window_rows = visible
        .saturating_sub(usize::from(top_ellipsis) + usize::from(bottom_ellipsis))
        .max(1);
    let start = if total > window_rows {
        (menu.selected.saturating_sub(window_rows / 2)).min(total - window_rows)
    } else {
        0
    };
    let end = (start + window_rows).min(total);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if top_ellipsis {
        lines.push(Line::styled("…", Style::default().fg(theme.muted)));
    }
    for (i, (name, desc)) in menu.items.iter().enumerate().skip(start).take(end - start) {
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
    if bottom_ellipsis {
        lines.push(Line::styled("…", Style::default().fg(theme.muted)));
    }
    // Menu also floats over the transcript: clear the area first so unselected rows do not bleed background text.
    frame.render_widget(ratatui::widgets::Clear, rect);
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
    let mut cache: HashMap<(u64, u16), Vec<Line<'static>>> = HashMap::new();
    let mut cache_width = 0u16;
    let mut committed = 0usize;
    let scrollback = mode == terminal::ViewMode::Inline;
    terminal
        .draw(|frame| {
            render_frame(
                frame,
                view,
                theme,
                RenderContext {
                    markdown_cache: &mut cache,
                    cache_width: &mut cache_width,
                    committed: &mut committed,
                    scrollback,
                    mode,
                },
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
        let mut cache: HashMap<(u64, u16), Vec<Line<'static>>> = HashMap::new();
        let mut cache_width = 0u16;
        let mut committed = 0usize;
        let mut overflow: Vec<Line<'static>> = Vec::new();
        terminal
            .draw(|frame| {
                let plan = render_frame(
                    frame,
                    view,
                    theme,
                    RenderContext {
                        markdown_cache: &mut cache,
                        cache_width: &mut cache_width,
                        committed: &mut committed,
                        scrollback: true,
                        mode: terminal::ViewMode::Inline,
                    },
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

    /// §视觉瘦身：工具卡片只有主行可点击（内容行留给文本选择，避免与拖选冲突）。
    #[test]
    fn tool_card_only_header_clickable() {
        let mut view = ViewModel::default();
        view.begin_tool("c1", "bash", Some("cmd".into()), None);
        view.finish_tool(
            ("c1", "bash"),
            crate::tool::outcome::ToolStatus::Failed,
            10,
            Some(1),
            "第一行\n第二行\n第三行\n第四行",
            None,
        );
        let mut cache = HashMap::new();
        let plan = plan_window(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
        // row_hits 与 window 等长；只有第 0 行（主行）可点击，正文行为 None。
        assert!(plan.window.len() >= 2, "卡片含主行+正文: {:?}", plan.window);
        assert!(
            matches!(&plan.row_hits[0], Some((HitTarget::Tool(id), end)) if id == "c1" && *end > 0),
            "主行必须可点击: {:?}",
            plan.row_hits[0]
        );
        for hit in plan.row_hits.iter().skip(1) {
            assert!(hit.is_none(), "正文行不可点击（留给文本选择）: {hit:?}");
        }
    }

    #[test]
    fn plan_window_resize_resets_committed_without_overflow() {
        let mut view = ViewModel::default();
        for i in 0..10 {
            view.push_line(LineKind::Assistant, format!("line {i}"));
        }
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

    /// §58 回归：Follow → PgUp（进 Locked）→ PgDn 到底 → 新内容 → 再 PgDn 必须能滚到底。
    /// 防止「滚动无反应」（scroll_down 在 Follow 直接 return + Locked 到底后不自动跟随）。
    #[test]
    fn scroll_up_then_down_with_new_content_is_not_stuck() {
        let mut view = ViewModel::default();
        for i in 0..8 {
            view.push_line(LineKind::Assistant, format!("line {i}"));
        }
        let mut cache = HashMap::new();
        // 布局：Follow，视口 4 行 → 顶部行 = 4（显示 line4-7）。
        let plan = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        assert_eq!(plan.window.len(), 4);
        let window_text: String = plan
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            window_text.contains("line 7"),
            "Follow 显示尾部: {window_text}"
        );

        // PgUp 两次（每次 8 行，实际 clamp 到顶）：应进入 Locked 并锚定顶部。
        view.scroll_up(8);
        view.scroll_up(8);
        assert!(
            matches!(view.scroll_mode, ScrollMode::Locked(_)),
            "PgUp 必须进入 Locked"
        );
        let plan_top = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        let window_text: String = plan_top
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            window_text.contains("line 0"),
            "PgUp 后应显示顶部: {window_text}"
        );

        // 新内容到达（模拟 run 中）→ 仍 Locked（视口不动）。
        view.push_line(LineKind::Assistant, "line 8 new".to_string());
        let plan_new = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        let window_text: String = plan_new
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            !window_text.contains("line 8 new"),
            "Locked 时新内容不移动视口"
        );

        // PgDn 回到底部（多次）：最终应能看到新内容（可滚回最新）。
        for _ in 0..10 {
            view.scroll_down(8);
        }
        let plan_bottom = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        let window_text: String = plan_bottom
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            window_text.contains("line 8 new"),
            "PgDn 到底后应能看到新内容（不得卡住）: {window_text}"
        );
        // 滚到底自动回 Follow：新内容继续自动跟随（§10 体验修复）。
        assert_eq!(
            view.scroll_mode,
            ScrollMode::Follow,
            "滚到底后必须自动回到 Follow（新内容自动跟随，不再卡住）"
        );
        view.push_line(LineKind::Assistant, "line 9 newest".to_string());
        let plan_newest = plan_window(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        let window_text: String = plan_newest
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            window_text.contains("line 9 newest"),
            "回 Follow 后新内容自动跟随: {window_text}"
        );
    }

    #[test]
    fn markdown_bold_code_and_code_block_render_styled() {
        let theme = theme::Theme::omp();
        let lines = render_markdown("**加粗** 和 `code`", theme, None);
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

        let lines = render_markdown("```rust\nfn main() {}\n```", theme, None);
        assert_eq!(lines.len(), 1, "代码块渲染为一行");
        assert_eq!(lines[0].spans[0].content, "fn main() {}");
    }

    #[test]
    fn markdown_list_and_link_render() {
        let theme = theme::Theme::omp();
        let lines = render_markdown(
            "- 第一项\n- 第二项\n\n[链接](https://example.com)",
            theme,
            None,
        );
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

    /// §16.2 增强：markdown 标题分级渲染（h1-h3 分色 + BOLD，h4+ 归一）。
    #[test]
    fn markdown_headings_render_with_level_styles() {
        let theme = theme::Theme::omp();
        let lines = render_markdown(
            "# 大标题\n\n## 副标题\n\n### 小节\n\n#### 四级\n",
            theme,
            None,
        );
        assert_eq!(lines.len(), 4, "四个标题各一行: {lines:?}");
        // h1 → primary + BOLD。
        let h1 = &lines[0];
        assert!(h1.spans.iter().all(|s| s.content == "大标题"));
        let h1_style = &h1.spans[0].style;
        assert_eq!(h1_style.fg, Some(theme.primary));
        assert!(
            h1_style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        // h2 → accent + BOLD。
        let h2 = &lines[1];
        assert_eq!(h2.spans[0].style.fg, Some(theme.accent));
        assert!(
            h2.spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        // h4 → text + BOLD（归一）。
        let h4 = &lines[3];
        assert_eq!(h4.spans[0].style.fg, Some(theme.text));
        assert!(
            h4.spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    /// §16.2 增强：markdown 表格渲染——带边框、表头加粗、列宽对齐。
    #[test]
    fn markdown_table_renders_with_borders_and_header() {
        let theme = theme::Theme::omp();
        let md = "| 名称 | 数量 |\n| --- | ---: |\n| 苹果 | 3 |\n| 香蕉 | 10 |";
        let lines = render_markdown(md, theme, None);
        // 期望：顶边框 + 表头 + 分隔 + 2 数据行 + 底边框 = 6 行。
        assert!(lines.len() >= 6, "表格应渲染多行: {lines:?}");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            text.contains("名称") && text.contains("数量"),
            "表头可见: {text}"
        );
        assert!(
            text.contains("苹果") && text.contains("香蕉"),
            "数据行可见: {text}"
        );
        assert!(text.contains('│'), "带竖边框: {text}");
        // 表头行加粗。
        let header_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "名称"))
            .expect("找到表头行");
        assert!(
            header_line.spans.iter().any(|s| s
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)),
            "表头必须加粗"
        );
    }

    /// §codex 移植：超宽表格在窄 width 下 cell 内换行，且**每个子行分隔符都在**。
    /// 修复旧实现「列分隔符被通用 wrapper 切碎」的缺陷。
    #[test]
    fn markdown_table_wraps_cells_within_width() {
        let theme = theme::Theme::omp();
        // 超宽 cell：第 2 列内容很长，窄 width 下必须 cell 内换行。
        let md =
            "| # | 现象 |\n| --- | --- |\n| 1 | 这是很长的一段描述，内容远超单列宽度，必须换行 |";
        let lines = render_markdown(md, theme, Some(24));
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // 每行宽度 ≤ 24（未被通用 wrapper 切碎）。
        for line in &lines {
            let w: usize = line
                .spans
                .iter()
                .map(|s| crate::tui::text::display_width(s.content.as_ref()))
                .sum();
            assert!(w <= 24, "表格行不得超宽: '{text}' w={w}");
        }
        // 换行后仍有多个含 │ 的行（每行分隔符都在）。
        let rows_with_border = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content == "│"))
            .count();
        assert!(rows_with_border >= 3, "换行后每行都应有分隔符: {text}");
        // 内容完整保留（不丢字）：按行检查关键片段。
        let row_texts: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(
            row_texts.iter().any(|r| r.contains("这是很长的一段")),
            "cell 首行片段保留: {text}"
        );
        assert!(
            row_texts.iter().any(|r| r.contains("换行")),
            "cell 末行片段保留（不丢字）: {text}"
        );
    }

    /// §codex 移植：极窄终端下表格退化为 records（label: value，逐行），
    /// 不产生 0 宽/不可读的输出。
    #[test]
    fn markdown_table_degrades_to_records_on_narrow_width() {
        let theme = theme::Theme::omp();
        let md = "| 名称 | 值 |\n| --- | --- |\n| alpha | 42 |";
        let lines = render_markdown(md, theme, Some(8));
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // 窄屏：records 模式（label: value），仍保留内容。
        assert!(
            text.contains("名称") && text.contains("42"),
            "records 模式仍应保留内容: {text}"
        );
    }

    /// §用户诉求：unified diff 红绿背景（+ 绿底 / - 红底 / @@ 主色）。
    #[test]
    fn diff_lines_render_with_add_remove_colors() {
        let theme = theme::Theme::omp();
        let diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n }";
        let lines = render_diff_lines(diff, theme);
        // 行数 = diff 行数（7 行：--- +++ @@ 上下文 - + 上下文）。
        assert_eq!(lines.len(), 7, "每行一个 Line: {lines:?}");
        // - 行 → error 背景（红底）。
        let minus = lines
            .iter()
            .find(|l| l.spans[0].content.starts_with("-    let x = 1;"))
            .expect("找到 - 行");
        assert_eq!(minus.style.bg, Some(theme.error));
        // + 行 → success 背景（绿底）。
        let plus = lines
            .iter()
            .find(|l| l.spans[0].content.starts_with("+    let x = 2;"))
            .expect("找到 + 行");
        assert_eq!(plus.style.bg, Some(theme.success));
        // @@ 行 → primary 色。
        let hunk = lines
            .iter()
            .find(|l| l.spans[0].content.starts_with("@@"))
            .expect("找到 @@ 行");
        assert_eq!(hunk.style.fg, Some(theme.primary));
    }

    /// §用户诉求：edit/write 卡片**未展开**时 diff 也必须显示（默认可见）。
    #[test]
    fn tool_card_shows_diff_without_expanding() {
        let theme = theme::Theme::omp();
        let card = ToolCard {
            id: "c1".into(),
            name: "edit".into(),
            target: Some("src/lib.rs".into()),
            command: None,
            state: ToolCardState::Done {
                status: crate::tool::outcome::ToolStatus::Succeeded,
                duration_ms: 10,
                exit_code: Some(0),
            },
            output: Some("status: succeeded\ntool: edit\npath: src/lib.rs\n".into()),
            diff: Some(
                "--- a/src/lib.rs\n+++ b/src/lib.rs\n-    let x = 1;\n+    let x = 2;\n".into(),
            ),
            output_truncated: false,
            expanded: false, // 未展开——diff 仍应显示
            tail: None,
        };
        let lines = tool_card_lines(&card, 0, theme, 100);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            text.contains("let x = 1") && text.contains("let x = 2"),
            "未展开时 diff 必须显示: {text:?}"
        );
        // diff 行带红/绿背景。
        let minus = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1")))
            .expect("找到 - 行");
        assert_eq!(minus.style.bg, Some(theme.error), "删除行红底");
        let plus = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 2")))
            .expect("找到 + 行");
        assert_eq!(plus.style.bg, Some(theme.success), "新增行绿底");
    }

    /// §用户诉求：diff 自动展开但限长——未展开时只显示前 N 行 + 折叠提示，
    /// 展开时显示全部。
    #[test]
    fn tool_card_diff_limits_length_when_collapsed() {
        let theme = theme::Theme::omp();
        // 30 行 diff。
        let mut diff = String::new();
        for i in 0..30 {
            diff.push_str(&format!("+line {i}\n"));
        }
        let mk = |expanded: bool| ToolCard {
            id: "c1".into(),
            name: "edit".into(),
            target: None,
            command: None,
            state: ToolCardState::Done {
                status: crate::tool::outcome::ToolStatus::Succeeded,
                duration_ms: 10,
                exit_code: Some(0),
            },
            output: None,
            diff: Some(diff.clone()),
            output_truncated: false,
            expanded,
            tail: None,
        };
        // 未展开：只显示 12 行 + 提示。
        let collapsed = tool_card_lines(&mk(false), 0, theme, 100);
        let diff_line_count = collapsed
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("line ")))
            .count();
        assert!(
            diff_line_count <= 12,
            "折叠态 diff 最多 12 行: {diff_line_count}"
        );
        let text: String = collapsed
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("点击展开"), "折叠态显示展开提示: {text}");
        // 展开：显示全部 30 行。
        let expanded = tool_card_lines(&mk(true), 0, theme, 100);
        let expanded_count = expanded
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("line ")))
            .count();
        assert_eq!(expanded_count, 30, "展开态显示全部 diff");
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
            diff: None,
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
            diff: None,
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
            diff: None,
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
            diff: None,
            output_truncated: false,
            expanded: false,
            tail: None,
        };
        let lines = tool_card_lines(&card, 0, theme, 100);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // §统一折叠（opencode 式）：运行中显示输出开头（前 10 行），不足 10 行全显。
        assert!(text.contains("progress 1"), "运行中显示输出开头: {text}");
        assert!(
            text.contains("progress 7"),
            "全部 7 行可见（<10 行不折叠）: {text}"
        );
        assert!(!text.contains("点击展开"), "7 行 < 10 行不显示折叠提示");
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
            diff: None,
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

/// §22 回归：多行粘贴（含换行）时光标定位正确——不漂移、不跑出输入区。
#[test]
fn multiline_input_cursor_stays_inside_lines() {
    // 粘贴 3 行文本；光标在末尾。
    let pasted = "第一行\n第二行\n第三行";
    let (row, col) = input_cursor_cell(pasted, pasted.len(), 40);
    assert_eq!(
        row, 2,
        "光标应落在最后一行（第 3 行, index 2）: ({row},{col})"
    );
    assert_eq!(
        col, 6,
        "第 3 行内容宽度 6（无 prompt，仅首行有）: ({row},{col})"
    );

    // 粘贴 3 行 + 窄宽度（每行折行）：光标仍应在可视行内。
    let narrow = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc"; // 每行 10 字符
    let (row, col) = input_cursor_cell(narrow, narrow.len(), 8); // 预算 6
    assert!(row >= 2, "窄屏多行后光标行必须在末尾附近: ({row},{col})");

    // 光标在中间行：第 2 行末尾。
    let idx = "第一行\n第二行".len();
    let (row, _) = input_cursor_cell(pasted, idx, 40);
    assert_eq!(row, 1, "光标在第 2 行: ({row})");
}

/// §22：input_area_rows 对多行输入返回多行（≤8），空输入 1 行。
#[test]
fn multiline_input_area_grows() {
    let mut view = ViewModel::default();
    assert_eq!(input_area_rows(&view, 80), 1, "空输入 1 行");
    view.input = "第一行\n第二行\n第三行".into();
    assert_eq!(input_area_rows(&view, 80), 3, "3 行输入 → 3 行区域");
    // 超 8 行 → clamp 8（内部滚动）。
    view.input = (0..10)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(input_area_rows(&view, 80), 8, "超 8 行 clamp 到 8");
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
        ("c", "bash"),
        crate::tool::outcome::ToolStatus::Failed,
        1,
        Some(1),
        "err",
        None,
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

/// Menu also floats over the transcript: unselected rows must not bleed background text.
#[test]
fn menu_clears_background_before_rendering() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::Assistant, "背景文字X".repeat(20));
    }
    view.input = "/".to_string();
    view.refresh_command_menu();
    let buf = draw_to_test_backend(&mut view, 80, 24);
    // Locate a menu row by its item label (the menu is drawn over the transcript).
    let mut menu_row = None;
    let mut row_text = String::new();
    for y in 0..24u16 {
        row_text.clear();
        for x in 0..80u16 {
            row_text.push_str(buf[(x, y)].symbol());
        }
        if row_text.contains("/help") {
            menu_row = Some(y);
            break;
        }
    }
    let row = menu_row.expect("menu row with /help must be rendered");
    // Trailing cells of a menu row (beyond the item text) must be cleared to blank.
    assert_eq!(
        buf[(47, row)].symbol(),
        " ",
        "menu row must clear background text (col 47 leaked: {:?})",
        buf[(47, row)].symbol()
    );
}

/// Reasoning overlay must show its own border title, not "Tool details".
#[test]
fn reasoning_overlay_uses_thinking_border_title() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::Assistant, "背景文字X".repeat(4));
    }
    view.overlay = Some(crate::tui::model::OverlayState::for_reasoning(
        "let me think",
    ));
    let buf = draw_to_test_backend(&mut view, 80, 24);
    // Find the overlay top border row (Block title is drawn there).
    let mut border_row = None;
    for y in 0..buf.area().height {
        let mut row = String::new();
        for x in 0..buf.area().width {
            row.push_str(buf[(x, y)].symbol());
        }
        if row.contains('┌') {
            border_row = Some(row);
            break;
        }
    }
    let row = border_row.expect("overlay top border must be rendered");
    assert!(
        row.contains('思') && row.contains('考') && row.contains("reasoning") && row.contains('）'),
        "reasoning overlay border must show the thinking title, got: {row:?}"
    );
    assert!(
        !row.contains("Tool details"),
        "reasoning overlay must not show the Tool details border title, got: {row:?}"
    );
}
/// 修复：模型输出（Running + 空输入）期间隐藏光标，避免一直闪烁；空闲时显示。
#[test]
fn cursor_hidden_during_run_when_input_empty() {
    // Product rule: idle always shows; running shows only when input is non-empty;
    // hidden while a Modal/Overlay is open or while search (Ctrl+F) is open.
    assert!(should_show_input_cursor(
        &StatusLine::Idle,
        true,
        false,
        false
    ));
    assert!(should_show_input_cursor(
        &StatusLine::Idle,
        false,
        false,
        false
    ));
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            turn: 1,
            tool: "x".into()
        },
        true,
        false,
        false
    ));
    assert!(should_show_input_cursor(
        &StatusLine::Running {
            turn: 1,
            tool: "x".into()
        },
        false,
        false,
        false
    ));
    assert!(
        !should_show_input_cursor(&StatusLine::Idle, true, true, false),
        "cursor must hide while Modal/Overlay is open"
    );
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            turn: 1,
            tool: "x".into()
        },
        false,
        true,
        false
    ));
    assert!(
        !should_show_input_cursor(&StatusLine::Idle, true, false, true),
        "cursor must hide while search is open (typing goes to the search box)"
    );
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            turn: 1,
            tool: "x".into()
        },
        false,
        false,
        true
    ));

    // Byte level: running + empty input must emit the hide sequence.
    let mut view = ViewModel {
        status: StatusLine::Running {
            turn: 1,
            tool: "generating".into(),
        },
        ..ViewModel::default()
    };
    let s = String::from_utf8_lossy(&draw_captured_bytes(&mut view)).into_owned();
    assert!(s.contains("\x1b[?25l"), "running must hide cursor: {s:?}");

    // Idle frame must show cursor.
    let mut view2 = ViewModel {
        status: StatusLine::Idle,
        ..ViewModel::default()
    };
    let s2 = String::from_utf8_lossy(&draw_captured_bytes(&mut view2)).into_owned();
    assert!(s2.contains("\x1b[?25h"), "idle must show cursor: {s2:?}");

    // Search open (Ctrl+F): typing goes into the search box, composer cursor must hide.
    let mut view3 = ViewModel {
        status: StatusLine::Idle,
        ..ViewModel::default()
    };
    view3.open_search();
    let s3 = String::from_utf8_lossy(&draw_captured_bytes(&mut view3)).into_owned();
    assert!(
        s3.contains("\x1b[?25l"),
        "search open must hide cursor: {s3:?}"
    );
}

/// §14 高亮：搜索命中条目整段带下划线，未命中条目不带。
#[test]
fn search_highlight_underlines_matched_entries() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "占位行");
    view.push_line(LineKind::Assistant, "第一条 hello 内容");
    view.push_line(LineKind::Assistant, "第二条 world 内容");
    view.open_search();
    view.update_search_query("hello");
    let buf = draw_to_test_backend(&mut view, 80, 12);
    let mut matched_underlined = false;
    let mut unmatched_not_underlined = true;
    for y in 0..12u16 {
        for x in 0..80u16 {
            let cell = &buf[(x, y)];
            match cell.symbol() {
                "h" => matched_underlined = cell.modifier.contains(Modifier::UNDERLINED),
                "w" => {
                    unmatched_not_underlined = !cell.modifier.contains(Modifier::UNDERLINED);
                }
                _ => {}
            }
        }
    }
    assert!(matched_underlined, "命中条目必须带下划线高亮");
    assert!(unmatched_not_underlined, "未命中条目不得带下划线");
}

/// §视觉瘦身：不再有常驻 header（信息并入 footer）；消息双角色 rail（you/AI）。
#[test]
fn role_rails_render_without_header() {
    let mut view = ViewModel {
        model_name: "test-model".into(),
        workspace: "tpi".into(),
        ..Default::default()
    };
    view.push_line(LineKind::User, "hello");
    view.push_line(LineKind::Assistant, "hi there");
    let buffer = draw_to_test_backend(&mut view, 60, 10);

    // 视觉瘦身：header 已移除，transcript 上移到第 0 行（不再有品牌行）。
    let top_symbol = buffer
        .cell((0, 0))
        .map(|c| c.symbol().to_string())
        .unwrap_or_default();
    assert_ne!(
        top_symbol.as_str(),
        "T",
        "无常驻 header：transcript 从第 0 行开始（首 cell 是 rail │ 而非品牌）"
    );

    // 消息 rail：整屏文本应含 you 与 AI 标签。
    let mut all = String::new();
    for y in 0..10u16 {
        for x in 0..60u16 {
            all.push(
                buffer
                    .cell((x, y))
                    .map(|c| c.symbol().chars().next().unwrap_or(' '))
                    .unwrap_or(' '),
            );
        }
    }
    assert!(all.contains("you"), "用户消息必须有 you rail: {all:?}");
    assert!(all.contains("AI"), "assistant 消息必须有 AI rail: {all:?}");
}

/// §16.2 增强：工具名按类别着色（bash=info 色，编辑类=success 色）。
#[test]
fn tool_name_style_is_category_colored() {
    let theme = theme::Theme::omp();
    assert_eq!(tool_name_style("bash", theme).fg, Some(theme.info));
    assert_eq!(tool_name_style("edit", theme).fg, Some(theme.success));
    assert_eq!(tool_name_style("read", theme).fg, Some(theme.accent));
    assert_eq!(tool_name_style("web_search", theme).fg, Some(theme.warning));
    assert_eq!(tool_name_style("unknown", theme).fg, Some(theme.text));
}

/// §InteractionRefactor：应用内选择高亮——语义选区（entry + 偏移）投影回
/// 视觉行。选中第 2、3 个 entry 的第一字符起 → 对应 window 行 1-2 高亮。
#[test]
fn selection_highlights_selected_window_rows() {
    use crate::tui::interaction::TextPosition;
    use crate::tui::scroll::EntryId;
    let mut view = ViewModel::default();
    for i in 0..6 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    // 语义选区：entry 2 偏移 0 → entry 4 偏移 0（覆盖 entry 2、3 两行正文）。
    view.selection_start(TextPosition {
        entry_id: EntryId(2),
        offset: 0,
    });
    view.selection_update(TextPosition {
        entry_id: EntryId(4),
        offset: 0,
    });
    view.selection_end();
    let mut cache = HashMap::new();
    let plan = plan_window(&mut view, theme::Theme::omp(), 80, 6, 0, false, &mut cache);
    // 每行 1 个 visual row；行 0..5 对应 window[0..6]。
    assert_eq!(plan.window.len(), 6, "6 条消息各一行: {:?}", plan.window);
    let has_reversed = |idx: usize| {
        plan.window[idx]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
    };
    assert!(has_reversed(1) && has_reversed(2), "选中行 1-2 必须高亮");
    assert!(!has_reversed(0) && !has_reversed(3), "未选中行不高亮");
}

/// §InteractionRefactor：plan_window 的语义映射与视觉行必须对齐——
/// semantic_rows 每行都能定位到对应 entry 的文本，且语义文本能命中真实内容。
#[test]
fn semantic_rows_align_with_window_and_map_text() {
    use crate::tui::scroll::EntryId;
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "hello world");
    view.push_line(LineKind::Assistant, "second line");
    let mut cache = HashMap::new();
    let plan = plan_window(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    // 语义行与窗口行等长（每 entry 至少 1 行）。
    assert_eq!(
        plan.semantic_rows.len(),
        plan.window.len(),
        "semantic_rows 必须与 window 等长"
    );
    assert!(!plan.semantic_rows.is_empty());
    // 每行都带 entry 归属；语义文本 = 消息正文（无 rail 前缀）。
    for row in plan.semantic_rows.iter() {
        let (entry_id, _start, text, _decor) = row.as_ref().expect("窗口行必须有语义映射");
        assert!(
            *entry_id == EntryId(1) || *entry_id == EntryId(2),
            "归属错误 entry: {entry_id:?}"
        );
        assert!(!text.starts_with("│"), "语义文本不得含 rail 前缀: {text:?}");
        assert!(!text.is_empty(), "正文行语义文本不得为空");
    }
    // 语义文本必须能被定位：entry 1 的语义文本就是 "hello world"（markdown 渲染无装饰）。
    let e1_text = plan
        .semantic_rows
        .iter()
        .find_map(|r| {
            let (id, _, t, _) = r.as_ref()?;
            (*id == EntryId(1)).then(|| t.clone())
        })
        .expect("entry 1 必须有语义行");
    assert!(
        e1_text.contains("hello"),
        "entry1 语义文本错误: {e1_text:?}"
    );
}

/// §InteractionRefactor：hit_text 从屏幕坐标命中语义位置——CJK 按 cell 宽度。
#[test]
fn hit_text_maps_screen_column_to_char_offset() {
    use crate::tui::interaction::{TextPosition, cell_to_char};
    // 纯函数直接验证（渲染层 hit_text 复用同一映射）。
    let text = "abc你好xyz";
    assert_eq!(cell_to_char(text, 3), 3, "你 的第 1 个 cell 是 char 3");
    assert_eq!(cell_to_char(text, 4), 3, "你 的第 2 个 cell 仍是 char 3");
    assert_eq!(cell_to_char(text, 5), 4, "好 的第 1 个 cell 是 char 4");
    // TextPosition 构造与排序。
    let a = TextPosition {
        entry_id: EntryId(1),
        offset: 3,
    };
    let b = TextPosition {
        entry_id: EntryId(1),
        offset: 5,
    };
    assert!(a < b);
}

/// §PointerHit ④：wrap_with_semantic 一次 layout 产出语义映射。
/// - 视觉行数与语义行数严格一致（不二次折行）；
/// - 首行 decor = 逻辑行前缀宽度，续行 decor = 0（P0-2 修复）。
#[test]
fn wrap_with_semantic_produces_exact_mapping() {
    use crate::tui::scroll::EntryId;
    let entry_id = EntryId(1);
    // 模拟 User 消息：rail "│ " + "you  " = 7 cell 装饰，正文 "hello"。
    let line = Line::from(vec![
        Span::styled("│ ", Style::default()),
        Span::styled("you  ", Style::default()),
        Span::styled("hello", Style::default()),
    ]);
    let semantic = SemanticLine {
        text: "hello".to_string(),
        decor_cells: 7,
    };
    let rows = wrap_with_semantic(vec![line], vec![None], &[semantic], 80, entry_id);
    assert_eq!(rows.len(), 1, "短文本单行");
    let row = &rows[0];
    let (eid, char_start, text, decor) = row.semantic.as_ref().expect("必须有语义映射");
    assert_eq!(*eid, entry_id);
    assert_eq!(*char_start, 0, "首行语义从 entry 偏移 0 开始");
    assert_eq!(text, "hello");
    assert_eq!(*decor, 7, "短文本首行 decor 必须是 rail 宽度（P0-2 修复）");

    // 长文本换行：语义行数 == 视觉行数，续行 decor=0。
    let long = "x".repeat(90); // 90 cell，宽 40 → 3 行。
    let line2 = Line::from(vec![
        Span::styled("│ ", Style::default()),
        Span::styled("you  ", Style::default()),
        Span::styled(long.clone(), Style::default()),
    ]);
    let semantic2 = SemanticLine {
        text: long.clone(),
        decor_cells: 7,
    };
    let rows = wrap_with_semantic(vec![line2], vec![None], &[semantic2], 40, entry_id);
    // 宽 40：首行内容容量 = 40-7 = 33 cell → "x"×33；续行 40 cell 各 40、17。
    assert_eq!(rows.len(), 3, "90 cell 内容宽 40 → 3 视觉行");
    // 首行 decor=7，续行 decor=0。
    assert_eq!(rows[0].semantic.as_ref().unwrap().3, 7, "首行 decor=rail");
    assert_eq!(rows[1].semantic.as_ref().unwrap().3, 0, "续行 decor=0");
    assert_eq!(rows[2].semantic.as_ref().unwrap().3, 0, "末行 decor=0");
    // 语义文本拼接 = 原文。
    let joined: String = rows
        .iter()
        .filter_map(|r| r.semantic.as_ref().map(|(_, _, t, _)| t.as_str()))
        .collect();
    assert_eq!(joined, long, "语义拼接必须等于原文（不丢字）");
}

/// 端到端：任意字符可选中（非整行）。
/// 用 plan_window 的 semantic_rows 模拟 hit_text 的列→offset 映射，再走
/// ViewModel::selected_text 的 char 级提取。选中中间 3 个字符 → 精确返回。
#[test]
fn arbitrary_char_selection_is_char_precise() {
    use crate::tui::interaction::{TextPosition, cell_to_char, chars_to_cells};
    use crate::tui::model::LineKind;
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "hello world");
    let mut cache = HashMap::new();
    let plan = plan_window(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    let (entry_id, char_start, text, decor) = plan.semantic_rows[0]
        .as_ref()
        .expect("第一行必须有语义映射");
    assert_eq!(text, "hello world");
    // 模拟 hit_text：视觉 cell 列 → 语义 offset。
    // 屏幕列 = rect.x + decor + 目标字符前的 cell 宽度。
    let to_offset = |screen_col: usize| -> usize {
        let semantic_col = screen_col.saturating_sub(*decor);
        cell_to_char(text, semantic_col) + *char_start
    };
    // 屏幕列指向 "world" 的开头（decor + "hello " = 7 + 6 = 13 cell 列）。
    let w_col = *decor + chars_to_cells("hello ", 6);
    let w_offset = to_offset(w_col);
    assert_eq!(w_offset, 6, "w 的 offset 应为 6");
    // 选中 "wor"（offset 6..9）。
    view.selection_start(TextPosition {
        entry_id: *entry_id,
        offset: w_offset,
    });
    view.selection_update(TextPosition {
        entry_id: *entry_id,
        offset: w_offset + 3,
    });
    view.selection_end();
    assert_eq!(view.selected_text(), "wor", "必须精确选中 3 个字符，非整行");
    // CJK：选中 2 个汉字。
    view.push_line(LineKind::Assistant, "你好世界");
    let mut cache2 = HashMap::new();
    let plan2 = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache2,
    );
    let (_, _, text2, _) = plan2.semantic_rows[1]
        .as_ref()
        .expect("第二行必须有语义映射");
    let (e2, _, _, _) = plan2.semantic_rows[1].as_ref().unwrap();
    assert_eq!(text2, "你好世界");
    view.selection_start(TextPosition {
        entry_id: *e2,
        offset: 1,
    });
    view.selection_update(TextPosition {
        entry_id: *e2,
        offset: 3,
    });
    view.selection_end();
    assert_eq!(view.selected_text(), "好世", "CJK 按 char 精确选中");
}
