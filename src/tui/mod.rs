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
use ratatui::widgets::{Paragraph, Widget, Wrap};
use std::collections::HashMap;
use std::time::Instant;

use markdown::{LinkRange, RenderedMarkdown, render_markdown, render_markdown_detailed};
use model::{Entry, LineKind, StatusLine, ViewModel};
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
    wrap_cache: HashMap<(EntryId, u16), Vec<WrappedRow>>,
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
    /// 最近一帧窗口内每行的语义信息（§InteractionRefactor：Ctrl+C 复制源，
    /// 从语义文本提取，不反推渲染结果）。与窗口行一一对应（§成熟化：
    /// 含可点击链接范围）。
    semantic_rows: Vec<Option<RowSemantic>>,
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
            wrap_cache: HashMap::new(),
            last_transcript_revision: 0,
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
/// 自下而上：输入区（多行，≤4 行）→ footer（1 行）→ 计划条（0..4 行）→ 转录区。
struct RenderContext<'a> {
    markdown_cache: &'a mut HashMap<(u64, u16), RenderedMarkdown>,
    /// wrap 结果缓存（§性能：历史 entry 折叠/内容不变 → 跨帧复用，
    /// 避免每帧对全量历史逐字符重建 span）。key = (entry_id, width)。
    wrap_cache: &'a mut HashMap<(EntryId, u16), Vec<WrappedRow>>,
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
    // 宽度变化 → Markdown 渲染缓存失效，提交位置重置为当前窗口起点
    // （§16.1：已提交到 scrollback 的历史 immutable，resize 后只重排活动区）。
    let reset_committed = *cache_width != area.width;
    if reset_committed {
        cache.clear();
        // §性能：wrap 结果按宽度缓存，resize 后一并失效（key 含 width）。
        wrap_cache.clear();
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
    // §美化：footer 上方 1-cell 分隔线，把底部状态栏锚定成独立区域。
    constraints.push(Constraint::Length(1)); // footer 分隔线
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

    if let Some(pa) = plan_area {
        draw_plan(frame, pa, view, theme);
    }
    // footer 分隔线（muted 横线；与左竖线语言一致，只作锚定不喧宾）。
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(footer_rule_area.width as usize),
            Style::default().fg(theme.border),
        )),
        footer_rule_area,
    );
    draw_footer(frame, footer_area, view, theme, scrollback, mode);

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

    // 命令补全菜单浮层（覆盖在转录区上方，§16.2 之外的小浮层）。
    if let Some(menu) = &view.menu {
        let h = (menu.items.len() as u16 + 1).min(9);
        // §用户诉求（恢复会话可判断）：会话列表比命令/文件补全宽得多
        // （名字 + 时间 + 事件数 + 短 id），48 列会截断会话名；其余菜单保持窄。
        let w = match menu.kind {
            model::MenuKind::Session => area.width.min(96),
            _ => area.width.min(48),
        };
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
#[allow(clippy::too_many_arguments)] // §性能：布局输入 + 双缓存（md/wrap）。
fn plan_window(
    view: &mut ViewModel,
    theme: theme::Theme,
    width: u16,
    area_h: u16,
    committed: usize,
    reset_committed: bool,
    cache: &mut HashMap<(u64, u16), RenderedMarkdown>,
    wrap_cache: &mut HashMap<(EntryId, u16), Vec<WrappedRow>>,
) -> FramePlan {
    let width = width.max(1) as usize;
    // 按 entry 构建逻辑行（entry → wrapped 行 + hits + 语义文本）。
    // ids/heights 必须以 wrapped_by_entry 为准（含 live 哨兵 group，§7.2）。
    let per_entry = build_transcript_text(view, theme, cache, width);
    let mut wrapped_by_entry: Vec<(EntryId, Vec<WrappedRow>)> = Vec::with_capacity(per_entry.len());
    // §性能：历史 transcript entry 的内容与折叠状态决定 wrap 结果。
    // transcript_revision 变化时 wrap_cache 已被清空；命中则直接复用，
    // 避免每帧对全量历史逐字符重建 span（上下文越多收益越大）。
    // live 区（不在 transcript 中）内容每帧变化，不缓存。
    let live_ids: std::collections::HashSet<EntryId> = view
        .live
        .tool_order
        .iter()
        .filter_map(|id| view.live.tools.get(id).map(|t| t.entry_id))
        .chain(view.live.assistant.iter().map(|m| m.entry_id))
        .chain(view.live.reasoning.iter().map(|m| m.entry_id))
        .collect();
    for (id, logical, hits, semantic_lines) in per_entry {
        let cacheable = !live_ids.contains(&id);
        let key = (id, width as u16);
        let rows = if cacheable {
            if let Some(hit) = wrap_cache.get(&key) {
                hit.clone()
            } else {
                let rows = wrap_with_semantic(logical, hits, &semantic_lines, width, id);
                wrap_cache.insert(key, rows.clone());
                rows
            }
        } else {
            wrap_with_semantic(logical, hits, &semantic_lines, width, id)
        };
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
    // **Line 级样式**（diff 行的红绿底）。行内元素背景（inline code 的
    // surface_subtle 等）绝不能被用来填 padding——否则 assistant 裸文本行
    // 含行内 code 时，行尾空白会泄漏 code 的深色底一直铺到右缘。
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
    for entry in view.transcript.iter() {
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
                            // md 行 span 烙 panel 底（与卡片面一致；保留 inline code 底色）。
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
                // §美化：整卡可点击——主行与内容行都带 Tool hit（轻点任意行
                // 展开/收缩；拖选仍是文本选择，不冲突）。内容行如有链接，
                // pointer_target 里链接优先于工具命中。
                for (i, line) in card_lines.into_iter().enumerate() {
                    let hit = Some((HitTarget::Tool(card_id.clone()), width as u16));
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
        let message_kind = match entry {
            Entry::Message { line, .. } => Some(line.kind),
            Entry::Tool { .. } => None,
        };
        let needs_gap = match message_kind {
            Some(LineKind::User | LineKind::Assistant | LineKind::Reasoning) => true,
            None => !matches!(entry, Entry::Tool { card, .. }
                if card.collapsed_lines == 0
                    && !card.expanded
                    && (card.diff.is_some() || card.output.is_some() || card.tail.is_some())),
            Some(_) => false,
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
        if view.reasoning_visible {
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
                out.push(line);
                // §修复：展开态每行都可点击（点正文任意处收起）——与历史
                // reasoning 展开态一致；否则 live 展开后无任何 Reasoning hit，
                // 点击无法触发收起（“打开后关不上”）。
                hits.push(Some((HitTarget::Reasoning(msg.entry_id), width as u16)));
                semantic.push(SemanticLine {
                    text: semantic_text,
                    decor_cells: 10, // "┃ ◆ 思考 " 前缀
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
    }
    // 流式 assistant（Markdown，按 version 缓存）。带左 rail + AI 标签，
    // 与历史 assistant 消息一致（§16.2 层次感）。独立 group（稳定 id）。
    if let Some(msg) = &live.assistant
        && !msg.text.is_empty()
    {
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
    }
    // 运行中工具卡片（按启动顺序）。§PointerHit ⑤：每张卡片独立 group/稳定 id。
    for call_id in &live.tool_order {
        if let Some(tool) = live.tools.get(call_id) {
            let card = &tool.card;
            let card_id = card.id.clone();
            let card_lines = tool_card_lines(card, view.anim_tick, theme, width);
            for (i, line) in card_lines.into_iter().enumerate() {
                // §美化：整卡可点击（与历史卡片一致）——主行与内容行都带
                // Tool hit；拖选仍是文本选择。
                let hit = Some((HitTarget::Tool(card_id.clone()), line.width() as u16));
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

/// 计划条显示的行（§用户诉求：修复计划技能锁死）：活跃项（Pending/InProgress）
/// 优先——整体替换后 Completed 历史排在 items 前，不能把进行中的计划挤出屏；
/// 活跃不足 3 条时用 Completed 历史补齐（沉底）。
fn plan_display_items(plan: &crate::tool::plan::Plan) -> Vec<&crate::tool::plan::PlanItem> {
    use crate::tool::plan::PlanStatus;
    let active: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.status != PlanStatus::Completed)
        .collect();
    if active.is_empty() {
        return plan
            .items
            .iter()
            .filter(|i| i.status == PlanStatus::Completed)
            .take(3)
            .collect();
    }
    let mut out: Vec<_> = active.into_iter().take(3).collect();
    if out.len() < 3 {
        out.extend(
            plan.items
                .iter()
                .filter(|i| i.status == PlanStatus::Completed)
                .take(3 - out.len()),
        );
    }
    out
}

/// 计划条（§16.2：编辑器上方独立紧凑区域，不出现在 transcript）。
fn plan_area_rows(view: &ViewModel) -> u16 {
    match &view.plan {
        Some(plan) if !plan.items.is_empty() => (1 + plan_display_items(plan).len()) as u16,
        _ => 0,
    }
}

fn draw_plan(frame: &mut ratatui::Frame, area: Rect, view: &ViewModel, theme: theme::Theme) {
    let Some(plan) = &view.plan else {
        return;
    };
    // §美化：plan 面板化——左竖线 + panel 背景 + 补空格到满宽（独立面板）。
    let rail = Style::default()
        .fg(theme.accent)
        .bg(theme.panel)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::from(vec![
        Span::styled("┃ ", rail),
        Span::styled(
            "计划",
            Style::default()
                .fg(theme.primary)
                .bg(theme.panel)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    for item in plan_display_items(plan) {
        let (marker, style) = match item.status {
            crate::tool::plan::PlanStatus::Completed => (
                "[x]",
                Style::default()
                    .fg(theme.muted)
                    .bg(theme.panel)
                    .add_modifier(Modifier::DIM),
            ),
            crate::tool::plan::PlanStatus::InProgress => {
                ("[>]", Style::default().fg(theme.warning).bg(theme.panel))
            }
            crate::tool::plan::PlanStatus::Pending => {
                ("[ ]", Style::default().fg(theme.muted).bg(theme.panel))
            }
        };
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default().bg(theme.panel)),
            Span::styled(format!("{marker} {}", item.text), style),
        ]));
    }
    // 补空格到满宽（与 plan_window 的补空格逻辑一致）。
    let width = area.width as usize;
    for line in lines.iter_mut() {
        let cur = unicode_width::UnicodeWidthStr::width(
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .as_str(),
        );
        if cur < width {
            let pad = width - cur;
            line.spans.push(Span::styled(
                " ".repeat(pad),
                Style::default().bg(theme.panel),
            ));
        }
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
        for line in command.split('\n') {
            content.push(Line::styled(
                line.to_string(),
                Style::default().fg(theme.text),
            ));
        }
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

/// 命令补全菜单（输入以 `/` 开头时弹出；↑/↓ 选择、Tab 补全、Enter 选中）。
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
    // §防御：selected 可能因 items 刷新指向旧索引——clamp 到有效范围。
    let selected = menu.selected.min(total - 1);
    let visible = (rect.height as usize).max(1);
    // 长菜单（如 /sessions 多会话）：可视窗口跟随选中项，上下用 … 表示有更多；
    // 否则选中项超出可视区时用户看不到当前选择。
    let top_ellipsis = total > visible;
    let bottom_ellipsis = total > visible;
    let window_rows = visible
        .saturating_sub(usize::from(top_ellipsis) + usize::from(bottom_ellipsis))
        .max(1);
    let start = if total > window_rows {
        (selected.saturating_sub(window_rows / 2)).min(total - window_rows)
    } else {
        0
    };
    let end = (start + window_rows).min(total);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if top_ellipsis {
        lines.push(Line::styled("…", Style::default().fg(theme.muted)));
    }
    for (i, (name, desc)) in menu.items.iter().enumerate().skip(start).take(end - start) {
        let selected = i == selected;
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
            model::MenuKind::File => name.clone(),
            // §用户诉求（恢复会话可判断）：会话菜单主列显示名字（首条消息摘要
            // + 时间 + 事件数），UUID 缩短为辅助列——不再让完整哈希抢视觉主体。
            model::MenuKind::Session => desc.to_string(),
        };
        let sub = match menu.kind {
            model::MenuKind::Session => {
                let short: String = name.chars().take(13).collect();
                format!("  (id {short}…)")
            }
            _ => format!("  {desc}"),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} {label}"), style),
            Span::styled(sub, Style::default().fg(theme.muted)),
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
    let mut cache: HashMap<(u64, u16), RenderedMarkdown> = HashMap::new();
    let mut wrap_cache: HashMap<(EntryId, u16), Vec<WrappedRow>> = HashMap::new();
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
        let mut wrap_cache: HashMap<(EntryId, u16), Vec<WrappedRow>> = HashMap::new();
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
        let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        // §美化：每条 Assistant 消息后插 1 空行 → 10 条 = 20 行。
        // 跟随模式：窗口是最后 4 行，前 16 行提交到 scrollback。
        assert_eq!(plan.window.len(), 4);
        assert_eq!(plan.overflow.len(), 16);
        assert_eq!(plan.committed_after, 16);
        let window_text: String = plan
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(window_text.contains("line 9"), "窗口应包含最新行");
        assert!(!window_text.contains("line 0"), "窗口不应包含已提交行");

        // 第二次调用：无新 overflow。
        let plan2 = plan_window_simple(
            &mut view,
            theme::Theme::omp(),
            80,
            4,
            plan.committed_after,
            false,
            &mut cache,
        );
        assert!(plan2.overflow.is_empty());
        assert_eq!(plan2.committed_after, 16);
    }

    #[test]
    fn plan_window_freezes_commits_when_scrolled() {
        let mut view = ViewModel::default();
        for i in 0..10 {
            view.push_line(LineKind::Assistant, format!("line {i}"));
        }
        let mut cache = HashMap::new();
        // 先布局一次（Follow 建立 layout_top = 视口顶部行）。
        let _ = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        // §美化：10 条消息 = 20 行；Follow 顶部行 = 16。
        // TUI v2：翻页 = 锚点（视口顶部）上移 2 行 → 窗口 [14, 18)。
        view.scroll_up(2);
        let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        // Locked：不提交到 scrollback。
        assert_eq!(plan.window.len(), 4);
        assert!(plan.overflow.is_empty());
        assert_eq!(plan.committed_after, 0);
        let window_text: String = plan
            .window
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // 行 14 = m7，行 16 = m8（m_i 在行 2i，空行在 2i+1）。
        assert!(window_text.contains("line 7") && window_text.contains("line 8"));
        assert!(!window_text.contains("line 9"));
        // Locked 保持：新内容不移动视口（§58 场景 A）。
        view.push_line(LineKind::Assistant, "line 10 new".to_string());
        let plan3 = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
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
        let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
        // §美化：整卡可点击——主行与内容行都带 Tool hit（轻点任意行展开；
        // §美化：整卡可点击——主行与内容行都带 Tool hit；卡片后的留白
        // 空行（间隔）不可点。row_hits 与 window 等长。
        assert!(plan.window.len() >= 2, "卡片含主行+正文: {:?}", plan.window);
        assert!(
            matches!(&plan.row_hits[0], Some((HitTarget::Tool(id), end)) if id == "c1" && *end > 0),
            "主行必须可点击: {:?}",
            plan.row_hits[0]
        );
        for (i, (line, hit)) in plan.window.iter().zip(plan.row_hits.iter()).enumerate() {
            if line.spans.is_empty() {
                // 留白空行（卡片间间隔）。
                continue;
            }
            assert!(
                matches!(hit, Some((HitTarget::Tool(id), end)) if id == "c1" && *end > 0),
                "整卡每行（第 {i} 行）都可点击展开: {hit:?}"
            );
        }
    }

    /// §用户诉求（修复计划技能锁死）：整体替换后 Completed 历史在 items 前，
    /// 计划条必须优先显示活跃项，不能把进行中的计划挤出屏。
    #[test]
    fn plan_strip_prefers_active_items_over_history() {
        use crate::tool::plan::{Plan, PlanItem, PlanStatus};
        let plan = Plan {
            explanation: None,
            items: vec![
                PlanItem {
                    text: "old1".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    text: "old2".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    text: "new1".into(),
                    status: PlanStatus::InProgress,
                },
                PlanItem {
                    text: "new2".into(),
                    status: PlanStatus::Pending,
                },
                PlanItem {
                    text: "new3".into(),
                    status: PlanStatus::Pending,
                },
            ],
        };
        let shown = plan_display_items(&plan);
        let texts: Vec<&str> = shown.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["new1", "new2", "new3"], "活跃项优先于历史");
        // 活跃不足 3 条时用 Completed 补齐（沉底）。
        let plan2 = Plan {
            explanation: None,
            items: vec![
                PlanItem {
                    text: "old1".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    text: "new1".into(),
                    status: PlanStatus::InProgress,
                },
            ],
        };
        let texts2: Vec<&str> = plan_display_items(&plan2)
            .iter()
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(texts2, vec!["new1", "old1"]);
        // 全部完成：只显示最近的完成历史。
        let plan3 = Plan {
            explanation: None,
            items: vec![
                PlanItem {
                    text: "a".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    text: "b".into(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    text: "c".into(),
                    status: PlanStatus::Completed,
                },
            ],
        };
        assert_eq!(plan_display_items(&plan3).len(), 3);
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
        // §美化：10 条消息 = 20 行；先提交 16 行（含每条消息后的留白空行）。
        let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
        assert_eq!(plan.committed_after, 16);
        // resize：不产生 overflow，提交位置重置为窗口起点。
        let plan2 = plan_window_simple(
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
        assert_eq!(plan2.committed_after, 16);
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
        let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
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
        let plan_top =
            plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
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
        let plan_new =
            plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
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
        let plan_bottom =
            plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
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
        let plan_newest =
            plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
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
        // §成熟化：rust 代码块经 syntect 高亮，行被 tokenize 为多个 span；
        // 断言整行文本（而非 spans[0]，后者是首 token）。
        let line_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(line_text, "fn main() {}");
        // 语法高亮生效：至少一个 token 使用语义色（keyword 等），非纯 muted。
        let has_semantic = lines[0]
            .spans
            .iter()
            .any(|s| s.style.fg.is_some() && s.style.fg != Some(theme.muted));
        assert!(
            has_semantic,
            "rust 代码块必须有语法高亮: {:?}",
            lines[0].spans
        );
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

    /// §用户诉求：unified diff 渲染为用户友好形式——文件头隐藏、hunk 头变
    /// 分隔行、内容行带真实行号；`+` 绿、`-` 红（只改前景色）。
    #[test]
    fn diff_lines_render_with_add_remove_colors() {
        let theme = theme::Theme::omp();
        let diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n }";
        let lines = render_diff_lines(diff, theme);
        // 文件头（---/+++）隐藏；hunk 头 → 1 行分隔；内容 4 行 → 共 5 行。
        assert_eq!(lines.len(), 5, "文件头隐藏、@@ 变分隔行: {lines:?}");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            !text.contains("--- a/") && !text.contains("+++ b/"),
            "文件头必须隐藏: {text:?}"
        );
        assert!(text.contains("⋯"), "@@ 渲染为分隔行: {text:?}");
        // - 行：行号 + 红字（span 级 error fg），无背景。
        let minus = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1")))
            .expect("找到 - 行");
        assert!(
            minus.spans.iter().any(|s| s.style.fg == Some(theme.error)),
            "- 行红字: {minus:?}"
        );
        assert!(
            minus.spans.iter().all(|s| s.style.bg.is_none()),
            "diff 行不带背景（只改前景色）"
        );
        // + 行：行号 + 绿字（span 级 success fg），无背景。
        let plus = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 2")))
            .expect("找到 + 行");
        assert!(
            plus.spans.iter().any(|s| s.style.fg == Some(theme.success)),
            "+ 行绿字: {plus:?}"
        );
        assert!(
            plus.spans.iter().all(|s| s.style.bg.is_none()),
            "diff 行不带背景（只改前景色）"
        );
        // 行号：- 用旧行号 1，+ 用新行号 2。
        let minus_no = minus
            .spans
            .iter()
            .find(|s| s.content.trim().chars().all(|c| c.is_ascii_digit()))
            .expect("- 行有行号 span");
        assert_eq!(minus_no.content.trim(), "1", "- 行显示旧行号");
        let plus_no = plus
            .spans
            .iter()
            .find(|s| s.content.trim().chars().all(|c| c.is_ascii_digit()))
            .expect("+ 行有行号 span");
        assert_eq!(plus_no.content.trim(), "2", "+ 行显示新行号");
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
                "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-    let x = 1;\n+    let x = 2;\n".into(),
            ),
            output_truncated: false,
            expanded: false, // 未展开——diff 仍应显示
            line_number_start: None,
                    collapsed_lines: 10,
                    started_at_ms: None,
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
        // diff 行只改前景色（红/绿字）；红绿背景不出现（面板底统一由卡片承担，
        // Line 级 style 无 bg）。
        let minus = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1")))
            .expect("找到 - 行");
        assert!(
            minus.spans.iter().any(|s| s.style.fg == Some(theme.error)),
            "删除行红字: {minus:?}"
        );
        assert_eq!(minus.style.bg, None, "diff 行不带红底（Line 级）");
        let plus = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 2")))
            .expect("找到 + 行");
        assert!(
            plus.spans.iter().any(|s| s.style.fg == Some(theme.success)),
            "新增行绿字: {plus:?}"
        );
        assert_eq!(plus.style.bg, None, "diff 行不带绿底（Line 级）");
    }

    /// §用户诉求：diff 自动展开但限长——未展开时只显示前 N 行 + 折叠提示，
    /// 展开时显示全部。
    #[test]
    fn tool_card_diff_limits_length_when_collapsed() {
        let theme = theme::Theme::omp();
        // 30 行 diff（带 hunk 头，模拟 unified_diff 真实格式）。
        let mut diff = "@@ -1,30 +1,30 @@\n".to_string();
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
            line_number_start: None,
            collapsed_lines: 10,
            started_at_ms: None,
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
        // §修复：折叠提示行每个 span 烙 panel 底（wrap 只保留 span 级 bg，
        // Line 级会丢——提示文字不得落到终端底色）。
        let hint_line = collapsed
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("点击展开")))
            .expect("找到提示行");
        assert!(
            hint_line
                .spans
                .iter()
                .filter(|s| !s.content.is_empty())
                .all(|s| s.style.bg == Some(theme.panel)),
            "提示行每段都带 panel 底: {:?}",
            hint_line
                .spans
                .iter()
                .map(|s| (s.content.as_ref(), s.style.bg))
                .collect::<Vec<_>>()
        );
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
            line_number_start: None,
            collapsed_lines: 10,
            started_at_ms: None,
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
            line_number_start: None,
            collapsed_lines: 10,
            started_at_ms: None,
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
            line_number_start: None,
            collapsed_lines: 10,
            started_at_ms: None,
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
            line_number_start: None,
                    collapsed_lines: 10,
                    started_at_ms: None,
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
            line_number_start: None,
            collapsed_lines: 10,
            started_at_ms: None,
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

    /// §用户诉求：read 卡片正文显示真实文件行号（从 line_number_start 递增）。
    #[test]
    fn read_card_shows_real_line_numbers() {
        let theme = theme::Theme::omp();
        let card = ToolCard {
            id: "c".into(),
            name: "read".into(),
            target: Some("src/lib.rs".into()),
            command: None,
            state: ToolCardState::Done {
                status: crate::tool::outcome::ToolStatus::Succeeded,
                duration_ms: 5,
                exit_code: Some(0),
            },
            output: Some("fn a() {}\nfn b() {}\n".into()),
            diff: None,
            output_truncated: false,
            expanded: true,
            line_number_start: Some(201),
            collapsed_lines: 10,
            started_at_ms: None,
            tail: None,
        };
        let lines = tool_card_lines(&card, 0, theme, 100);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            text.contains("201 │") && text.contains("202 │"),
            "必须显示递增的真实行号: {text:?}"
        );
        assert!(
            text.contains("fn a() {}") && text.contains("fn b() {}"),
            "正文保留: {text:?}"
        );
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
    // §美化：footer 上方加了分隔线 → trans_area 高度 = 24-1(input)-1(rule)-1(footer)=21，
    // overlay 居中 (2,1,76,19)，底边框在行 19。内部未覆盖区域取行 18
    //（边框上方内部行）→ 必须被清空为空格。
    assert_eq!(
        buf[(40, 18)].symbol(),
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

/// §性能：wrap 缓存——历史 entry 跨帧复用；transcript 结构变化后
/// （push_line 等 bump revision）缓存由 draw 层清空重建；active_hit/search
/// 高亮在窗口层应用，不依赖缓存内容。
#[test]
fn wrap_cache_reuses_and_invalidates() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "第一行 hello");
    let rev0 = view.transcript_revision;
    let mut cache = HashMap::new();
    let mut wrap_cache: HashMap<(EntryId, u16), Vec<WrappedRow>> = HashMap::new();
    let plan1 = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    assert_eq!(wrap_cache.len(), 1, "历史 entry 必须写入缓存");
    assert_eq!(plan1.window.len(), 2, "消息行 + 留白空行");
    // 再次调用（revision 未变）→ 命中缓存，不新增条目。
    let _plan2 = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    assert_eq!(wrap_cache.len(), 1, "未变化时命中缓存不增长");
    // 新增 entry → revision bump；draw 层会清空缓存（此处模拟）。
    view.push_line(LineKind::Assistant, "第二行 world");
    assert_ne!(
        view.transcript_revision, rev0,
        "push_line 必须 bump revision"
    );
    wrap_cache.clear();
    let _plan3 = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    assert_eq!(wrap_cache.len(), 2, "两个历史 entry 都缓存");
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
    // §美化：6 条消息 + 6 留白空行 = 12 行；视口 12 容纳全部。
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 12, 0, false, &mut cache);
    assert_eq!(
        plan.window.len(),
        12,
        "6 条消息 + 6 留白各一行: {:?}",
        plan.window
    );
    let has_reversed = |idx: usize| {
        plan.window[idx]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
    };
    // 正文行布局：entry_id 从 1 起，entry_i 正文在行 2*(i-1)、留白在 2*(i-1)+1。
    // 选中 entry2/3 正文 → window 行 2、4 高亮；留白行 3、5 与未选行不高亮。
    assert!(
        has_reversed(2) && has_reversed(4),
        "选中 entry2/3 正文必须高亮"
    );
    assert!(!has_reversed(3), "留白空行不得高亮");
    assert!(!has_reversed(5), "留白空行不得高亮");
    assert!(
        !has_reversed(0) && !has_reversed(1) && !has_reversed(11),
        "未选中行不高亮"
    );
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
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    // 语义行与窗口行等长（每 entry 至少 1 行）。
    assert_eq!(
        plan.semantic_rows.len(),
        plan.window.len(),
        "semantic_rows 必须与 window 等长"
    );
    assert!(!plan.semantic_rows.is_empty());
    // §美化：消息块间有留白空行（text 为空、不可选）；非空行断言正文语义。
    for row in plan.semantic_rows.iter() {
        let row = row.as_ref().expect("窗口行必须有语义映射");
        assert!(
            row.entry_id == EntryId(1) || row.entry_id == EntryId(2),
            "归属错误 entry: {:?}",
            row.entry_id
        );
        if row.text.is_empty() {
            // 留白空行：合法分隔（不可选），跳过 rail/非空断言。
            continue;
        }
        assert!(
            !row.text.starts_with('│'),
            "语义文本不得含 rail 前缀: {:?}",
            row.text
        );
        assert!(!row.text.is_empty(), "正文行语义文本不得为空");
    }
    // 语义文本必须能被定位：entry 1 的语义文本就是 "hello world"（markdown 渲染无装饰）。
    let e1_text = plan
        .semantic_rows
        .iter()
        .find_map(|r| {
            let row = r.as_ref()?;
            (row.entry_id == EntryId(1)).then(|| row.text.clone())
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
        links: Vec::new(),
        rail: Some(Span::styled("│ ", Style::default())),
    };
    let rows = wrap_with_semantic(vec![line], vec![None], &[semantic], 80, entry_id);
    assert_eq!(rows.len(), 1, "短文本单行");
    let row = &rows[0];
    let row_sem = row.semantic.as_ref().expect("必须有语义映射");
    assert_eq!(row_sem.entry_id, entry_id);
    assert_eq!(row_sem.char_start, 0, "首行语义从 entry 偏移 0 开始");
    assert_eq!(row_sem.text, "hello");
    assert_eq!(
        row_sem.decor, 7,
        "短文本首行 decor 必须是 rail 宽度（P0-2 修复）"
    );

    // 长文本换行：语义行数 == 视觉行数，续行 decor=rail 宽、带竖线前缀。
    let long = "x".repeat(90); // 90 cell，宽 40 → 3 行。
    let line2 = Line::from(vec![
        Span::styled("│ ", Style::default()),
        Span::styled("you  ", Style::default()),
        Span::styled(long.clone(), Style::default()),
    ]);
    let semantic2 = SemanticLine {
        text: long.clone(),
        decor_cells: 7,
        links: Vec::new(),
        rail: Some(Span::styled("│ ", Style::default())),
    };
    let rows = wrap_with_semantic(vec![line2], vec![None], &[semantic2], 40, entry_id);
    // 宽 40：首行内容容量 = 40-7 = 33 cell → "x"×33；续行带 rail（2）→
    // 内容容量 38、19。
    assert_eq!(rows.len(), 3, "90 cell 内容宽 40 → 3 视觉行");
    // 首行 decor=7；续行 decor=rail 宽（2）——竖线前缀延续。
    assert_eq!(
        rows[0].semantic.as_ref().unwrap().decor,
        7,
        "首行 decor=rail"
    );
    assert_eq!(
        rows[1].semantic.as_ref().unwrap().decor,
        2,
        "续行 decor=rail 宽（竖线不截断）"
    );
    assert_eq!(
        rows[2].semantic.as_ref().unwrap().decor,
        2,
        "末行 decor=rail 宽"
    );
    // 续行视觉行首 span 必须是竖线（§用户诉求：换行竖线连续）。
    for (i, row) in rows.iter().enumerate().skip(1) {
        let first = row
            .line
            .spans
            .first()
            .map(|s| s.content.as_ref())
            .unwrap_or("");
        assert_eq!(first, "│ ", "续行 {i} 首 span 必须是竖线前缀");
    }
    // 语义文本拼接 = 原文。
    let joined: String = rows
        .iter()
        .filter_map(|r| r.semantic.as_ref().map(|s| s.text.as_str()))
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
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    let first = plan.semantic_rows[0]
        .as_ref()
        .expect("第一行必须有语义映射");
    let text = first.text.as_str();
    let char_start = first.char_start;
    let decor = first.decor;
    assert_eq!(text, "hello world");
    // 模拟 hit_text：视觉 cell 列 → 语义 offset。
    // 屏幕列 = rect.x + decor + 目标字符前的 cell 宽度。
    let to_offset = |screen_col: usize| -> usize {
        let semantic_col = screen_col.saturating_sub(decor);
        cell_to_char(text, semantic_col) + char_start
    };
    // 屏幕列指向 "world" 的开头（decor + "hello " = 7 + 6 = 13 cell 列）。
    let w_col = decor + chars_to_cells("hello ", 6);
    let w_offset = to_offset(w_col);
    assert_eq!(w_offset, 6, "w 的 offset 应为 6");
    // 选中 "wor"（offset 6..9）。
    view.selection_start(TextPosition {
        entry_id: first.entry_id,
        offset: w_offset,
    });
    view.selection_update(TextPosition {
        entry_id: first.entry_id,
        offset: w_offset + 3,
    });
    view.selection_end();
    assert_eq!(view.selected_text(), "wor", "必须精确选中 3 个字符，非整行");
    // CJK：选中 2 个汉字。§美化：entry2 前有留白空行，按 entry 定位
    //（不用固定 index——留白行 text 为空）。
    view.push_line(LineKind::Assistant, "你好世界");
    let mut cache2 = HashMap::new();
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache2,
    );
    let second = plan2
        .semantic_rows
        .iter()
        .find_map(|r| {
            let row = r.as_ref()?;
            (row.entry_id == EntryId(2)).then_some(row)
        })
        .expect("entry 2 必须有语义映射");
    assert_eq!(second.text, "你好世界");
    view.selection_start(TextPosition {
        entry_id: second.entry_id,
        offset: 1,
    });
    view.selection_update(TextPosition {
        entry_id: second.entry_id,
        offset: 3,
    });
    view.selection_end();
    assert_eq!(view.selected_text(), "好世", "CJK 按 char 精确选中");
}

/// §美化：User 消息 = 左竖线(┃) + panel 背景块（行内首个 span 带 bg），
/// Assistant 保持无背景 rail —— 形成"用户有底、助手裸文本"层次。
#[test]
fn user_message_gets_panel_background_assistant_stays_plain() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "帮我修复");
    view.push_line(LineKind::Assistant, "好的");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 12, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 通过语义行定位正文行索引（wrap 后 span 逐字符拆分，不能直接按文本找）。
    let find_row = |target: &str| -> usize {
        plan.semantic_rows
            .iter()
            .position(|r| r.as_ref().is_some_and(|row| row.text == target))
            .expect("目标消息必须渲染")
    };
    let user_row = find_row("帮我修复");
    assert!(
        plan.window[user_row]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(theme.panel)),
        "用户消息行首 span 必须带 panel 背景: {:?}",
        plan.window[user_row].spans
    );
    let assistant_row = find_row("好的");
    assert!(
        plan.window[assistant_row]
            .spans
            .iter()
            .all(|s| s.style.bg.is_none()),
        "assistant 消息保持裸文本（无背景）: {:?}",
        plan.window[assistant_row].spans
    );
}

/// §美化：消息块间插留白空行（text 为空、不可选）。
#[test]
fn message_blocks_are_separated_by_gap_rows() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "line a");
    view.push_line(LineKind::Assistant, "line b");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    // 每条消息 1 正文 + 1 留白 = 4 行（无折行）。
    assert_eq!(plan.window.len(), 4);
    // 第 1、3 行是留白空行（语义为空，不可选）。
    assert_eq!(plan.semantic_rows[1].as_ref().unwrap().text, "");
    assert_eq!(plan.semantic_rows[3].as_ref().unwrap().text, "");
    // 正文行语义非空。
    assert_eq!(plan.semantic_rows[0].as_ref().unwrap().text, "line a");
    assert_eq!(plan.semantic_rows[2].as_ref().unwrap().text, "line b");
}

/// §美化：thinking 加 ◆ 图标前缀（与正文/工具区分）。
#[test]
fn thinking_lines_carry_icon_prefix() {
    let mut view = ViewModel {
        collapsed_lines: 10, // 单行 thinking 不折叠（默认 0 会折叠）。
        ..Default::default()
    };
    view.push_line(LineKind::Reasoning, "先分析再动手");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 12, 0, false, &mut cache);
    // 首行是 thinking（◆ 前缀 + 正文）；语义文本不含前缀。
    let first = &plan.window[0];
    let text: String = first.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("◆ 思考"),
        "thinking 行带 ◆ 图标前缀: {text:?}"
    );
    assert!(text.contains("先分析再动手"), "正文保留: {text:?}");
    // 语义文本是纯正文（无前缀，不可复制前缀）。
    let semantic = plan.semantic_rows[0].as_ref().unwrap();
    assert_eq!(semantic.text, "先分析再动手");
}

/// §美化：代码块语法高亮路径统一 surface_subtle 背景（与 fallback 一致）。
#[test]
fn highlighted_code_block_has_background() {
    let theme = theme::Theme::omp();
    let lines = highlight::highlight_code_block("fn main() { let x = 1; }", Some("rust"), theme)
        .expect("rust 可解析");
    assert_eq!(lines.len(), 1);
    for span in &lines[0].spans {
        assert_eq!(
            span.style.bg,
            Some(theme.surface_subtle),
            "语法高亮 span 必须带 surface_subtle 背景: {:?}",
            span
        );
    }
}

/// BUG 回归：assistant 裸文本行含行内 code 时，行尾 padding 不得继承
/// code 的 surface_subtle 背景（此前 find_map 抓到行内第一个 bg span）。
#[test]
fn inline_code_bg_does_not_leak_into_trailing_padding() {
    let mut view = ViewModel::default();
    // assistant 消息行首 rail 无背景；行内含 inline code（surface_subtle 底）。
    view.push_line(
        LineKind::Assistant,
        r"请查看 `snake\src\main.rs` 是否已存在",
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 60, 10, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 定位含 "main.rs" 的行。
    let row_idx = plan
        .semantic_rows
        .iter()
        .position(|r| r.as_ref().is_some_and(|s| s.text.contains("main.rs")))
        .expect("消息必须渲染");
    let line = &plan.window[row_idx];
    // inline code span 本身保留 surface_subtle 背景（wrap 逐字符拆分 span）。
    let has_code_bg = line.spans.iter().any(|s| {
        !s.content.is_empty()
            && !s.content.chars().all(|c| c == ' ')
            && s.style.bg == Some(theme.surface_subtle)
    });
    assert!(has_code_bg, "行内 code 背景必须保留: {:?}", line.spans);
    // 行尾 padding（内容全空格的 span）不得带任何背景（不再泄漏）。
    for span in &line.spans {
        if span.content.chars().all(|c| c == ' ') && !span.content.is_empty() {
            assert_eq!(
                span.style.bg, None,
                "行尾 padding 不得继承行内 code 背景: {:?}",
                span
            );
        }
    }
}

/// §用户诉求：diff 行只改前景色——面板底统一由卡片承担，行尾 padding
/// 用 panel 填满，绝不出现红/绿背景（也不再需要 Line 级红绿 fallback）。
#[test]
fn diff_line_padding_keeps_panel_background() {
    let mut view = ViewModel {
        collapsed_lines: 10, // 展开 diff 正文（默认 0 折叠）。
        ..Default::default()
    };
    view.begin_tool("c1", "edit", Some("src/main.rs".into()), None);
    view.finish_tool(
        ("c1", "edit"),
        crate::tool::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "",
        Some("-    let x = 1;\n+    let x = 2;".into()),
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 50, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 定位删除行（含 "let x = 1"）。
    let row_idx = plan
        .semantic_rows
        .iter()
        .position(|r| r.as_ref().is_some_and(|s| s.text.contains("let x = 1")))
        .expect("diff 删除行必须渲染");
    let line = &plan.window[row_idx];
    // diff 行前景 = error（红字），背景统一 panel（无红/绿底）。
    assert!(
        line.spans.iter().any(|s| s.style.fg == Some(theme.error)),
        "diff 删除行必须红字: {:?}",
        line.spans
    );
    assert!(
        line.spans
            .iter()
            .all(|s| s.style.bg != Some(theme.error) && s.style.bg != Some(theme.success)),
        "diff 行不得带红/绿背景: {:?}",
        line.spans
    );
    // 行尾 padding 用 panel 底填满（不是红/绿）。
    let has_panel_pad = line.spans.iter().any(|s| {
        !s.content.is_empty()
            && s.content.chars().all(|c| c == ' ')
            && s.style.bg == Some(theme.panel)
    });
    assert!(
        has_panel_pad,
        "diff 行尾必须用 panel 底填充到满宽: {:?}",
        line.spans
    );
}

/// §修复回归：卡片正文文字区背景与卡片面板一致（不落到终端底色）。
/// 主行 name/内容行正文都烙 panel；inline code（surface_subtle）保留。
#[test]
fn tool_card_body_bg_matches_panel() {
    let mut view = ViewModel {
        collapsed_lines: 10, // 显示内容行正文（默认 0 折叠）。
        ..Default::default()
    };
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        crate::tool::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "第一行输出\n第二行输出",
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 主行 name span 带 panel（wrap 逐字符拆分，任一 "bash" 字符即可）。
    let header_row = &plan.window[0];
    assert!(
        header_row
            .spans
            .iter()
            .any(|s| s.content == "b" && s.style.bg == Some(theme.panel)),
        "主行 name 必须烙 panel 底: {:?}",
        header_row.spans
    );
    // 主行所有非空 span 不得残留无背景（含分隔空格）。
    for span in &header_row.spans {
        if !span.content.is_empty() {
            assert!(
                span.style.bg.is_some(),
                "主行每段文字都带面板底: {:?}",
                span
            );
        }
    }
    // 内容行正文（"第一行输出"）带 panel。
    let body_row = plan
        .window
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content == "第"))
        .expect("内容行必须渲染");
    assert!(
        body_row
            .spans
            .iter()
            .any(|s| s.content == "一" && s.style.bg == Some(theme.panel)),
        "内容行正文必须烙 panel 底: {:?}",
        body_row.spans
    );
}

/// §修复回归：User 消息正文背景与面板一致（inline code 保留自身背景）。
#[test]
fn user_message_body_bg_matches_panel() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, r"修复 `snake\src\main.rs`");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    let row = plan
        .window
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content == "修"))
        .expect("用户消息必须渲染");
    // 正文普通字符烙 panel。
    assert!(
        row.spans
            .iter()
            .any(|s| s.content == "修" && s.style.bg == Some(theme.panel)),
        "用户消息正文必须烙 panel 底: {:?}",
        row.spans
    );
    // inline code 保留 surface_subtle（不被 panel 覆盖）。
    assert!(
        row.spans
            .iter()
            .any(|s| s.content == "s" && s.style.bg == Some(theme.surface_subtle)),
        "inline code 背景保留: {:?}",
        row.spans
    );
}

/// §修复：thinking 卡片化——panel 底 + 左竖线 + 整卡可点展开。
#[test]
fn thinking_renders_as_panel_card() {
    // 配置 collapsed_lines=6：8 行 thinking → 折叠态（8 > 6，且默认 0 只显示
    // 主行；这里显式 6 复现“折叠线”场景）。
    let mut view = ViewModel {
        collapsed_lines: 6,
        ..Default::default()
    };
    // 8 行 thinking → 折叠态。
    let mut text = String::new();
    for i in 0..8 {
        text.push_str(&format!("思考第{i}行\n"));
    }
    view.push_line(LineKind::Reasoning, text);
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 折叠态单行：panel 底 + 整行可点（Reasoning hit）。
    assert_eq!(plan.window.len(), 2, "折叠单行 + 留白空行");
    let card_line = &plan.window[0];
    assert!(
        card_line
            .spans
            .iter()
            .any(|s| s.style.bg == Some(theme.panel)),
        "thinking 卡片带 panel 底: {:?}",
        card_line.spans
    );
    let card_text: String = card_line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        card_text.contains("点击展开"),
        "折叠态显示点击展开提示: {card_text:?}"
    );
    assert!(
        matches!(
            &plan.row_hits[0],
            Some((HitTarget::Reasoning(id), end)) if *end > 0
        ),
        "thinking 折叠卡整行可点: {:?}",
        plan.row_hits[0]
    );
    // 留白空行在卡片后（第 2 行）。
    assert_eq!(plan.semantic_rows[1].as_ref().unwrap().text, "");
}

/// §修复：展开态 thinking 逐行 panel 底；折叠提示行带 panel 底。
#[test]
fn thinking_expanded_rows_keep_panel_and_toggle_hint() {
    let mut view = ViewModel {
        collapsed_lines: 6, // 8 行 > 6 → 溢出（展开态有折叠提示行）。
        ..Default::default()
    };
    let mut text = String::new();
    // 8 行（末尾不换行——split('\n') 才恰好 8 段，避免空尾段）。
    for i in 0..7 {
        text.push_str(&format!("思考第{i}行\n"));
    }
    text.push_str("思考第7行");
    view.push_line(LineKind::Reasoning, text);
    let entry_id = view.transcript[0].id();
    view.toggle_reasoning_expanded(entry_id);
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 展开态：8 行正文 + 1 折叠提示 + 1 留白 = 10 行。
    assert_eq!(plan.window.len(), 10);
    // 每行（含折叠提示）都带 panel 底。
    for (i, line) in plan.window.iter().enumerate() {
        if line.spans.is_empty() {
            continue; // 留白
        }
        assert!(
            line.spans.iter().any(|s| s.style.bg == Some(theme.panel)),
            "展开行 {i} 必须带 panel 底: {:?}",
            line.spans
        );
    }
    // 折叠提示行（"点击折叠"）带 panel 底。
    let hint_row = plan
        .window
        .iter()
        .find(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            text.contains("点击折叠")
        })
        .expect("展开态必须有折叠提示");
    assert!(
        hint_row
            .spans
            .iter()
            .any(|s| s.style.bg == Some(theme.panel)),
        "折叠提示行带 panel 底"
    );
}

/// §修复：live reasoning 展开态每行都可点击（点正文任意处收起）——
/// 否则展开后无任何 Reasoning hit，点击无法触发收起（“打开后关不上”）。
#[test]
fn live_reasoning_expanded_rows_are_clickable_to_collapse() {
    let mut view = ViewModel::default();
    // 流式 reasoning（live 区，非 transcript）。
    let text = "第一行思考\n第二行思考\n第三行思考";
    view.push_stream_delta(LineKind::Reasoning, text);
    view.reasoning_visible = true; // 全局展开
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let live_entry = view
        .live
        .reasoning
        .as_ref()
        .expect("live reasoning")
        .entry_id;
    let mut content_hits = 0usize;
    for (line, hit) in plan.window.iter().zip(plan.row_hits.iter()) {
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if rendered.contains("行思考") {
            assert!(
                matches!(hit, Some((HitTarget::Reasoning(id), end)) if *id == live_entry && *end > 0),
                "live 展开正文行必须可点（id={live_entry:?}）: {rendered:?} hit={hit:?}"
            );
            content_hits += 1;
        }
    }
    assert_eq!(content_hits, 3, "3 行思考正文都应是 Reasoning hit");
    // 折叠提示行也可点（与历史展开态一致）。
    assert!(
        plan.window
            .iter()
            .zip(plan.row_hits.iter())
            .any(|(line, hit)| {
                let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                rendered.contains("点击折叠") && hit.is_some()
            }),
        "展开态必须有可点击的折叠提示行"
    );
}

/// §修复：卡片内选中一小段 → 只高亮该行（不得整卡全选），
/// 且复制内容与高亮一致（offset 对齐）。
#[test]
fn selecting_part_of_tool_card_highlights_only_that_row() {
    use crate::tui::interaction::TextPosition;
    use crate::tui::scroll::EntryId;
    let mut view = ViewModel {
        collapsed_lines: 10, // 显示内容行（默认 0 折叠）。
        ..Default::default()
    };
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        crate::tool::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "第一行输出\n第二行输出\n第三行输出",
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    // 定位内容行（含 "一" 的行）的语义 char_start。
    let body_row = plan
        .semantic_rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.as_ref().is_some_and(|s| s.text.starts_with("第一行输出")))
        .map(|(i, r)| (i, r.as_ref().unwrap().char_start))
        .expect("内容行必须有语义");
    let (row_idx, char_start) = body_row;
    // 选中该行前 2 个字符（"第一"）。
    view.selection_start(TextPosition {
        entry_id: EntryId(1),
        offset: char_start,
    });
    view.selection_update(TextPosition {
        entry_id: EntryId(1),
        offset: char_start + 2,
    });
    view.selection_end();
    let mut cache2 = HashMap::new();
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        20,
        0,
        false,
        &mut cache2,
    );
    // 统计带 REVERSED 高亮的行。
    let has_reversed = |line: &Line<'static>| {
        line.spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
    };
    let highlighted: Vec<usize> = plan2
        .window
        .iter()
        .enumerate()
        .filter(|(_, l)| has_reversed(l))
        .map(|(i, _)| i)
        .collect();
    assert!(
        highlighted.len() == 1 && highlighted[0] == row_idx,
        "选中内容行中段只应高亮该行，实际 {:?}（目标行 {row_idx}）",
        highlighted
    );
    // 复制内容 = "第一"（offset 对齐：渲染 char_start == selected_text 同 offset）。
    assert_eq!(view.selected_text(), "第一", "复制内容必须与高亮一致");
}

/// §用户诉求：默认 collapsed_lines=0 → 工具卡片折叠态只显示主行，
/// 不显示正文，也不显示「点击展开」提示（干净的主行摘要）。
#[test]
fn tool_card_default_zero_collapses_to_main_row_only() {
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
        output: Some("line1\nline2\nline3\n".into()),
        diff: None,
        output_truncated: false,
        expanded: false,
        line_number_start: None,
        collapsed_lines: 0,
        started_at_ms: None,
        tail: None,
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        !text.contains("line1") && !text.contains("line2"),
        "collapsed_lines=0 折叠态不显示正文: {text:?}"
    );
    assert!(
        !text.contains("点击展开") && !text.contains("点击折叠"),
        "collapsed_lines=0 折叠态只显示主行（无提示）: {text:?}"
    );
}

/// §用户诉求（紧凑）：collapsed_lines==0 且未展开的工具卡片之间取消间隔
/// （只显示主行）；展开态卡片保留间隔（多行块需要分隔）。
#[test]
fn zero_collapsed_tool_cards_have_no_gap_between() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        crate::tool::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "out1",
        None,
    );
    view.begin_tool("c2", "read", Some("file.rs".into()), None);
    view.finish_tool(
        ("c2", "read"),
        crate::tool::outcome::ToolStatus::Succeeded,
        5,
        Some(0),
        "out2",
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    assert!(
        plan.window.iter().all(|l| !l.spans.is_empty()),
        "两张 0 折叠未展开卡之间无空行（紧凑）: {:?}",
        plan.window
            .iter()
            .map(|l| l
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        plan.window.len(),
        2,
        "只主行、无留白空行: {:?}",
        plan.window
    );

    // 展开第一张 → 多行块，其后保留间隔（空行）以分隔。
    view.toggle_expand("c1");
    let mut cache2 = HashMap::new();
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        20,
        0,
        false,
        &mut cache2,
    );
    assert!(
        plan2.window.iter().any(|l| l.spans.is_empty()),
        "展开卡后保留间隔空行: {:?}",
        plan2
            .window
            .iter()
            .map(|l| l
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>())
            .collect::<Vec<_>>()
    );
}

/// §用户诉求：thinking 用 markdown 渲染——展开后代码块带语法高亮背景。
#[test]
fn thinking_expanded_renders_markdown_code_highlight() {
    let mut view = ViewModel {
        collapsed_lines: 10,
        ..Default::default()
    };
    view.push_line(LineKind::Reasoning, "先想一下\n```rust\nlet x = 1;\n```");
    let entry_id = view.transcript[0].id();
    view.toggle_reasoning_expanded(entry_id);
    let mut cache = HashMap::new();
    let theme = theme::Theme::omp();
    let plan = plan_window_simple(&mut view, theme, 80, 20, 0, false, &mut cache);
    let text: String = plan
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("先想一下") && text.contains("let x = 1"),
        "thinking 展开显示 md 内容: {text:?}"
    );
    // 代码行有语法高亮背景（surface_subtle）。
    assert!(
        plan.window.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.bg == Some(theme.surface_subtle))
        }),
        "代码块必须带高亮背景: {:?}",
        plan.window
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.style.bg.map(|b| (s.content.as_ref(), b)))
            .collect::<Vec<_>>()
    );
}
