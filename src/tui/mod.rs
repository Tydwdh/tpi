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
//!   plan 集中在右侧 Todo；footer 展示 workspace/model/usage/状态；编辑器硬件光标。
//! - OMP 语义主题集中在 `theme` 模块。
//! - Markdown 渲染（pulldown-cmark）：assistant/用户消息的加粗、行内代码、
//!   代码块、列表、引用、链接；按条目版本缓存渲染结果，流式增量只失效最后一条。

pub mod editor;
pub mod effect;
pub mod event;
pub mod highlight;
pub mod interaction;
pub mod keymap;
mod markdown;
pub mod model;
pub mod paste;
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
use ratatui::widgets::{Paragraph, Widget};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use markdown::{LinkRange, RenderedMarkdown, render_markdown, render_markdown_detailed};
use model::{Entry, LineKind, StatusLine, ToolCard, ViewModel};
use scroll::{EntryId, ScrollMode};
use tool_card::{card_semantic_rows, tool_card_lines};

/// 帧合并间隔（§16.1：高频事件按帧合并，而不是事件数量等于 draw 次数）。
/// 33 ms ≈ 30 FPS：文本流式/拖选在此帧率下视觉已顺滑，同时把 Windows
/// 终端上的全量重绘与 CSI I/O 压到 60 FPS 的一半（性能：终端卡顿）。
pub const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// 活动区高度：约 2/5 屏，随终端行数自适应。
///
/// - 下限 12 行（小终端仍可读）；
/// - 上限为 `rows - 12`（给 footer/input 区域留空间），大终端自动拓展。
pub fn activity_height(rows: u16) -> u16 {
    if rows < 12 {
        return rows.max(1);
    }
    let proportional = ((u32::from(rows) * 2) / 5) as u16;
    proportional
        .clamp(12, rows.saturating_sub(12).max(12))
        .min(rows)
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
    ("theme", "切换主题（UI + 代码高亮）"),
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
    /// 右侧边栏屏幕矩形（§用户诉求：大纲行 hit-test）。
    sidebar_rect: Option<Rect>,
    /// 边栏每个可视行对应的大纲 entry id（§用户诉求：点击跳转）。
    sidebar_hits: Vec<Option<EntryId>>,
    /// 当前窗口起始全局行（scrollbar 比例用，§24）。
    window_start: usize,
    /// 内容总 visual 行数（scrollbar 比例用，§24）。
    total_rows: usize,
    /// 窗口内每行的语义信息（复制源；与 window 等长）。§InteractionRefactor：
    /// copy 从语义文本提取，不反推渲染结果（§成熟化：含可点击链接范围）。
    semantic_rows: Vec<Option<RowSemantic>>,
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
    /// Markdown 渲染缓存：条目 version → 渲染结果（行 + 链接范围）。
    md_cache: HashMap<(u64, u16), RenderedMarkdown>,
    /// wrap 结果缓存（§性能）：历史 entry (id, width) → 折行结果，跨帧复用；
    /// transcript_revision 变化或 resize 时清空。
    wrap_cache: HashMap<(EntryId, u16), Arc<Vec<WrappedRow>>>,
    /// 上次 draw 的 transcript 结构版本（wrap 缓存失效依据）。
    last_transcript_revision: u64,
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
    /// 最近一帧右侧边栏矩形（§用户诉求：大纲行 hit-test）。
    last_sidebar_rect: Option<Rect>,
    /// 边栏每个可视行对应的大纲 entry id（§用户诉求：点击跳转）。
    last_sidebar_hits: Vec<Option<EntryId>>,
    /// 最近一帧窗口内每行的语义信息（§InteractionRefactor：Ctrl+C 复制源，
    /// 从语义文本提取，不反推渲染结果）。与窗口行一一对应（§成熟化：
    /// 含可点击链接范围）。
    semantic_rows: Vec<Option<RowSemantic>>,
}
impl Renderer {
    /// 运行时切换主题（/theme 菜单）：更新主题并清空渲染缓存——
    /// md_cache/wrap_cache 的 key 不含 theme，颜色随主题变化必须失效。
    pub fn set_theme(&mut self, theme: theme::Theme) {
        self.theme = theme;
        self.md_cache.clear();
        self.wrap_cache.clear();
    }
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
            wrap_cache: HashMap::new(),
            last_transcript_revision: 0,
            cache_width: 0,
            last_transcript_rect: None,
            last_row_hits: Vec::new(),
            hits_valid: false,
            last_scrollbar_rect: None,
            last_sidebar_rect: None,
            last_sidebar_hits: Vec::new(),
            semantic_rows: Vec::new(),
        })
    }

    /// §用户诉求：最近一帧右侧边栏矩形（鼠标 hit-test；无 = 边栏关闭）。
    pub fn sidebar_rect(&self) -> Option<Rect> {
        self.last_sidebar_rect
    }

    /// §用户诉求：屏幕坐标 → 边栏大纲 entry id（命中大纲行返回可跳转目标）。
    /// 仅在边栏矩形内有效；行内任意列都算命中（整行可点）。
    pub fn sidebar_hit(&self, column: u16, row: u16) -> Option<EntryId> {
        let rect = self.last_sidebar_rect?;
        if column < rect.x || row < rect.y || row >= rect.y + rect.height {
            return None;
        }
        let index = (row - rect.y) as usize;
        self.last_sidebar_hits.get(index).cloned().flatten()
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
        let row_semantic = self.semantic_rows.get(index)?.as_ref()?;
        // 屏幕列 → 语义内 cell 列：先扣 rail/icon 前缀宽度（§PointerHit：`│ AI`
        // 等前缀不参与语义偏移），再按 cell 宽度映射 char（CJK/emoji 精确）。
        let col = column.saturating_sub(rect.x) as usize;
        let semantic_col = col.saturating_sub(row_semantic.decor);
        let char_off = crate::tui::interaction::cell_to_char(&row_semantic.text, semantic_col);
        Some(crate::tui::interaction::TextPosition {
            entry_id: row_semantic.entry_id,
            offset: row_semantic.char_start + char_off,
        })
    }

    /// §成熟化：屏幕坐标 → 可点击链接（点击链接文本区域返回 URL）。
    /// 命中优先级：文本位置 → 链接范围（char 坐标基于语义文本）。
    pub fn link_at(&self, column: u16, row: u16) -> Option<String> {
        let rect = self.last_transcript_rect?;
        if column < rect.x || row < rect.y || row >= rect.y + rect.height {
            return None;
        }
        let index = (row - rect.y) as usize;
        let row_semantic = self.semantic_rows.get(index)?.as_ref()?;
        if row_semantic.links.is_empty() {
            return None;
        }
        // 屏幕列 → 语义内 char 偏移（与 hit_text 同一映射）。
        let col = column.saturating_sub(rect.x) as usize;
        let semantic_col = col.saturating_sub(row_semantic.decor);
        let char_off = crate::tui::interaction::cell_to_char(&row_semantic.text, semantic_col);
        let links = &row_semantic.links;
        // 命中 char 落在 [start, end) 即命中；列落在链接文本后半格（2-cell 字符
        // 右半）也命中该链接（cell_to_char 已归一到前一 char）。
        let char_off_hi = char_off.saturating_add(1);
        links
            .iter()
            .find(|l| {
                (char_off >= l.start && char_off < l.end)
                    || (char_off_hi > l.start && char_off_hi <= l.end)
            })
            .map(|l| l.url.clone())
    }

    /// 距上次 draw 是否已过帧间隔（§16.1：FRAME_INTERVAL 合并）。
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
        let mut wrap_cache = std::mem::take(&mut self.wrap_cache);
        let mut cache_width = self.cache_width;
        let mut committed = self.committed_lines;
        let mut overflow: Vec<Line<'static>> = Vec::new();
        let mut new_committed = committed;
        let scrollback = self.scrollback;
        let mut plan_out: Option<FramePlan> = None;
        // §性能：transcript 结构变化（新增/trim/折叠切换）→ 清空 wrap 缓存。
        // 流式期间 revision 不变 → 历史行 wrap 结果跨帧复用。
        let transcript_revision = view.transcript_revision;
        if transcript_revision != self.last_transcript_revision {
            wrap_cache.clear();
            self.last_transcript_revision = transcript_revision;
        }
        self.driver.draw(|frame| {
            let FramePlan {
                window,
                row_hits,
                transcript_rect,
                scrollbar_rect,
                sidebar_rect,
                sidebar_hits,
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
                    wrap_cache: &mut wrap_cache,
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
                sidebar_rect,
                sidebar_hits,
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
            self.last_sidebar_rect = plan.sidebar_rect;
            self.last_sidebar_hits = plan.sidebar_hits;
            // §InteractionRefactor：保存每窗口行的语义文本（复制源）。
            // 从语义文本提取复制，不反推渲染结果（无 padding/装饰污染）。
            self.semantic_rows = plan.semantic_rows;
        }
        // §16.1：闭合且不再变化的行提交到 scrollback（活动区上方；仅 inline）。
        // fullscreen：alternate screen 内直接绘制整个终端，无 scrollback。
        if scrollback && !overflow.is_empty() {
            let lines = std::mem::take(&mut overflow);
            for chunk in lines.chunks(u16::MAX as usize) {
                let chunk = chunk.to_vec();
                let height = u16::try_from(chunk.len()).unwrap_or(u16::MAX);
                if self
                    .driver
                    .insert_before(height, |buf| {
                        let area = Rect::new(0, 0, buf.area().width, height);
                        ratatui::widgets::Paragraph::new(Text::from(chunk.clone()))
                            .render(area, buf);
                    })
                    .is_err()
                {
                    // 终端不支持 scrolling region：降级为活动区内部滚动（footer 提示）。
                    self.scrollback = false;
                    break;
                }
            }
        }
        self.committed_lines = new_committed;
        self.md_cache = cache;
        self.wrap_cache = wrap_cache;
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
/// 自下而上：输入区（多行）→ footer → 转录区；计划只在右侧栏展示。
struct RenderContext<'a> {
    markdown_cache: &'a mut HashMap<(u64, u16), RenderedMarkdown>,
    /// wrap 结果缓存（§性能：历史 entry 折叠/内容不变 → 跨帧复用，
    /// 避免每帧对全量历史逐字符重建 span）。key = (entry_id, width)。
    wrap_cache: &'a mut HashMap<(EntryId, u16), Arc<Vec<WrappedRow>>>,
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
        wrap_cache,
        cache_width,
        committed,
        scrollback,
        mode,
    } = context;
    let area = frame.area();
    // §用户诉求：右侧边栏（todo + 用户消息大纲）。打开时横向切分——主区让出
    // SIDEBAR_WIDTH 列，边栏占右侧整列高（贯穿 transcript 到 footer）。
    let (main_area, sidebar_area) = if view.sidebar.open {
        let main_w = area
            .width
            .saturating_sub(crate::tui::model::SIDEBAR_WIDTH)
            .max(1);
        (
            Rect::new(area.x, area.y, main_w, area.height),
            Some(Rect::new(
                area.x + main_w,
                area.y,
                area.width - main_w,
                area.height,
            )),
        )
    } else {
        (area, None)
    };
    // 宽度变化（resize 或 sidebar 开关）→ Markdown 渲染缓存失效，提交位置重置
    // 为当前窗口起点（§16.1：已提交到 scrollback 的历史 immutable，resize 后只重排活动区）。
    let reset_committed = *cache_width != main_area.width;
    if reset_committed {
        cache.clear();
        // §性能：wrap 结果按宽度缓存，resize 后一并失效（key 含 width）。
        wrap_cache.clear();
        *cache_width = main_area.width;
    }

    let input_rows = input_area_rows(view, main_area.width);
    // §视觉瘦身：去掉常驻 Header（信息与 footer 重复），transcript 上移到顶部。
    // 自下而上 = footer(1) → input(1..8) → transcript(Min)。Todo 只占侧边栏。
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(1)];
    constraints.push(Constraint::Length(input_rows));
    // §美化：footer 上方 1-cell 分隔线，把底部状态栏锚定成独立区域。
    constraints.push(Constraint::Length(1)); // footer 分隔线
    constraints.push(Constraint::Length(1)); // footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(main_area);
    let mut idx = 0;
    let trans_area = chunks[idx];
    idx += 1;
    let input_area = chunks[idx];
    idx += 1;
    let footer_rule_area = chunks[idx];
    idx += 1;
    let footer_area = chunks[idx];

    // §视觉瘦身：scrollbar 预留 1 列（inline 兼容模式不预留）。
    let scrollbar_enabled = mode == terminal::ViewMode::Fullscreen && trans_area.width >= 2;
    let transcript_width = if scrollbar_enabled {
        trans_area.width - 1
    } else {
        trans_area.width
    };

    // footer 分隔线（muted 横线；与左竖线语言一致，只作锚定不喧宾）。
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(footer_rule_area.width as usize),
            Style::default().fg(theme.border),
        )),
        footer_rule_area,
    );
    draw_footer(frame, footer_area, view, theme, scrollback, mode);

    // §用户诉求（表格后文字复制）：把本次渲染的 markdown 内容宽度写回 view——
    // canonical_semantic_text 用它渲染语义文本，与 renderer 的 hit 坐标系
    // 同宽才不 offset 错位（表格渲染宽度敏感）。
    view.semantic_width = Some((transcript_width as usize).saturating_sub(RAIL_WIDTH));
    let plan = plan_window(
        view,
        theme,
        transcript_width,
        trans_area.height,
        *committed,
        reset_committed,
        cache,
        wrap_cache,
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

    // 操作型 Modal（§42：/help /settings /doctor 等；覆盖显示，Esc 关闭）。
    // §用户诉求：/sessions、/theme 悬浮窗（Modal）是“说明/预览小窗”——靠上
    // 小框，与底部菜单列表错开；其余 Modal 保持居中大框。
    // §侧边栏：浮层横向布局一律基于 main_area，打开侧边栏时不覆盖边栏区域。
    // 先画 Modal 再画菜单：菜单浮在最上，列表永不被悬浮窗盖住。
    let is_menu_browser = matches!(
        view.menu.as_ref().map(|m| m.kind),
        Some(model::MenuKind::Session) | Some(model::MenuKind::Theme)
    );
    if view.modal.is_some() {
        let w = modal_width(main_area.width);
        let h = if is_menu_browser {
            (10u16).min(trans_area.height.saturating_sub(2).max(1))
        } else {
            modal_height(trans_area.height)
        };
        let x = main_area.x + (main_area.width.saturating_sub(w)) / 2;
        let y = if is_menu_browser {
            trans_area.y + 1
        } else {
            trans_area.y + trans_area.height.saturating_sub(h) / 2
        };
        draw_modal(frame, Rect::new(x, y, w, h), view, theme);
    }

    // 命令补全菜单浮层（覆盖在转录区上方，§16.2 之外的小浮层）。
    // §用户诉求：菜单美化——高度含 2 边框 + 1 快捷键提示行；宽度按内容
    // 自适应（会话/主题菜单宽，命令/文件菜单窄）。
    // §用户诉求（菜单滚动）：菜单内容有合理上限（8 项 + 上下 `…` 滚动，
    // fzf 式标准行为）——命令菜单 14 项、/sessions 几十项时滚动而非撑满
    // 屏幕；项少（如 3 个文件）时全显示无省略号。draw_menu 内部已有
    // 窗口跟随选中项的滚动逻辑，这里只定高度。
    if let Some(menu) = &view.menu {
        let h = (menu.items.len() as u16 + 1)
            .min(8)
            .saturating_add(3)
            .min(trans_area.height.max(4));
        let w = menu_floating_width(menu, main_area.width);
        let y = trans_area.y + trans_area.height.saturating_sub(h);
        draw_menu(frame, Rect::new(main_area.x, y, w, h), view, theme);
    }

    // 搜索框（§14：Ctrl+F；悬浮在转录区顶部）。
    if let Some(search) = &view.search {
        let w = main_area.width.clamp(1, 56);
        let x = main_area.x + (main_area.width.saturating_sub(w)) / 2;
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
        let w = modal_width(main_area.width);
        let h = modal_height(trans_area.height);
        let x = main_area.x + (main_area.width.saturating_sub(w)) / 2;
        let y = trans_area.y + trans_area.height.saturating_sub(h) / 2;
        draw_overlay(frame, Rect::new(x, y, w, h), view, theme);
    }

    draw_input(frame, input_area, view, theme);

    // §用户诉求：右侧边栏（todo + 用户消息大纲）。贯穿整个主区高度。
    // sidebar_rect = 整个边栏矩形（供鼠标 hit-test）；draw_sidebar 返回
    // 可视行 → 大纲 entry id（点击跳转）。
    let sidebar_hits = match sidebar_area {
        Some(rect) => draw_sidebar(frame, rect, view, theme),
        None => Vec::new(),
    };
    let sidebar_rect = sidebar_area;
    FramePlan {
        window: Vec::new(),
        row_hits: plan.row_hits,
        scrollbar_rect,
        sidebar_rect,
        sidebar_hits,
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
#[allow(clippy::too_many_arguments)] // §性能：布局输入 + 双缓存（md/wrap）。
fn plan_window(
    view: &mut ViewModel,
    theme: theme::Theme,
    width: u16,
    area_h: u16,
    committed: usize,
    reset_committed: bool,
    cache: &mut HashMap<(u64, u16), RenderedMarkdown>,
    wrap_cache: &mut HashMap<(EntryId, u16), Arc<Vec<WrappedRow>>>,
) -> FramePlan {
    let width = width.max(1) as usize;
    // 输入、光标、footer 或动画变化时 transcript 通常完全没变。旧路径即使
    // wrap-cache 全命中，也会先为全部历史重建 Markdown 逻辑行，再把结果丢掉；
    // 长会话中一次按键因此仍可能耗费几十毫秒。缓存完整时直接复用历史的
    // wrapped rows，只构建确实会变化的 live 区。
    let historical_ids: Vec<EntryId> = view.transcript.iter().map(Entry::id).collect();
    let history_cache_complete = historical_ids
        .iter()
        .all(|id| wrap_cache.contains_key(&(*id, width as u16)));
    let mut wrapped_by_entry: Vec<(EntryId, Arc<Vec<WrappedRow>>)> = Vec::new();
    if history_cache_complete {
        wrapped_by_entry.reserve(historical_ids.len() + 3);
        for id in historical_ids {
            if let Some(rows) = wrap_cache.get(&(id, width as u16)) {
                wrapped_by_entry.push((id, Arc::clone(rows)));
            }
        }
        for (id, logical, hits, semantic_lines) in
            build_live_transcript_text(view, theme, cache, width)
        {
            let rows = Arc::new(wrap_with_semantic(
                logical,
                hits,
                &semantic_lines,
                width,
                id,
            ));
            wrapped_by_entry.push((id, rows));
        }
    } else {
        // 结构变化/resize：重建一次全部 entry 并填充缓存。
        let per_entry = build_transcript_text(view, theme, cache, width);
        let live_ids: std::collections::HashSet<EntryId> = view
            .live
            .tool_order
            .iter()
            .filter_map(|id| view.live.tools.get(id).map(|t| t.entry_id))
            .chain(view.live.assistant.iter().map(|m| m.entry_id))
            .chain(view.live.reasoning.iter().map(|m| m.entry_id))
            .collect();
        wrapped_by_entry.reserve(per_entry.len());
        for (id, logical, hits, semantic_lines) in per_entry {
            let cacheable = !live_ids.contains(&id);
            let key = (id, width as u16);
            let rows = if cacheable {
                if let Some(hit) = wrap_cache.get(&key) {
                    Arc::clone(hit)
                } else {
                    let rows = Arc::new(wrap_with_semantic(
                        logical,
                        hits,
                        &semantic_lines,
                        width,
                        id,
                    ));
                    wrap_cache.insert(key, Arc::clone(&rows));
                    rows
                }
            } else {
                Arc::new(wrap_with_semantic(
                    logical,
                    hits,
                    &semantic_lines,
                    width,
                    id,
                ))
            };
            wrapped_by_entry.push((id, rows));
        }
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
    let mut semantic_rows: Vec<Option<RowSemantic>> = Vec::new();
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
    // §性能：search/active_hit 高亮在**窗口层**应用（wrap 缓存只缓存净内容，
    // 这些装饰状态变化无需失效缓存；原 build 阶段的高亮移到这里，语义等价
    // ——按行 entry_id / 行 hit 逐行判断）。补空格前应用，padding 一并高亮。
    if let Some(search) = &view.search
        && !search.hits.is_empty()
    {
        let hit_ids: std::collections::HashSet<EntryId> = search.hits.iter().copied().collect();
        for (i, row) in semantic_rows.iter().enumerate() {
            if let Some(row) = row
                && hit_ids.contains(&row.entry_id)
                && let Some(line) = window.get_mut(i)
            {
                *line = Line::from(
                    line.spans
                        .iter()
                        .map(|s| {
                            Span::styled(
                                s.content.clone(),
                                s.style.add_modifier(Modifier::UNDERLINED),
                            )
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
    if let Some(active) = &view.active_hit {
        let active_bg = theme.surface;
        for (i, hit) in row_hits.iter().enumerate() {
            if hit.as_ref().map(|(t, _)| t) == Some(active)
                && let Some(line) = window.get_mut(i)
            {
                *line = Line::from(
                    line.spans
                        .iter()
                        .map(|s| {
                            Span::styled(
                                s.content.clone(),
                                s.style.bg(active_bg).add_modifier(Modifier::BOLD),
                            )
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
    // §用户诉求：卡片整行背景填满——有背景色（卡片/surface 或 diff 红绿）的
    // 行补空格到满宽，避免「只有文字处有背景」的碎片感。
    // BUG 修复：背景只取**行首 span**（rail/icon/prompt 前缀的 panel 底）或
    // **Line 级样式**（diff 行的红绿底）。行内元素背景绝不能被用来填
    // padding——否则行尾空白会泄漏行内元素的底色一直铺到右缘。
    for line in window.iter_mut() {
        let bg = line
            .spans
            .first()
            .and_then(|s| s.style.bg)
            .or(line.style.bg);
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
            let Some(row) = row else {
                continue;
            };
            let row_lo = row.char_start;
            let row_hi = row.char_start + row.text.chars().count();
            let row_entry = row.entry_id;
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
            let cell_from =
                crate::tui::interaction::chars_to_cells(&row.text, from_sel) + row.decor;
            let cell_to = crate::tui::interaction::chars_to_cells(&row.text, to_sel) + row.decor;
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
        // §用户诉求：边栏矩形/命中由 render_frame 计算（需要屏幕坐标）。
        sidebar_rect: None,
        sidebar_hits: Vec::new(),
        window_start,
        total_rows: heights.iter().sum(),
        semantic_rows,
        overflow,
        committed_after,
    }
}

/// 测试便捷封装：plan_window 带独立临时 wrap 缓存（语义等同旧行为）。
/// 生产路径在 Renderer 内复用缓存；单测逐次独立调用即可。
#[cfg(test)]
fn plan_window_simple(
    view: &mut ViewModel,
    theme: theme::Theme,
    width: u16,
    area_h: u16,
    committed: usize,
    reset_committed: bool,
    cache: &mut HashMap<(u64, u16), RenderedMarkdown>,
) -> FramePlan {
    plan_window(
        view,
        theme,
        width,
        area_h,
        committed,
        reset_committed,
        cache,
        &mut HashMap::new(),
    )
}

/// 每窗口行的语义信息（复制源；hit-test 与复制共用，§InteractionRefactor）。
/// `links` 为该视觉行内可点击链接（char 偏移基于 `text`；§成熟化）。
#[derive(Debug, Clone)]
struct RowSemantic {
    entry_id: EntryId,
    /// 该行在 entry 语义流中的起始 char 偏移。
    char_start: usize,
    /// 该行无装饰语义文本。
    text: String,
    /// 视觉渲染中前缀装饰占用的 cell 宽度。
    decor: usize,
    /// 可点击链接（char 范围基于 `text`）。
    links: Vec<LinkRange>,
}

/// 一次 wrap 的视觉行：line + hit + 语义映射（§PointerHit：视觉换行处
/// 即语义映射断点，不再进行第二次 semantic wrap）。
#[derive(Debug, Clone)]
struct WrappedRow {
    line: Line<'static>,
    hit: Option<HitRange>,
    /// 该视觉行的语义信息：None = 纯装饰/空行（不可选）。
    semantic: Option<RowSemantic>,
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
        let logical_links: &[LinkRange] = semantic.map(|s| s.links.as_slice()).unwrap_or(&[]);
        let decor = semantic.map(|s| s.decor_cells).unwrap_or(0);
        // §用户诉求：逻辑行有视觉前缀（rail/icon）时，折行/换行后的续行
        // 补同一前缀，最左竖线不中断。前缀 span 在语义文本之外（decor）。
        let rail = semantic.and_then(|s| s.rail.clone());
        let rail_w = rail.as_ref().map_or(0, |s| {
            unicode_width::UnicodeWidthStr::width(s.content.as_ref())
        });
        let line_start = entry_offset;
        let mut line_consumed = 0usize;
        // 逻辑行内 cell 位置（判断是否越过 decor；只在逻辑行首段有效）。
        let mut line_cell = 0usize;
        // 首段含 decor，换行后为续行（无 decor）。
        let mut is_line_start = true;

        if line.spans.is_empty() {
            // 空行：不进入 cur，直接产出一行。
            if !cur.is_empty() {
                push_wrapped_row(
                    &mut out,
                    &mut cur,
                    &mut cur_hit,
                    &mut cur_sem_start,
                    &mut cur_sem_text,
                    &mut cur_decor,
                    &mut cur_w,
                    entry_id,
                    line_start,
                    logical_links,
                );
            }
            out.push(WrappedRow {
                line: Line::default(),
                hit,
                semantic: semantic.map(|s| RowSemantic {
                    entry_id,
                    char_start: line_start,
                    text: s.text.clone(),
                    decor,
                    links: Vec::new(),
                }),
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
                        push_wrapped_row(
                            &mut out,
                            &mut cur,
                            &mut cur_hit,
                            &mut cur_sem_start,
                            &mut cur_sem_text,
                            &mut cur_decor,
                            &mut cur_w,
                            entry_id,
                            line_start,
                            logical_links,
                        );
                        // 续段补 rail（竖线连续；decor 仅 rail 宽，语义继续）。
                        if let Some(rail_span) = &rail {
                            cur.push(rail_span.clone());
                            cur_w = rail_w;
                            cur_decor = rail_w;
                        }
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
                // 超宽折行：先 flush 当前视觉行，续行补 rail。
                if cur_w + w > width && !cur.is_empty() {
                    push_wrapped_row(
                        &mut out,
                        &mut cur,
                        &mut cur_hit,
                        &mut cur_sem_start,
                        &mut cur_sem_text,
                        &mut cur_decor,
                        &mut cur_w,
                        entry_id,
                        line_start,
                        logical_links,
                    );
                    cur_sem_start = None;
                    cur_hit = hit.clone();
                    is_line_start = false;
                    if let Some(rail_span) = &rail {
                        cur.push(rail_span.clone());
                        cur_w = rail_w;
                        cur_decor = rail_w;
                    }
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
        push_wrapped_row(
            &mut out,
            &mut cur,
            &mut cur_hit,
            &mut cur_sem_start,
            &mut cur_sem_text,
            &mut cur_decor,
            &mut cur_w,
            entry_id,
            line_start,
            logical_links,
        );
        // 逻辑行结束：entry 语义流推进 = 内容字符数 + 换行位。
        entry_offset += line_consumed + 1;
    }
    push_wrapped_row(
        &mut out,
        &mut cur,
        &mut cur_hit,
        &mut cur_sem_start,
        &mut cur_sem_text,
        &mut cur_decor,
        &mut cur_w,
        entry_id,
        entry_offset,
        &[],
    );
    out
}

/// flush 当前视觉行：把累积的 span/semantic/links 打包成 WrappedRow。
/// `line_start` 是当前逻辑行在 entry 语义流的起始偏移；链接按该行内
/// 内容偏移切分（视觉行内 char 偏移归零）。
#[allow(clippy::too_many_arguments)]
fn push_wrapped_row(
    out: &mut Vec<WrappedRow>,
    cur: &mut Vec<Span<'static>>,
    cur_hit: &mut Option<HitRange>,
    cur_sem_start: &mut Option<usize>,
    cur_sem_text: &mut String,
    cur_decor: &mut usize,
    cur_w: &mut usize,
    entry_id: EntryId,
    line_start: usize,
    logical_links: &[LinkRange],
) {
    if cur.is_empty() {
        return;
    }
    // §修复：必须 take（消费）cur_sem_start——此前只 take 了 cur_sem_text，
    // 残留的起始偏移泄漏到下一逻辑行（char_start 全变 0 → 卡片内选中
    // 会误全选所有行）。take 保证每次 flush 后起始偏移归零重置。
    let sem_start = cur_sem_start.take();
    let links = sem_start.map(|start| {
        let lo = start.saturating_sub(line_start);
        slice_links(logical_links, lo, cur_sem_text.chars().count())
    });
    out.push(WrappedRow {
        line: Line::from(std::mem::take(cur)),
        hit: cur_hit.take(),
        semantic: sem_start.map(|start| RowSemantic {
            entry_id,
            char_start: start,
            text: std::mem::take(cur_sem_text),
            decor: *cur_decor,
            links: links.unwrap_or_default(),
        }),
    });
    *cur_w = 0;
    *cur_decor = 0;
}

/// 逻辑行链接 → 视觉行链接：取 `[start, start+len)` 交集，偏移归零到视觉行。
fn slice_links(links: &[LinkRange], start: usize, len: usize) -> Vec<LinkRange> {
    links
        .iter()
        .filter_map(|l| {
            let lo = l.start.max(start);
            let hi = l.end.min(start.saturating_add(len));
            if lo < hi {
                Some(LinkRange {
                    start: lo - start,
                    end: hi - start,
                    url: l.url.clone(),
                })
            } else {
                None
            }
        })
        .collect()
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
/// - `links`：该逻辑行内可点击链接（char 偏移基于语义文本；§成熟化）。
/// - `rail`：该逻辑行的视觉前缀 span（rail/icon）。§用户诉求：长行换行后
///   最左竖线被截断——wrap 折行时把该前缀补到续行，竖线连续不中断。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticLine {
    text: String,
    decor_cells: usize,
    links: Vec<LinkRange>,
    rail: Option<Span<'static>>,
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

/// 构建一个"整行可点"的 hit（reasoning 折叠行）。
fn full_line_hit(target: HitTarget, width: u16) -> Option<HitRange> {
    Some((target, width))
}

/// 工具卡片是否"紧凑"：collapsed_lines==0 且未展开且带内容——只显示主行，
/// 卡片之间取消间隔（§用户诉求：紧凑）。
fn tool_card_compact(card: &ToolCard) -> bool {
    card.collapsed_lines == 0
        && !card.expanded
        && (card.diff.is_some() || card.output.is_some() || card.tail.is_some())
}

/// live 区首个组是否为 thinking/assistant 卡（历史末尾与 live 衔接时判定
/// 是否需要补间隔；§用户诉求：思考 / AI 输出卡与其他卡片保持一格间隔）。
fn live_starts_with_gap_card(view: &ViewModel) -> bool {
    view.live
        .reasoning
        .as_ref()
        .is_some_and(|m| !m.text.is_empty())
        || view
            .live
            .assistant
            .as_ref()
            .is_some_and(|m| !m.text.is_empty())
}

/// 历史末尾 entry 是否要求与 live 首组补间隔：compact 工具卡或 System/Tool
/// 文本行（这类 entry 的 gap 完全依赖下一组的类型；User/Assistant/Reasoning
/// 与展开态工具卡在历史循环里已无条件补间隔）。
fn history_tail_requires_gap(view: &ViewModel) -> bool {
    matches!(
        view.transcript.last(),
        Some(Entry::Tool { card, .. }) if tool_card_compact(card)
    ) || matches!(
        view.transcript.last(),
        Some(Entry::Message { line, .. })
            if matches!(line.kind, LineKind::System | LineKind::Tool)
    )
}

/// 把转录条目渲染为逻辑行（Message 按类型着色/加 rail；Tool 渲染为卡片）。
///
/// 返回按 entry 分组的结果（含 live 哨兵组，§7.2）。
fn build_transcript_text(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<(u64, u16), RenderedMarkdown>,
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
                    semantic_text: String,
                    links: Vec<LinkRange>| {
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
        // 有装饰前缀（rail/icon）时记住首个 span：wrap 折行后续行补同一前缀
        // （§用户诉求：换行竖线不截断）。纯装饰/空行（decor=0）无 rail。
        let rail = if decor_cells > 0 {
            line.spans.first().cloned()
        } else {
            None
        };
        lines.push(line);
        hits.push(hit);
        semantic.push(SemanticLine {
            text: semantic_text,
            decor_cells,
            links,
            rail,
        });
    };
    for (i, entry) in view.transcript.iter().enumerate() {
        let entry_id = entry.id();
        out.clear();
        hits.clear();
        match entry {
            Entry::Message { line, .. } => match line.kind {
                // §16.2 + §美化：用户消息 = opencode 式"左竖线 + 面板背景块"。
                // 竖线与 `you` 标签带 panel 背景（成为行内首个 bg span），
                // plan_window 的补空格机制把整行填满 panel → 形成"用户有底"，
                // 与 Assistant 的裸文本形成强烈角色层次。
                LineKind::User => {
                    // §codex：表格在 rail 前缀之后的内容宽度内布局（防止加 rail 后超宽）。
                    let content_width = width.saturating_sub(RAIL_WIDTH);
                    let (rendered, line_links) =
                        cached_markdown(cache, line.version, &line.text, theme, content_width);
                    let rail_style = Style::default()
                        .fg(theme.accent)
                        .bg(theme.panel)
                        .add_modifier(Modifier::BOLD);
                    for (i, rendered_line) in rendered.iter().enumerate() {
                        let mut spans = vec![Span::styled("┃ ", rail_style)];
                        if i == 0 {
                            spans.push(Span::styled("you  ", rail_style));
                        } else {
                            spans.push(Span::styled("     ", rail_style));
                        }
                        // §修复：正文 span 烙 panel 底（保留已有背景如 inline
                        // code）——否则文字区落到终端底色，与用户消息面板分离。
                        spans.extend(
                            rendered_line
                                .spans
                                .iter()
                                .map(|s| {
                                    let style = if s.style.bg.is_some() {
                                        s.style
                                    } else {
                                        s.style.bg(theme.panel)
                                    };
                                    Span::styled(s.content.clone(), style)
                                })
                                .collect::<Vec<_>>(),
                        );
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
                            line_links.get(i).cloned().unwrap_or_default(),
                        );
                    }
                }
                // §16.2：assistant 消息带左 rail + `AI` 标签（与用户消息呼应，
                // 形成清晰的双角色层次；正文 Markdown 渲染）。§美化：保持
                // 无背景裸文本（opencode：user 有底、assistant 裸文本对比）。
                LineKind::Assistant => {
                    // §codex：表格在 rail 前缀之后的内容宽度内布局。
                    let content_width = width.saturating_sub(RAIL_WIDTH);
                    let (rendered, line_links) =
                        cached_markdown(cache, line.version, &line.text, theme, content_width);
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
                            line_links.get(i).cloned().unwrap_or_default(),
                        );
                    }
                }
                // §用户诉求：thinking 与工具统一折叠——未展开显示前 N 行 +
                // "… 点击展开"，展开显示全文（点击行切换）。N = view.collapsed_lines
                // （[ui] collapsed_lines；0 = 只显示主行摘要）。
                // §用户诉求：thinking 用 markdown 渲染（代码块高亮，同 assistant）；
                // §美化：thinking 卡片化——panel 底 + 左竖线 + 整卡可点展开/折叠。
                LineKind::Reasoning => {
                    let collapsed = view.collapsed_lines;
                    // md 渲染预算：扣 rail（┃ 2 格）+ 首行前缀（◆ 思考 6 格）。
                    let (rendered, _links) = cached_markdown(
                        cache,
                        line.version,
                        &line.text,
                        theme,
                        width.saturating_sub(8).max(10),
                    );
                    let total_lines = rendered.len();
                    // 折叠态 = 未展开 且 (正文超折叠线 或 折叠线为 0)。
                    // 0 = 折叠态不显示任何正文行，只显示主行摘要。
                    let overflow = collapsed == 0 || total_lines > collapsed;
                    let expanded = view.is_reasoning_expanded(entry_id);
                    // 竖线 + 面板背景（与工具卡片主行同款 rail_style 结构）。
                    let rail_style = Style::default()
                        .fg(theme.accent)
                        .bg(theme.panel)
                        .add_modifier(Modifier::BOLD);
                    if overflow && !expanded {
                        // 折叠态：单行卡片「◆ 思考 · 共 N 行 · 点击展开」。
                        let hint = format!("◆ 思考 · 共 {total_lines} 行 · 点击展开");
                        push_hit(
                            &mut out,
                            &mut hits,
                            &mut semantic,
                            Line::from(vec![
                                Span::styled("┃ ", rail_style),
                                Span::styled(
                                    hint,
                                    Style::default()
                                        .fg(theme.muted)
                                        .bg(theme.panel)
                                        .add_modifier(Modifier::ITALIC),
                                ),
                            ]),
                            // 整行可点（end_col = 宽度，不再是 0）。
                            full_line_hit(HitTarget::Reasoning(entry_id), width as u16),
                            String::new(), // 折叠摘要不是可复制内容
                            Vec::new(),
                        );
                    } else {
                        // 展开态 / 未溢出：md 渲染行逐行显示（首行带 ◆ 图标，续行对齐）。
                        let clickable = overflow; // 只有溢出才可点击折叠
                        for (i, rendered_line) in rendered.iter().enumerate() {
                            // §用户诉求：续行 4 格对齐（原 6/8 格太宽）。
                            let prefix = if i == 0 { "◆ 思考 " } else { "    " };
                            let mut spans = vec![Span::styled("┃ ", rail_style)];
                            spans.push(Span::styled(
                                prefix,
                                Style::default()
                                    .fg(theme.muted)
                                    .bg(theme.panel)
                                    .add_modifier(Modifier::ITALIC),
                            ));
                            // md 行 span 烙 panel 底（与卡片面一致；inline code
                            // 无背景也随正文烙 panel，仅保留前景色）。
                            for s in &rendered_line.spans {
                                let mut style = s.style;
                                if style.bg.is_none() {
                                    style = style.bg(theme.panel);
                                }
                                spans.push(Span::styled(s.content.clone(), style));
                            }
                            let semantic_text = rendered_line
                                .spans
                                .iter()
                                .map(|s| s.content.as_ref())
                                .collect::<String>();
                            let hit = if clickable {
                                full_line_hit(HitTarget::Reasoning(entry_id), width as u16)
                            } else {
                                None
                            };
                            push_hit(
                                &mut out,
                                &mut hits,
                                &mut semantic,
                                Line::from(spans),
                                hit,
                                semantic_text,
                                Vec::new(),
                            );
                        }
                        if overflow {
                            // 折叠提示行：带 panel 底（与工具卡片折叠提示一致）。
                            push_hit(
                                &mut out,
                                &mut hits,
                                &mut semantic,
                                Line::from(vec![
                                    Span::styled("┃ ", rail_style),
                                    Span::styled(
                                        "思考 · 点击折叠",
                                        Style::default()
                                            .fg(theme.muted)
                                            .bg(theme.panel)
                                            .add_modifier(Modifier::ITALIC),
                                    ),
                                ]),
                                full_line_hit(HitTarget::Reasoning(entry_id), width as u16),
                                String::new(),
                                Vec::new(),
                            );
                        }
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
                            Vec::new(),
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
                            Vec::new(),
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
                                Vec::new(),
                            );
                        }
                    }
                }
            },
            Entry::Tool { card, .. } => {
                let card_id = card.id.clone();
                let card_lines = tool_card_lines(card, view.anim_tick, theme, width);
                // §PointerHit：语义文本与渲染行一一对应（P0-1：同一窗口）。
                let semantic_rows = card_semantic_rows(card);
                // §美化：整卡可点击——主行与内容行都带 Tool hit（轻点任意行
                // 展开/收缩；拖选仍是文本选择，不冲突）。内容行如有链接，
                // pointer_target 里链接优先于工具命中。
                for (i, line) in card_lines.into_iter().enumerate() {
                    let hit = Some((HitTarget::Tool(card_id.clone()), width as u16));
                    let semantic_text = semantic_rows.get(i).cloned().unwrap_or_default();
                    push_hit(
                        &mut out,
                        &mut hits,
                        &mut semantic,
                        line,
                        hit,
                        semantic_text,
                        Vec::new(),
                    );
                }
            }
        }
        // §美化：块间间隔一行（opencode marginTop=1 留白即分隔）。
        // User/Assistant/Reasoning 消息 + 工具卡片都间隔；System 提示与
        // 旧 Tool 文本行保持紧凑。
        // §用户诉求（紧凑）：collapsed_lines==0 且未展开的工具卡片只显示
        // 主行（无正文/提示），卡片间取消间隔；展开态（多行块）保留间隔
        // 以分隔。消息块间隔不变。
        // §用户诉求：思考卡片 / AI 输出卡片始终与其他卡片保持一格间隔——
        // 紧凑工具卡（或 System 行）后若紧接 thinking/assistant 卡，强制补间隔。
        // §修复：历史末尾与 live 首组（thinking/assistant）的间隔不在这里补，
        // 改由 build_live_group 在 live 首组前补——live 组每帧重建不缓存，历史
        // wrap 结果不再依赖 live 状态；否则 thinking 流式开始后 wrap_cache 命中
        // 无 gap 旧行，思考卡顶部缺空行（transcript_revision 流式期间不变）。
        let message_kind = match entry {
            Entry::Message { line, .. } => Some(line.kind),
            Entry::Tool { .. } => None,
        };
        let next_is_gap_card = matches!(
            view.transcript.get(i + 1),
            Some(Entry::Message { line, .. })
                if matches!(line.kind, LineKind::Reasoning | LineKind::Assistant)
        );
        let needs_gap = match message_kind {
            Some(LineKind::User | LineKind::Assistant | LineKind::Reasoning) => true,
            None => {
                let compact = matches!(entry, Entry::Tool { card, .. } if tool_card_compact(card));
                !compact || next_is_gap_card
            }
            Some(_) => next_is_gap_card,
        };
        if needs_gap {
            out.push(Line::default());
            hits.push(None);
            semantic.push(SemanticLine {
                text: String::new(),
                decor_cells: 0,
                links: Vec::new(),
                rail: None,
            });
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

/// 只构建 live 区，用于历史 wrap-cache 完整命中的常见重绘路径。
fn build_live_transcript_text(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<(u64, u16), RenderedMarkdown>,
    width: usize,
) -> Vec<EntryGroup> {
    let mut groups = Vec::new();
    let mut lines = Vec::new();
    let mut hits = Vec::new();
    let mut semantic = Vec::new();
    build_live_group(
        view,
        theme,
        cache,
        width,
        GroupBuffers {
            lines: &mut lines,
            hits: &mut hits,
            semantic: &mut semantic,
            groups: &mut groups,
        },
    );
    groups
}

/// 追加一行间隔（块间留白；text 空、不可选，与历史区 needs_gap 同构）。
fn push_gap_row(
    out: &mut Vec<Line<'static>>,
    hits: &mut Vec<Option<HitRange>>,
    semantic: &mut Vec<SemanticLine>,
) {
    out.push(Line::default());
    hits.push(None);
    semantic.push(SemanticLine {
        text: String::new(),
        decor_cells: 0,
        links: Vec::new(),
        rail: None,
    });
}

/// 渲染 live 区（§7.2）：reasoning（折叠策略同历史）→ assistant（Markdown）
/// → 运行中工具卡片（按启动顺序）。
fn build_live_group(
    view: &ViewModel,
    theme: theme::Theme,
    cache: &mut HashMap<(u64, u16), RenderedMarkdown>,
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
    // §修复：历史末尾 compact 工具卡 / System 行与 live 首组（thinking/
    // assistant）之间的间隔在 live 首组前补——live 组每帧重建不缓存，历史
    // wrap 结果不依赖 live 状态（否则 thinking 流式开始后 wrap_cache 命中
    // 无 gap 旧行，思考卡顶部缺空行；finalize 后 revision 变化才恢复）。
    if history_tail_requires_gap(view) && live_starts_with_gap_card(view) {
        push_gap_row(out, hits, semantic);
    }
    // §用户诉求：live 区组间与历史区同规则——思考卡片 / AI 输出卡片与
    // 相邻组保持一格间隔；紧凑工具卡之间取消间隔。prev_requires_gap = 上一组
    // 是否要求与后续间隔。
    let mut prev_requires_gap = false;
    // 流式 reasoning（折叠策略与历史一致：Alt+T 展开 / 点击查看）。
    // §PointerHit ⑤：reasoning 是独立 group（自己的稳定 EntryId）。
    // §美化：与历史 thinking 卡片同构——panel 底 + 左竖线。
    if let Some(msg) = &live.reasoning
        && !msg.text.is_empty()
    {
        let rail_style = Style::default()
            .fg(theme.accent)
            .bg(theme.panel)
            .add_modifier(Modifier::BOLD);
        // §修复：live thinking 与历史同用按条目状态（entry_id finalize 后沿用，
        // 点击收起稳定生效）——不再依赖全局 reasoning_visible。
        if view.is_reasoning_expanded(msg.entry_id) {
            // §用户诉求：live thinking 也用 markdown 渲染（代码块高亮同 assistant）。
            let (rendered, _links) = cached_markdown(
                cache,
                msg.version,
                &msg.text,
                theme,
                width.saturating_sub(8).max(10),
            );
            for (i, rendered_line) in rendered.iter().enumerate() {
                // §用户诉求：续行不再重复「◆ 思考」——首行带图标，续行 4 格对齐。
                let prefix = if i == 0 { "◆ 思考 " } else { "    " };
                let mut spans = vec![Span::styled("┃ ", rail_style)];
                spans.push(Span::styled(
                    prefix,
                    Style::default()
                        .fg(theme.muted)
                        .bg(theme.panel)
                        .add_modifier(Modifier::ITALIC),
                ));
                for s in &rendered_line.spans {
                    let mut style = s.style;
                    if style.bg.is_none() {
                        style = style.bg(theme.panel);
                    }
                    spans.push(Span::styled(s.content.clone(), style));
                }
                let semantic_text = rendered_line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                let line = Line::from(spans);
                let rail = line.spans.first().cloned();
                // §修复：展开态每行都可点击（点正文任意处收起）——与历史
                // reasoning 展开态一致；否则 live 展开后无任何 Reasoning hit，
                // 点击无法触发收起（“打开后关不上”）。
                hits.push(Some((HitTarget::Reasoning(msg.entry_id), width as u16)));
                // §修复：decor 按实际宽度动态计算（与 push_hit 一致）——此前
                // 硬编码 10，但 "┃ ◆ 思考 " 实际只有 9 cell（┃/◆ 各宽 1），
                // 内容首字会被误判为装饰，语义文本错位丢字。
                let line_width = unicode_width::UnicodeWidthStr::width(
                    line.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .as_str(),
                );
                let text_width = unicode_width::UnicodeWidthStr::width(semantic_text.as_str());
                let decor_cells = line_width.saturating_sub(text_width);
                out.push(line);
                semantic.push(SemanticLine {
                    text: semantic_text,
                    decor_cells,
                    links: Vec::new(),
                    rail,
                });
            }
            // 与历史 reasoning 展开态一致：末尾折叠提示行（可点击收起）。
            out.push(Line::from(vec![
                Span::styled("┃ ", rail_style),
                Span::styled(
                    "思考 · 点击折叠",
                    Style::default()
                        .fg(theme.muted)
                        .bg(theme.panel)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
            hits.push(Some((HitTarget::Reasoning(msg.entry_id), width as u16)));
            semantic.push(SemanticLine {
                text: String::new(),
                decor_cells: 0,
                links: Vec::new(),
                rail: None,
            });
        } else {
            out.push(Line::from(vec![
                Span::styled("┃ ", rail_style),
                Span::styled(
                    "◆ 思考 · 流式中…（点击展开）",
                    Style::default()
                        .fg(theme.muted)
                        .bg(theme.panel)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
            // §修复：end_col 必须是行宽（此前 0 被 hit_target 判为不可点，
            // live thinking 折叠行实际点不了）。
            hits.push(Some((HitTarget::Reasoning(msg.entry_id), width as u16)));
            semantic.push(SemanticLine {
                text: String::new(),
                decor_cells: 0,
                links: Vec::new(),
                rail: None,
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
        // §用户诉求：思考卡与后续组保持一格间隔。
        prev_requires_gap = true;
    }
    // 流式 assistant（Markdown，按 version 缓存）。带左 rail + AI 标签，
    // 与历史 assistant 消息一致（§16.2 层次感）。独立 group（稳定 id）。
    if let Some(msg) = &live.assistant
        && !msg.text.is_empty()
    {
        // §用户诉求：AI 输出卡与前面的思考/工具卡保持一格间隔。
        if prev_requires_gap {
            push_gap_row(out, hits, semantic);
        }
        let (rendered, line_links) = cached_markdown(cache, msg.version, &msg.text, theme, width);
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
            let rail = line.spans.first().cloned();
            out.push(line);
            hits.push(None);
            semantic.push(SemanticLine {
                text: semantic_text,
                decor_cells: line_width.saturating_sub(text_width),
                links: line_links.get(i).cloned().unwrap_or_default(),
                rail,
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
        // §用户诉求：AI 输出卡与后续工具卡保持一格间隔。
        prev_requires_gap = true;
    }
    // 运行中工具卡片（按启动顺序）。§PointerHit ⑤：每张卡片独立 group/稳定 id。
    for call_id in &live.tool_order {
        if let Some(tool) = live.tools.get(call_id) {
            let card = &tool.card;
            // §用户诉求：前面是思考/AI 输出卡（或展开态工具卡）时补间隔；
            // 紧凑工具卡之间保持紧凑（与历史区规则一致）。
            if prev_requires_gap {
                push_gap_row(out, hits, semantic);
            }
            let card_id = card.id.clone();
            let card_lines = tool_card_lines(card, view.anim_tick, theme, width);
            // §PointerHit：语义文本与渲染行一一对应（P0-1：同一窗口）。
            let semantic_rows = card_semantic_rows(card);
            for (i, line) in card_lines.into_iter().enumerate() {
                // §美化：整卡可点击（与历史卡片一致）——主行与内容行都带
                // Tool hit；拖选仍是文本选择。
                let hit = Some((HitTarget::Tool(card_id.clone()), line.width() as u16));
                let semantic_text = semantic_rows.get(i).cloned().unwrap_or_default();
                let raw_text = line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                let line_width = unicode_width::UnicodeWidthStr::width(raw_text.as_str());
                let text_width = unicode_width::UnicodeWidthStr::width(semantic_text.as_str());
                let rail = line.spans.first().cloned();
                out.push(line);
                hits.push(hit);
                semantic.push(SemanticLine {
                    text: semantic_text,
                    decor_cells: line_width.saturating_sub(text_width),
                    links: Vec::new(),
                    rail,
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
            // 紧凑工具卡后不强制间隔；展开态（多行块）保留间隔。
            prev_requires_gap = !tool_card_compact(&tool.card);
        }
    }
}

/// §codex 移植：User/Assistant 消息 rail 前缀宽度（`│ you  ` / `│ AI   ` = 7 cell）。
/// 表格在内容宽度（width - rail）内布局，防止加 rail 后超宽被二次 wrap。
const RAIL_WIDTH: usize = 7;

/// 按条目版本缓存 Markdown 渲染（流式增量只重渲染变化条目，§16.1）。
/// 返回缓存结果的行与链接（链接范围基于渲染行文本；§成熟化）。
fn cached_markdown(
    cache: &mut HashMap<(u64, u16), RenderedMarkdown>,
    version: u64,
    text: &str,
    theme: theme::Theme,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Vec<LinkRange>>) {
    let key = (version, width as u16);
    if let Some(entry) = cache.get(&key) {
        return (entry.lines.clone(), entry.links.clone());
    }
    let rendered = render_markdown_detailed(text, theme, Some(width));
    if cache.len() > 2048 {
        cache.clear();
    }
    let lines = rendered.lines.clone();
    let links = rendered.links.clone();
    cache.insert(key, rendered);
    (lines, links)
}

/// 侧边栏 Todo 项：当前项优先，其余开放项随后，终态项沉底。侧边栏自身可滚动。
fn sidebar_plan_items(plan: &crate::tool::plan::Plan) -> Vec<&crate::tool::plan::PlanItem> {
    use crate::tool::plan::PlanStatus;
    let rank = |status| match status {
        PlanStatus::InProgress => 0,
        PlanStatus::Pending => 1,
        PlanStatus::Blocked => 2,
        PlanStatus::Completed => 3,
        PlanStatus::Cancelled => 4,
    };
    let mut out: Vec<_> = plan.items.iter().collect();
    out.sort_by_key(|item| rank(item.status));
    out
}

/// 右侧边栏（§用户诉求：opencode 式）。上方 Todo（`view.plan` 项），
/// 下方用户消息大纲（单行摘要）；整栏一个滚动偏移 + 右侧 1 列滚动条。
///
/// 返回 `(矩形, 可视行 → 大纲 entry id)`：矩形供鼠标 hit-test，命中表
/// 供点击跳转（仅大纲行有 entry；todo/标题/空白为 None）。
///
/// 内容行数 = todo（标题 + 项）+ 大纲（标题 + 用户消息），超区域高度时
/// 可滚动；写回 `view.sidebar.total_rows`（reducer 用它 clamp 滚动偏移）。
fn draw_sidebar(
    frame: &mut ratatui::Frame,
    area: Rect,
    view: &mut ViewModel,
    theme: theme::Theme,
) -> Vec<Option<EntryId>> {
    let title_style = Style::default()
        .fg(theme.primary)
        .bg(theme.surface)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme.muted).bg(theme.surface);
    // 内容列宽 = 整栏 - 左侧竖线 1 列（滚动条单独绘制，不占内容列）。
    let content_w = area.width.saturating_sub(1) as usize;

    // 组装内容行（逻辑行）：todo 段 + 大纲段。
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<Option<EntryId>> = Vec::new();
    // —— Todo ——
    lines.push(Line::from(vec![Span::styled(" ☑ Todo", title_style)]));
    hits.push(None);
    // §用户诉求：全部项完成/取消后计划视为结束——与 plan_snapshot 的语义一致
    // （plan.rs：全部终态 → 空快照，不再注入模型上下文），侧边栏同样不显示
    // 已结束计划的完成态列表（此前只检查 items 非空，全标完成后 Todo 仍挂在 UI）。
    // 有开放项时保留历史（终态项沉底，sidebar_plan_items 排序）。
    let has_open = view
        .plan
        .as_ref()
        .is_some_and(|plan| plan.items.iter().any(|item| item.status.is_open()));
    match &view.plan {
        Some(plan) if !plan.items.is_empty() && has_open => {
            for item in sidebar_plan_items(plan) {
                let (marker, style) = match item.status {
                    crate::tool::plan::PlanStatus::Completed => (
                        "[x]",
                        Style::default()
                            .fg(theme.muted)
                            .bg(theme.surface)
                            .add_modifier(Modifier::DIM),
                    ),
                    crate::tool::plan::PlanStatus::InProgress => {
                        ("[>]", Style::default().fg(theme.warning).bg(theme.surface))
                    }
                    crate::tool::plan::PlanStatus::Pending => {
                        ("[ ]", Style::default().fg(theme.muted).bg(theme.surface))
                    }
                    crate::tool::plan::PlanStatus::Blocked => {
                        ("[!]", Style::default().fg(theme.error).bg(theme.surface))
                    }
                    crate::tool::plan::PlanStatus::Cancelled => (
                        "[-]",
                        Style::default()
                            .fg(theme.muted)
                            .bg(theme.surface)
                            .add_modifier(Modifier::DIM),
                    ),
                };
                // §用户诉求：todo 长文本折行显示完整（不截断）；marker 占 4 列
                // （3 字符 + 空格），续行缩进 4 列对齐。CJK 双宽按 2 cells 折行。
                let wrapped = crate::tui::text::wrap_to_cell_width(
                    &item.text,
                    content_w.saturating_sub(4),
                );
                for (index, segment) in wrapped.iter().enumerate() {
                    let prefix = if index == 0 {
                        format!("{marker} ")
                    } else {
                        "     ".to_string()
                    };
                    lines.push(Line::from(vec![Span::styled(
                        format!("{prefix}{segment}"),
                        style,
                    )]));
                    hits.push(None);
                }
            }
        }
        _ => {
            lines.push(Line::from(vec![Span::styled("(无活动计划)", muted)]));
            hits.push(None);
        }
    }
    // —— 用户消息大纲 ——
    lines.push(Line::from(vec![Span::styled(" ┋ 用户消息", title_style)]));
    hits.push(None);
    let outline = view.sidebar_outline();
    if outline.is_empty() {
        lines.push(Line::from(vec![Span::styled("(无用户消息)", muted)]));
        hits.push(None);
    } else {
        for (entry_id, text) in &outline {
            let shown =
                crate::tui::text::truncate_to_cell_width(text, content_w.saturating_sub(1), "…");
            // 大纲行可点击：整行用 hover 色提示可跳转。
            lines.push(Line::from(vec![Span::styled(
                format!(" {shown}"),
                Style::default().fg(theme.info).bg(theme.surface),
            )]));
            hits.push(Some(*entry_id));
        }
    }
    let total_rows = lines.len();
    view.sidebar.total_rows = total_rows;
    // §修复：写回可视高度——滚动条点击按比例跳转（scroll_to_ratio）依赖
    // area_height；此前从未写回（默认 1），点击侧边栏滚动条必跳到底部。
    let area_h = area.height.max(1) as usize;
    view.sidebar.area_height = area_h;
    let max_start = total_rows.saturating_sub(area_h);
    let start = view.sidebar.scroll.min(max_start);
    let window_lines: Vec<Line<'static>> = lines.iter().skip(start).take(area_h).cloned().collect();
    let window_hits: Vec<Option<EntryId>> = hits.iter().skip(start).take(area_h).cloned().collect();

    // 渲染：左侧竖线分隔主区，内容区 surface 背景，右侧 1 列滚动条。
    let bg = Style::default().bg(theme.surface);
    let mut rendered: Vec<Line<'static>> = Vec::with_capacity(window_lines.len());
    for line in window_lines.iter() {
        let mut spans: Vec<Span<'static>> = vec![Span::styled("│", bg)];
        // 每行内容补空格到 content_w（视觉上整栏 panel）。
        let mut content = line.clone();
        let cur = unicode_width::UnicodeWidthStr::width(
            content
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .as_str(),
        );
        if cur < content_w {
            content
                .spans
                .push(Span::styled(" ".repeat(content_w - cur), bg));
        }
        spans.extend(content.spans);
        rendered.push(Line::from(spans));
    }
    // 不足一屏时补空白行到满高。
    while rendered.len() < area_h {
        rendered.push(Line::from(vec![Span::styled(
            "│".to_string() + &" ".repeat(content_w),
            bg,
        )]));
    }
    // 滚动条单独画（比例 thumb），不混入内容行。
    if area.width >= 2 {
        let rect = Rect::new(area.x + area.width - 1, area.y, 1, area.height);
        draw_scrollbar(frame, rect, theme, start, total_rows);
    }
    frame.render_widget(Paragraph::new(rendered), area);
    window_hits
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
    // §用户诉求：Claude Code 式缓存命中显示——累计旁附“最近一次请求命中率”。
    if view.cache_read_tokens > 0 {
        let mut text = format!(" · ⇄{}", fmt_tokens(view.cache_read_tokens));
        if view.last_input_tokens > 0 && view.last_cache_read_tokens > 0 {
            let pct = (view.last_cache_read_tokens as f64
                / view.last_input_tokens as f64
                * 100.0) as u64;
            text.push_str(&format!("({pct}%)"));
        }
        spans.push(Span::styled(
            text,
            Style::default().fg(theme.success),
        ));
    }
    // §用户诉求：Claude Code 式重连提示——本次 run 断线重连/重启累计次数。
    if view.reconnect_count > 0 {
        spans.push(Span::styled(
            format!(" · ⟳{}", view.reconnect_count),
            Style::default().fg(theme.warning),
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
    // §用户诉求：滚动条加粗——thumb 用实心块（█ 比 ▐ 更粗更醒目），
    // track 用细竖线。
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
            *glyph = "█";
        }
    }
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area_h);
    for g in glyphs {
        let style = if g == "█" {
            Style::default().fg(theme.info).add_modifier(Modifier::BOLD)
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
/// §美化：输入区面板化——`❯ ` prompt 带 panel 背景 + 整行补空格，
/// 输入区成为独立盒子（opencode Prompt 盒子；光标计算不受 bg 影响）。
/// 输入区（§16.2：硬件 cursor 放在真实输入位置，优先保证中文 IME）。
///
/// 支持多行（Alt+Enter）；行数与光标位置按 display width 折行计算。
/// §修复：折行必须「首行 width-prompt、续行 width」——此前 draw_input 按 width
/// 折含 prompt 整行（首行截 2）、input_cursor_cell 按 width-2 折全部（续行多折 2），
/// 视觉与光标各偏一档，多行后累积偏移（3 行偏 1、4 行偏 2）。两者共用 input_wrap。
fn draw_input(frame: &mut ratatui::Frame, area: Rect, view: &ViewModel, theme: theme::Theme) {
    const PROMPT_WIDTH: u16 = 2; // "❯ "
    let wrapped = input_wrap(&view.input, area.width as usize, PROMPT_WIDTH as usize);
    let prompt = Span::styled(
        "❯ ",
        Style::default()
            .fg(theme.accent)
            .bg(theme.panel)
            .add_modifier(Modifier::BOLD),
    );
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(wrapped.len());
    for (i, text) in wrapped.iter().enumerate() {
        let mut spans = if i == 0 {
            vec![
                prompt.clone(),
                Span::styled(text.clone(), Style::default().bg(theme.panel)),
            ]
        } else {
            vec![Span::styled(text.clone(), Style::default().bg(theme.panel))]
        };
        // 整行补空格到满宽（面板底连续）。
        let cur: usize = spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .chars()
            .map(crate::tui::text::char_cell_width)
            .sum();
        if cur < area.width as usize {
            spans.push(Span::styled(
                " ".repeat(area.width as usize - cur),
                Style::default().bg(theme.panel),
            ));
        }
        lines.push(Line::from(spans));
    }
    let rows = lines.len().max(1) as u16;
    let (cursor_row, cursor_col) = input_cursor_cell(&view.input, view.input_cursor, area.width);
    // BUG-008：滚动基准必须是可见区域高度（area.height），而不是全部折行行数
    // （rows）——否则长输入（>8 行）时光标会被放到输入区之外、不可见。
    let scroll_rows = input_scroll_offset(cursor_row, rows, area.height);
    frame.render_widget(Paragraph::new(lines).scroll((scroll_rows, 0)), area);
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
    // 与 draw_input/input_cursor_cell 共用同一规则：只有首行扣 prompt 宽度，
    // 续行使用整行宽度。此前这里每一行都扣 2 列，会错误增高输入区并造成
    // 光标/内部滚动在窄窗口中抖动。
    let wrapped = input_wrap(&view.input, width as usize, 2);
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
    // 首行预算 width-prompt（含 ❯ ），续行预算 width（无 prompt）——
    // 与 draw_input 同一折行（§修复：视觉与光标不再各自偏移）。
    let rows = input_wrap(&input[..cursor], width as usize, PROMPT_WIDTH as usize);
    let row = rows.len().saturating_sub(1) as u16;
    let last_w = rows
        .last()
        .map(|s| s.chars().map(crate::tui::text::char_cell_width).sum())
        .unwrap_or(0) as u16;
    let col = if row == 0 {
        PROMPT_WIDTH + last_w
    } else {
        last_w
    };
    (row, col)
}

/// 输入文本按 display width 折行（§修复）：首行可用 `width - prompt_w`
/// （prompt 占位），续行可用 `width`。返回每行纯文本（不含 prompt）。
/// 显式 `\n` 也换行（多行输入/粘贴）。
fn input_wrap(text: &str, width: usize, prompt_w: usize) -> Vec<String> {
    let width = width.max(prompt_w + 1);
    let budget = |row: usize| if row == 0 { width - prompt_w } else { width };
    let mut rows: Vec<String> = vec![String::new()];
    let mut row = 0usize;
    let mut w = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            rows.push(String::new());
            row += 1;
            w = 0;
            continue;
        }
        let cw = crate::tui::text::char_cell_width(ch);
        if w + cw > budget(row) && w > 0 {
            rows.push(String::new());
            row += 1;
            w = 0;
        }
        rows[row].push(ch);
        w += cw;
    }
    rows
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
    // §用户诉求：/sessions 悬浮窗预览行着色——`你 ` accent、`AI ` 正文色。
    let diff_colored = modal.title == "/diff";
    let session_preview = modal.title == "/sessions";
    for line in modal.body.lines() {
        let styled: Line<'static> = if session_preview && line.starts_with("你 ") {
            Line::styled(line.to_string(), Style::default().fg(theme.accent))
        } else if session_preview && line.starts_with("AI ") {
            Line::styled(line.to_string(), Style::default().fg(theme.text))
        } else if diff_colored && line.starts_with('+') && !line.starts_with("+++") {
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
    let scroll = modal.scroll.min(total.saturating_sub(inner_h));
    let start = scroll;
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
        let command_language = if cfg!(windows) { "powershell" } else { "bash" };
        content.extend(
            highlight::highlight_code_block(command, Some(command_language), theme).unwrap_or_else(
                |_| {
                    command
                        .split('\n')
                        .map(|line| Line::styled(line.to_string(), Style::default().fg(theme.text)))
                        .collect()
                },
            ),
        );
        content.push(Line::default());
    }
    // §成熟化：Link Overlay 正文显示 URL（info 色可读）。
    if overlay.kind == model::OverlayKind::Link {
        for line in overlay.body.split('\n') {
            content.push(Line::styled(
                line.to_string(),
                Style::default()
                    .fg(theme.info)
                    .add_modifier(Modifier::UNDERLINED),
            ));
        }
    } else if !overlay.body.is_empty() {
        content.push(Line::styled(
            "Output",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ));
        if let Some(language) = overlay.body_language.as_deref() {
            content.extend(
                highlight::highlight_code_block(&overlay.body, Some(language), theme)
                    .unwrap_or_else(|_| {
                        overlay
                            .body
                            .split('\n')
                            .map(|line| {
                                Line::styled(line.to_string(), Style::default().fg(theme.text))
                            })
                            .collect()
                    }),
            );
        } else {
            for line in overlay.body.split('\n') {
                content.push(Line::styled(
                    line.to_string(),
                    Style::default().fg(theme.text),
                ));
            }
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
    let hint = match overlay.kind {
        model::OverlayKind::Link => "[Enter] 打开 · [c] 复制 · [Esc] 关闭",
        _ => "[Esc] 关闭 · [PgUp/PgDn] 滚动",
    };
    content.push(Line::styled(
        hint,
        Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
    ));

    // 内部滚动（逻辑行按 inner_w 折行后窗口化）。
    let rows = wrap_with_semantic(content, Vec::new(), &[], inner_w, EntryId(0));
    let wrapped: Vec<Line<'static>> = rows.into_iter().map(|r| r.line).collect();
    let total = wrapped.len();
    let scroll = overlay.scroll.min(total.saturating_sub(inner_h));
    let start = scroll;
    let end = (start + inner_h).min(wrapped.len());
    let window = wrapped[start..end].to_vec();

    // Border title follows overlay kind: tool details vs thinking (reasoning) vs link.
    let border_title = match overlay.kind {
        model::OverlayKind::Tool => " Tool details ",
        model::OverlayKind::Reasoning => " 思考（reasoning） ",
        model::OverlayKind::Link => " 链接 ",
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

/// 菜单项的 (主列 label, 辅助列 desc) 文本（§用户诉求：菜单美化）。
fn menu_item_texts(menu: &model::MenuView, name: &str, desc: &str) -> (String, String) {
    match menu.kind {
        model::MenuKind::SlashCommand => (format!("/{name}"), desc.to_string()),
        model::MenuKind::File => (name.to_string(), desc.to_string()),
        // §用户诉求（恢复会话可判断）：会话菜单主列显示名字（首条消息摘要
        // + 时间 + 事件数），UUID 缩短为辅助列。
        model::MenuKind::Session => (
            desc.to_string(),
            format!("(id {}…)", name.chars().take(13).collect::<String>()),
        ),
        model::MenuKind::Theme => (name.to_string(), desc.to_string()),
    }
}

/// 菜单浮层宽度：主列（label）按内容自适应，辅助列（sub）给固定预算并在
/// 行内截断——避免最长 desc 把命令菜单撑得比 transcript 还宽。会话/主题
/// 菜单预算更大，命令/文件菜单保持窄。
fn menu_floating_width(menu: &model::MenuView, max_w: u16) -> u16 {
    let label_w = menu
        .items
        .iter()
        .map(|(name, desc)| crate::tui::text::display_width(&menu_item_texts(menu, name, desc).0))
        .max()
        .unwrap_or(0);
    // §用户诉求（菜单截断怪相）：辅助列按**最长描述自适应**，不再用固定
    // cap 截断——命令/会话/主题菜单的描述都是短句，完整显示比截断更重要；
    // 极端长内容（文件路径等）仍由行内 truncate_head 兜底。
    let sub_w = menu
        .items
        .iter()
        .map(|(name, desc)| crate::tui::text::display_width(&menu_item_texts(menu, name, desc).1))
        .max()
        .unwrap_or(0);
    // ▸+空格(2) + 两列间距(2) + 左右边框(2)。
    let w = label_w as u16 + 6 + sub_w as u16;
    w.clamp(28, max_w.min(96))
}

/// 菜单边框标题（按类型）。
fn menu_title(kind: model::MenuKind) -> &'static str {
    match kind {
        model::MenuKind::SlashCommand => " 命令 ",
        model::MenuKind::File => " 文件 ",
        model::MenuKind::Session => " 会话 ",
        model::MenuKind::Theme => " 主题 ",
    }
}

/// 菜单底部快捷键提示（按类型）。
fn menu_hint(kind: model::MenuKind) -> &'static str {
    match kind {
        model::MenuKind::Session | model::MenuKind::Theme => "↑/↓ 选择 · Enter 应用 · Esc 取消",
        _ => "↑/↓ 选择 · Enter 选中 · Esc 取消",
    }
}

/// 命令补全/选择菜单（/命令、@文件、/sessions、/theme）。
///
/// §用户诉求：菜单美化——带边框 + 类型标题、选中行整行高亮、主列/辅助列
/// 对齐、底部快捷键提示；宽度按内容自适应（不再裸文本浮在 transcript 上）。
fn draw_menu(frame: &mut ratatui::Frame, rect: Rect, view: &ViewModel, theme: theme::Theme) {
    let Some(menu) = &view.menu else {
        return;
    };
    let total = menu.items.len();
    // §防御：空菜单（会话被删光/过滤后）直接不画——selected 已 clamp，
    // 避免窗口计算对 total=0 越界导致屏幕空白。
    if total == 0 {
        return;
    }
    let selected = menu.selected.min(total - 1);
    // 边框内宽（左右各 1 列）与内容高（上下边框 + 底部 hint）。
    let inner_w = rect.width.saturating_sub(2).max(1) as usize;
    let inner_h = rect.height.saturating_sub(3).max(1) as usize; // 2 边框 + 1 hint

    // 两列内容宽度（用于对齐：主列 label 左对齐，辅助列从同一列开始）。
    let mut label_w = 0usize;
    let mut sub_w = 0usize;
    let mut texts: Vec<(String, String)> = Vec::with_capacity(total);
    for (name, desc) in &menu.items {
        let (label, sub) = menu_item_texts(menu, name, desc);
        label_w = label_w.max(crate::tui::text::display_width(&label));
        sub_w = sub_w.max(crate::tui::text::display_width(&sub));
        texts.push((label, sub));
    }
    // 每行前缀（选中标记 ▸ + 空格）占 2 列；两列间距 2 列。
    let content_w = label_w
        .saturating_add(2)
        .saturating_add(sub_w)
        .saturating_add(4);
    let inner_w = inner_w.min(content_w.max(2)); // 窄屏不溢出（内容超宽时截断）

    // 长菜单：可视窗口跟随选中项。
    // §用户诉求（菜单滚动）：`…` 只在对应方向**确实有未显示项**时出现——
    // 两次计算：先估窗口范围判断省略号（start>0 → 顶部，end<total → 底部），
    // 再把省略号行数从窗口扣除，重算精确 start/end。避免滚动菜单顶部/底部
    // 永远挂着 `…`，也避免省略号行溢出 rect。
    let first_window = inner_h.min(total);
    let est_start = if total > first_window {
        (selected.saturating_sub(first_window / 2)).min(total - first_window)
    } else {
        0
    };
    let est_end = (est_start + first_window).min(total);
    let top_ellipsis = est_start > 0;
    let bottom_ellipsis = est_end < total;
    let window_rows = inner_h
        .saturating_sub(usize::from(top_ellipsis) + usize::from(bottom_ellipsis))
        .max(1);
    let start = if total > window_rows {
        (selected.saturating_sub(window_rows / 2)).min(total - window_rows)
    } else {
        0
    };
    let end = (start + window_rows).min(total);

    // 行样式：未选中行整行 surface_subtle 底色（菜单浮层面），选中行提亮为
    // surface + primary 加粗（§用户诉求：菜单选中清晰可见）。
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(window_rows + 2);
    if top_ellipsis {
        lines.push(Line::styled(
            "…",
            Style::default().fg(theme.muted).bg(theme.surface_subtle),
        ));
    }
    for (i, (label, sub)) in texts.iter().enumerate().skip(start).take(end - start) {
        let is_selected = i == selected;
        let bg = if is_selected {
            theme.surface
        } else {
            theme.surface_subtle
        };
        let mut spans = vec![
            Span::styled(
                if is_selected { "▸ " } else { "  " },
                if is_selected {
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD)
                        .bg(bg)
                } else {
                    Style::default().fg(theme.muted).bg(bg)
                },
            ),
            Span::styled(
                label.clone(),
                if is_selected {
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD)
                        .bg(bg)
                } else {
                    Style::default().fg(theme.text).bg(bg)
                },
            ),
        ];
        // 辅助列对齐：sub 从固定列（▸ 2 + label_w + 间距 2）开始，不足
        // 对齐宽度时补空格；sub 超宽截断到剩余空间（room 只扣固定前缀，
        // 否则短 label 行会因 pad 大而把 sub 挤没）。
        // §用户诉求（菜单截断怪相）：描述是短句，超宽用**尾部截断**
        // （保留开头 + …）——中段截断会保留孤立的尾字符（如“…代码高亮）”，
        // 读起来残缺。
        let pad = label_w.saturating_sub(crate::tui::text::display_width(label)) + 2;
        let room = inner_w.saturating_sub(label_w + 4);
        let sub_shown = crate::tui::text::truncate_head_to_cell_width(sub, room, "…");
        spans.push(Span::styled(
            format!("{}{sub_shown}", " ".repeat(pad)),
            Style::default().fg(theme.muted).bg(bg),
        ));
        // 补白到内宽：整行背景连续（浮层不露出底下的 transcript）。
        let cur = unicode_width::UnicodeWidthStr::width(
            spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .as_str(),
        );
        if cur < inner_w {
            spans.push(Span::styled(
                " ".repeat(inner_w - cur),
                Style::default().bg(bg),
            ));
        }
        lines.push(Line::from(spans));
    }
    if bottom_ellipsis {
        lines.push(Line::styled(
            "…",
            Style::default().fg(theme.muted).bg(theme.surface_subtle),
        ));
    }

    // 边框 + 标题 + hint。整个浮层背景统一为 surface_subtle（§用户诉求：
    // 背景一致）——Block.style 覆盖边框与标题，hint 行显式补背景，
    // 选中行才提亮为 surface。
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.surface_subtle))
        .title(menu_title(menu.kind))
        .style(Style::default().bg(theme.surface_subtle));
    lines.push(Line::styled(
        format!("  {}", menu_hint(menu.kind)),
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::DIM)
            .bg(theme.surface_subtle),
    ));
    // 内容区固定背景：无背景的段落会透出底下 transcript（颜色不一致）。
    let content = Text::from(lines);
    let styled = Paragraph::new(content)
        .block(block)
        .scroll((0, 0))
        .style(Style::default().bg(theme.surface_subtle));
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(styled, rect);
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
    let mut cache: HashMap<(u64, u16), RenderedMarkdown> = HashMap::new();
    let mut wrap_cache: HashMap<(EntryId, u16), Arc<Vec<WrappedRow>>> = HashMap::new();
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
                    wrap_cache: &mut wrap_cache,
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
        let mut cache: HashMap<(u64, u16), RenderedMarkdown> = HashMap::new();
        let mut cache_width = 0u16;
        let mut committed = 0usize;
        let mut wrap_cache: HashMap<(EntryId, u16), Arc<Vec<WrappedRow>>> = HashMap::new();
        let mut overflow: Vec<Line<'static>> = Vec::new();
        terminal
            .draw(|frame| {
                let plan = render_frame(
                    frame,
                    view,
                    theme,
                    RenderContext {
                        markdown_cache: &mut cache,
                        wrap_cache: &mut wrap_cache,
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
            let lines = std::mem::take(&mut overflow);
            for chunk in lines.chunks(u16::MAX as usize) {
                let chunk = chunk.to_vec();
                let height = u16::try_from(chunk.len()).unwrap_or(u16::MAX);
                let _ = terminal.insert_before(height, |buf| {
                    let area = Rect::new(0, 0, buf.area().width, height);
                    Paragraph::new(Text::from(chunk.clone())).render(area, buf);
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
