//! TUI 渲染模型（§16.2 信息层级）。
//!
//! ViewModel 是只读渲染输入：由 app 从 ephemeral 事件构建，
//! renderer 不直接访问 Agent/tool 内部状态。
//!
//! M6+：转录条目分为「消息」（User/Assistant/Reasoning/Tool/System）与
//! 「工具卡片」（单行 icon name duration status，§16.2），
//! 支持思考折叠（Alt+T）、命令补全菜单与累积 token 用量。

use std::collections::HashMap;

use crate::session::Usage;
use crate::tool::outcome::ToolStatus;
use crate::tool::plan::Plan;
use crate::tui::interaction::TextPosition;
use crate::tui::scroll::{EntryId, ScrollAnchor, ScrollMode};

/// 转录行类型（§16.2：普通工具可见、plan call 隐藏由 UI 策略决定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
    /// 单调递增版本号：文本变化时递增，renderer 用它缓存 Markdown 渲染结果。
    pub version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    System,
}

/// 工具卡片（§16.2 + TUI 整改 A2：一个 call 一张卡、主行恒单行）。
///
/// - 主行 = `icon name target metadata` 单 visual line（target 按宽度 ellipsis）；
/// - 运行中实时输出/失败 tail 只在折叠态下方显示有限行数；
/// - 完整 command 与输出进详情 overlay（不重写 scrollback）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCard {
    /// ToolCallId 的字符串形式。
    pub id: String,
    pub name: String,
    /// 主行 target 摘要（bash = 压缩后的命令；其他 = 路径/参数摘要）。
    pub target: Option<String>,
    /// 完整命令（overlay 展示；bash 命令原文，有界）。
    pub command: Option<String>,
    pub state: ToolCardState,
    /// 完整输出（有界累积，≤ MAX_CARD_OUTPUT；成功与失败都保留）。
    pub output: Option<String>,
    /// edit/write 的 unified diff（§用户诉求：默认展开显示红绿 diff）。
    pub diff: Option<String>,
    /// 输出被截断（超过 MAX_CARD_OUTPUT 丢弃中段）。
    pub output_truncated: bool,
    /// 展开状态（active 卡片内联显示完整输出；scrollback 卡片走 overlay）。
    pub expanded: bool,
    /// 失败/超时等终态的关键 tail（有界，折叠态显示最多 4 行）。
    pub tail: Option<String>,
}

/// Stable identity carried by every tool lifecycle event.
///
/// Treating the call id and tool name as one value prevents lifecycle APIs from
/// accepting a mismatched pair and keeps terminal updates below an argument list
/// that obscures their meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolIdentity {
    pub id: String,
    pub name: String,
}

impl<I, N> From<(I, N)> for ToolIdentity
where
    I: Into<String>,
    N: Into<String>,
{
    fn from((id, name): (I, N)) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }
}

/// 卡片输出上限（UI 内存与渲染预算；完整输出仍可通过 read @artifact 读取）。
pub const MAX_CARD_OUTPUT: usize = 32 * 1024;
/// P1-9：assistant/reasoning 单条消息上限（工具卡输出另有 MAX_CARD_OUTPUT）。
/// 超出丢弃中段并标记 truncated，防止 transcript 无限膨胀。
pub const MAX_MESSAGE_CHARS: usize = 64 * 1024;
/// 命令上限（overlay 展示用；正常命令远小于此）。
pub const MAX_CARD_COMMAND: usize = 8 * 1024;
pub const MAX_CARD_DIFF: usize = 64 * 1024;
const MAX_TOOL_TARGET: usize = 4 * 1024;
const MAX_TOOL_NAME: usize = 256;
const MAX_MODAL_BODY: usize = 256 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SEARCH_QUERY: usize = 4 * 1024;
/// 搜索命中数上限（§成熟化：超大转录下防止命中集合无限膨胀；
/// 跳转仍可循环，上限只是截断"最旧命中"）。
const MAX_SEARCH_HITS: usize = 256;

/// 耗时格式化（Overlay 标题与 ToolCard metadata 共用）。
pub fn fmt_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms / 1_000) % 60)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCardState {
    Running,
    Done {
        status: ToolStatus,
        duration_ms: u64,
        /// 进程退出码（run/bash；纯读取工具为 None）。
        exit_code: Option<i32>,
    },
}

/// 转录搜索（TUI v2 §14）：Ctrl+F 打开；命中 → Locked 锚定 + 循环跳转。
/// 搜索范围：User/Assistant/Reasoning/System 消息文本、工具 target/name/tail。
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    /// 命中的 entry（按出现顺序）。
    pub hits: Vec<EntryId>,
    /// 当前命中位置（hits 下标）。
    pub index: usize,
}

impl SearchState {
    pub fn open() -> Self {
        Self::default()
    }

    /// 重新计算命中（大小写不敏感；§成熟化：使用每条目的惰性小写缓存，
    /// 不再每键全量 `to_lowercase`）。命中数上限 `MAX_SEARCH_HITS` 防超大
    /// 转录下命中集合膨胀（跳转仍循环可用）。
    pub fn recompute(&mut self, transcript: &mut [Entry]) {
        let query = self.query.to_lowercase();
        self.hits.clear();
        self.index = 0;
        if query.is_empty() {
            return;
        }
        for entry in transcript.iter_mut() {
            let haystack = entry.search_lower();
            if haystack.contains(&query) {
                self.hits.push(entry.id());
                if self.hits.len() >= MAX_SEARCH_HITS {
                    break;
                }
            }
        }
    }
}

/// 操作型 UI Modal（TUI v2 §42/§61）：/help /settings /doctor /session
/// /sessions /diff 等输出进 Modal，不污染 transcript。
/// 与 Overlay 的区别：Overlay 是“内容详情”，Modal 是“操作面板”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalState {
    /// 标题（如 `/settings`）。
    pub title: String,
    /// 正文（多行）。
    pub body: String,
    /// Modal 内滚动偏移（行）。
    pub scroll: usize,
}

impl ModalState {
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        let title = title.into();
        let body = body.into();
        Self {
            title: crate::tui::text::truncate_middle_utf8(&title, 512, "…"),
            body: crate::tui::text::truncate_middle_utf8(
                &body,
                MAX_MODAL_BODY,
                "\n…[modal truncated]…\n",
            ),
            scroll: 0,
        }
    }
}

/// 详情 Overlay 类型（§成熟化：tool / reasoning / link 三种渲染与交互差异）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    /// 工具卡片详情。
    Tool,
    /// 思考（reasoning）原文。
    Reasoning,
    /// 链接（确认打开 / 复制 URL）。
    Link,
}

/// 详情 Overlay 状态（整改 B：历史/工具详情不重写 scrollback，覆盖显示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayState {
    /// 标题行（如 `bash · succeeded · 3.6s · exit 0`）。
    pub title: String,
    /// 完整命令（bash；其他工具为 None）。
    pub command: Option<String>,
    /// 正文（输出或 reasoning 原文；有界）。
    pub body: String,
    /// 正文被截断。
    pub body_truncated: bool,
    /// Overlay 内滚动偏移（行）。
    pub scroll: usize,
    /// 对应工具卡 id（P2：Alt+[/Alt+] 卡片间切换用；reasoning overlay 为 None）。
    pub tool_id: Option<String>,
    /// Overlay 类型（渲染提示与按键语义用）。
    pub kind: OverlayKind,
}

impl OverlayState {
    /// 工具详情 overlay。
    pub fn for_tool(card: &ToolCard) -> Self {
        let status_word = match &card.state {
            ToolCardState::Running => "running".to_string(),
            ToolCardState::Done { status, .. } => match status {
                ToolStatus::Succeeded => "succeeded".into(),
                ToolStatus::Failed => "failed".into(),
                ToolStatus::TimedOut => "timed_out".into(),
                ToolStatus::Cancelled => "cancelled".into(),
                ToolStatus::Interrupted => "interrupted".into(),
                ToolStatus::Rejected => "rejected".into(),
            },
        };
        let mut title = format!("{} · {status_word}", card.name);
        if let ToolCardState::Done {
            duration_ms,
            exit_code,
            ..
        } = &card.state
        {
            title.push_str(&format!(" · {}", fmt_duration(*duration_ms)));
            if let Some(code) = exit_code {
                title.push_str(&format!(" · exit {code}"));
            }
        }
        let body = card.output.clone().unwrap_or_default();
        Self {
            title,
            command: card.command.clone(),
            body_truncated: card.output_truncated,
            body,
            scroll: 0,
            tool_id: Some(card.id.clone()),
            kind: OverlayKind::Tool,
        }
    }

    /// reasoning 原文 overlay。
    pub fn for_reasoning(text: &str) -> Self {
        Self {
            title: "思考（reasoning）".into(),
            command: None,
            body: bound_message(text),
            body_truncated: false,
            scroll: 0,
            tool_id: None,
            kind: OverlayKind::Reasoning,
        }
    }

    /// 链接 overlay（§成熟化：点击链接文本打开；确认打开/复制由 app 执行 effect）。
    pub fn for_link(url: &str) -> Self {
        Self {
            title: "链接".into(),
            command: None,
            body: url.to_string(),
            body_truncated: false,
            scroll: 0,
            tool_id: None,
            kind: OverlayKind::Link,
        }
    }
}

/// 转录条目：消息或工具卡片（保持原始出现顺序）。
///
/// 每个条目带稳定 [`EntryId`]（TUI v2 §4.1）：trim/折叠不会改变 id，
/// 滚动锚点与搜索命中都基于它，而不是 Vec index。
///
/// 搜索/选区文本缓存（§成熟化）：转录条目进入后文本不可变（streaming 在
/// live 区完成、finalize 时才创建条目），故缓存只需惰性计算一次；工具卡片
/// 仅在防御性 re-finish 时变更，那里显式失效。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Message {
        id: EntryId,
        line: TranscriptLine,
        /// 搜索用小写 haystack 缓存（惰性；§成熟化避免每键全量 to_lowercase）。
        search_cache: Option<String>,
        /// 选区文本用的语义文本缓存（键为 line.version；§成熟化避免每次
        /// 复制都重新跑 markdown 渲染）。
        semantic_cache: Option<(u64, String)>,
    },
    Tool {
        id: EntryId,
        card: ToolCard,
        /// 搜索用小写 haystack 缓存（惰性；re-finish 时失效）。
        search_cache: Option<String>,
    },
}

impl Entry {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::Message { id, .. } | Entry::Tool { id, .. } => *id,
        }
    }

    /// 搜索 haystack（小写缓存；message 不可变，tool 由调用方失效）。
    pub fn search_lower(&mut self) -> &str {
        let cache = match self {
            Entry::Message {
                line, search_cache, ..
            } => {
                if search_cache.is_none() {
                    *search_cache = Some(line.text.to_lowercase());
                }
                search_cache
            }
            Entry::Tool {
                card, search_cache, ..
            } => {
                if search_cache.is_none() {
                    *search_cache = Some(search_tool_haystack(card).to_lowercase());
                }
                search_cache
            }
        };
        cache.as_deref().unwrap_or_default()
    }

    /// 工具卡片字段变更后失效搜索缓存（防御性 re-finish 路径）。
    pub fn invalidate_search_cache(&mut self) {
        if let Entry::Tool { search_cache, .. } = self {
            *search_cache = None;
        }
    }
}

/// 工具卡片的搜索文本（name/target/tail；小字段，随卡片构造一次）。
fn search_tool_haystack(card: &ToolCard) -> String {
    let mut out = card.name.clone();
    if let Some(target) = &card.target {
        out.push(' ');
        out.push_str(target);
    }
    if let Some(tail) = &card.tail {
        out.push('\n');
        out.push_str(tail);
    }
    out
}

/// 状态栏内容（§16.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusLine {
    Idle,
    Running { turn: u32, tool: String },
    Compacting,
}

/// 斜杠命令补全菜单（输入以 `/` 开头时弹出，§16.2 信息层级之外的小浮层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuView {
    /// 与输入前缀匹配的项（label, 中文说明）。
    pub items: Vec<(String, String)>,
    pub selected: usize,
    pub kind: MenuKind,
}

/// 菜单种类（决定 Enter/Tab 的插入行为）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// `/命令` 补全：Enter/Tab 插入 `/name`。
    SlashCommand,
    /// `@文件` 引用：Enter 把选中路径替换到光标前的 `@` token。
    File,
    /// `/sessions` 会话列表：Enter 恢复选中 session。
    Session,
}

/// 流式消息（TUI v2 §7.2：live 区，finalize 前不进 transcript）。
#[derive(Debug, Clone, Default)]
pub struct StreamingMessage {
    /// 稳定 EntryId（§PointerHit：streaming 期间可选中，finalize 后沿用同一 id，
    /// 选区不因 finalize 悬空）。
    pub entry_id: EntryId,
    pub text: String,
    /// 文本变化时递增（渲染缓存失效；与 TranscriptLine.version 同义）。
    pub version: u64,
    /// 超出单条消息上限（MAX_MESSAGE_CHARS）标记。
    pub truncated: bool,
}

/// 运行中的工具卡片 + 稳定 EntryId（§PointerHit ⑤：每个 live 对象独立 id，
/// finish 沿用——finalize 后选区不悬空，tool 输出可选择/复制）。
#[derive(Debug, Clone)]
pub struct LiveTool {
    pub entry_id: EntryId,
    pub card: ToolCard,
}

/// 正在变化的 turn 状态（TUI v2 §7.2）：与 finalized transcript 分离。
///
/// 分离收益（§59）：历史 finalized entry 不会因当前 tool output /
/// streaming 重排；滚动锚点（EntryId + row）只针对 finalized 内容。
/// finalize 时机：工具开始（上一段流式文本结束）、工具完成（卡片终态）、
/// run 结束（剩余内容全部提交）。
#[derive(Debug, Clone, Default)]
pub struct LiveTurnState {
    /// 流式 assistant 消息（未完成；begin_tool 时 finalize）。
    pub assistant: Option<StreamingMessage>,
    /// 流式 reasoning 消息（未完成；begin_tool / run 结束时 finalize）。
    pub reasoning: Option<StreamingMessage>,
    /// 运行中的工具卡片（按 call_id；§PointerHit：每个带稳定 EntryId）。
    pub tools: HashMap<String, LiveTool>,
    /// 工具启动顺序（finalize/渲染保持视觉顺序）。
    pub tool_order: Vec<String>,
}

/// 只读渲染输入（§16.1：renderer 的唯一输入）。
#[derive(Debug, Clone)]
pub struct ViewModel {
    pub transcript: Vec<Entry>,
    pub input: String,
    pub input_cursor: usize,
    /// 过渡提示（footer 显示；下一次键盘/鼠标操作清除）。
    pub transient_hint: Option<String>,
    pub plan: Option<Plan>,
    pub status: StatusLine,
    pub model_name: String,
    /// workspace 目录名（footer 展示）。
    pub workspace: String,
    /// 当前正在运行的 turn 数。
    pub turn: u32,
    /// 正在变化的 turn（流式文本 + 运行中工具；§7.2 与 transcript 分离）。
    pub live: LiveTurnState,
    /// 滚动模式（TUI v2 §3）：Follow = 尾部；Locked = 锚定 EntryId + row。
    pub scroll_mode: ScrollMode,
    /// scroll lock 期间到达的新条目数（footer 提示；End/Ctrl+End 清空）。
    pub pending_below: u64,
    /// 最近一次布局的视口顶部行（renderer 写回；滚动操作的基础，§4）。
    pub layout_top: Option<(EntryId, usize)>,
    /// 各 entry 最近一次布局的 visual 高度（renderer 写回；滚动跨 entry 用）。
    pub entry_heights: HashMap<EntryId, usize>,
    /// 最近一次布局的转录区高度（PageUp/PageDown 按 viewport-2 移动，§10）。
    pub transcript_rows: u16,
    /// 排队中的待提交消息数（footer 提示；由 UiState 同步）。
    pub pending_queue_len: usize,
    /// 思考内容是否展开（Alt+T 全局切换；§用户诉求：点击单个 thinking 行按条目展开）。
    pub reasoning_visible: bool,
    /// 按条目展开的 thinking（§用户诉求：像 diff 一样点击行展开/收缩）。
    /// 该 entry 的 thinking 折叠态显示一行，展开态显示全文。
    pub reasoning_expanded: std::collections::HashSet<EntryId>,
    /// 鼠标点击命中的目标（工具卡片/reasoning 行；§24 高亮反馈）。
    /// Overlay 打开期间该行高亮；关闭 Overlay 后清除。
    pub active_hit: Option<crate::tui::HitTarget>,
    /// 应用内选择复制（语义位置：entry + 逻辑文本偏移，不依赖屏幕坐标；
    /// resize/rewrap/滚动不改变选中内容）。`Some` = 正在选择或已有选区。
    pub selection: Option<crate::tui::interaction::TextSelection>,
    /// 本会话累计 token 用量（AgentOutcome.usage 累积，§16.2：无 pricing 时显示 usage）。
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 缓存命中的输入 token（§16.2：`⇄` 展示）。
    pub cache_read_tokens: u64,
    /// 本会话累计花费（美元；§16.2：config 配置单价后显示）。
    pub cost_usd: f64,
    /// 输入/输出单价（每百万 token，美元；None = 不显示花费）。
    pub price_input: Option<f64>,
    pub price_output: Option<f64>,
    /// 动画帧计数（spinner；由 app 的 ticker 推进，§16.1：动画时钟独立）。
    pub anim_tick: u64,
    /// 命令补全菜单（None = 关闭）。
    pub menu: Option<MenuView>,
    /// 上下文占用投影（projected, usable；footer 用量条）。
    pub context_usage: Option<(u64, u64)>,
    /// workspace 文件索引（`@` 引用补全用；会话开始时扫描一次，有界）。
    pub file_index: Vec<String>,
    /// 详情 Overlay（None = 关闭；打开时 Esc 关闭、PgUp/PgDn 滚动）。
    pub overlay: Option<OverlayState>,
    /// 操作型 Modal（None = 关闭；§42：/help /settings /doctor 等不污染 transcript）。
    pub modal: Option<ModalState>,
    /// 转录搜索（None = 关闭；§14 Ctrl+F）。
    pub search: Option<SearchState>,
    pub next_version: u64,
    pub next_entry_id: u64,
}

impl Default for ViewModel {
    fn default() -> Self {
        Self {
            transcript: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            transient_hint: None,
            plan: None,
            status: StatusLine::Idle,
            model_name: "?".into(),
            workspace: String::new(),
            turn: 0,
            live: LiveTurnState::default(),
            scroll_mode: ScrollMode::Follow,
            pending_below: 0,
            layout_top: None,
            entry_heights: HashMap::new(),
            transcript_rows: 0,
            pending_queue_len: 0,
            reasoning_visible: false,
            reasoning_expanded: std::collections::HashSet::new(),
            active_hit: None,
            selection: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            price_input: None,
            price_output: None,
            anim_tick: 0,
            menu: None,
            context_usage: None,
            file_index: Vec::new(),
            overlay: None,
            modal: None,
            search: None,
            next_version: 1,
            next_entry_id: 1,
        }
    }
}

impl ViewModel {
    /// 输入行与光标位置（编辑器渲染用）。
    pub fn input_position(&self) -> (String, usize) {
        (self.input.clone(), self.input_cursor.min(self.input.len()))
    }

    fn alloc_version(&mut self) -> u64 {
        let v = self.next_version;
        self.next_version += 1;
        v
    }

    /// 分配稳定条目 ID（§4.1：trim/折叠不影响）。
    fn alloc_entry_id(&mut self) -> EntryId {
        let id = EntryId(self.next_entry_id);
        self.next_entry_id += 1;
        id
    }

    /// BUG-006：/new 后清空全部会话投影（transcript/live/plan/usage/滚动/浮层），
    /// 保证屏幕不再显示旧 session 内容（模型上下文已清空，屏幕必须同步）。
    /// 保留 next_entry_id / next_version（单调递增即可，不需要复用）。
    pub fn reset_for_new_session(&mut self) {
        self.transcript.clear();
        self.live = LiveTurnState::default();
        self.plan = None;
        self.status = StatusLine::Idle;
        self.turn = 0;
        self.reasoning_expanded.clear();
        self.scroll_mode = ScrollMode::Follow;
        self.pending_below = 0;
        self.layout_top = None;
        self.entry_heights.clear();
        self.transcript_rows = 0;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cache_read_tokens = 0;
        self.cost_usd = 0.0;
        self.context_usage = None;
        self.menu = None;
        self.overlay = None;
        self.modal = None;
        self.search = None;
        self.active_hit = None;
        self.selection = None;
    }

    /// BUG-006：会话切换/恢复后把模型上下文（history）重建到屏幕，
    /// 避免“屏幕是 session A、模型在 session B”的状态不一致。
    /// User/Assistant 按原文重建；工具结果以单行摘要呈现（不伪造卡片）。
    pub fn load_history(&mut self, history: &[crate::provider::ChatMessage]) {
        self.reset_for_new_session();
        for message in history {
            match message {
                crate::provider::ChatMessage::User(text) => {
                    self.push_line(LineKind::User, text.clone());
                }
                crate::provider::ChatMessage::Assistant { content, .. } => {
                    if !content.is_empty() {
                        self.push_line(LineKind::Assistant, content.clone());
                    }
                }
                crate::provider::ChatMessage::Tool { name, content, .. } => {
                    let first = content.lines().next().unwrap_or_default();
                    self.push_line(LineKind::Tool, format!("{name}: {first}"));
                }
                crate::provider::ChatMessage::System(text) => {
                    self.push_line(LineKind::System, text.clone());
                }
            }
        }
    }
    /// 追加转录行。
    pub fn push_line(&mut self, kind: LineKind, text: impl Into<String>) {
        let version = self.alloc_version();
        let id = self.alloc_entry_id();
        self.transcript.push(Entry::Message {
            id,
            line: TranscriptLine {
                kind,
                text: bound_message(&text.into()),
                version,
            },
            search_cache: None,
            semantic_cache: None,
        });
        self.note_new_content();
        self.trim_transcript();
    }

    /// 追加同一条流式消息（TUI v2 §7.2：写 live 区，finalize 前不进 transcript）。
    /// provider 的 token chunk 不是视觉行，不能逐 chunk 创建条目，
    /// 否则会把“你好”渲染成多条带前缀的碎片行。
    pub fn push_stream_delta(&mut self, kind: LineKind, text: &str) {
        let version = self.alloc_version();
        let needs_new = match kind {
            LineKind::Assistant => self.live.assistant.is_none(),
            LineKind::Reasoning => self.live.reasoning.is_none(),
            _ => false,
        };
        if needs_new {
            let entry_id = self.alloc_entry_id();
            let msg = StreamingMessage {
                entry_id,
                text: String::new(),
                version: 0,
                truncated: false,
            };
            match kind {
                LineKind::Assistant => self.live.assistant = Some(msg),
                LineKind::Reasoning => self.live.reasoning = Some(msg),
                _ => {}
            }
        }
        let slot = match kind {
            LineKind::Assistant => &mut self.live.assistant,
            LineKind::Reasoning => &mut self.live.reasoning,
            _ => {
                // User/System/Tool 不走流式（防御）。
                self.push_line(kind, text);
                return;
            }
        };
        let Some(msg) = slot.as_mut() else {
            // Defensive only: the matching branch above initializes this slot.
            debug_assert!(false, "streaming slot must be initialized");
            return;
        };
        // P1-9：单条消息有界（超出丢弃中段并标记，防膨胀）。
        if !msg.truncated && msg.text.len() < MAX_MESSAGE_CHARS {
            let room = MAX_MESSAGE_CHARS - msg.text.len();
            if text.len() <= room {
                msg.text.push_str(text);
            } else {
                let marker = "…[truncated]";
                let content_budget = MAX_MESSAGE_CHARS.saturating_sub(marker.len());
                if msg.text.len() > content_budget {
                    let keep = crate::tui::text::floor_char_boundary(&msg.text, content_budget);
                    msg.text.truncate(keep);
                }
                let content_room = content_budget.saturating_sub(msg.text.len());
                let keep =
                    crate::tui::text::floor_char_boundary(text, content_room.min(text.len()));
                msg.text.push_str(&text[..keep]);
                msg.text.push_str(marker);
                msg.truncated = true;
            }
        } else if !msg.truncated {
            let marker = "…[truncated]";
            let content_budget = MAX_MESSAGE_CHARS.saturating_sub(marker.len());
            let keep = crate::tui::text::floor_char_boundary(&msg.text, content_budget);
            msg.text.truncate(keep);
            msg.text.push_str(marker);
            msg.truncated = true;
        }
        msg.version = version; // 文本变化 → 渲染缓存失效
        // 整改 C + TUI v2 §16：Locked 时流式新内容不移动视口（计数
        // 语义：新 entry 才计数；流式合并不增加条目，保持视觉连续）。
    }

    /// 把 live 区的流式消息提交为 transcript 条目（§7.2 finalize）。
    /// 工具开始（上一段文本结束）与 run 结束时调用。
    pub fn finalize_streaming(&mut self) {
        for kind in [LineKind::Reasoning, LineKind::Assistant] {
            // 循环只产生上述两种；其余类型不可能出现——日志上报并跳过。
            let slot = match kind {
                LineKind::Reasoning => &mut self.live.reasoning,
                LineKind::Assistant => &mut self.live.assistant,
                other => {
                    tracing::error!(kind = ?other, "finalize_streaming: 意外消息类型（内部不变量破坏）");
                    continue;
                }
            };
            if let Some(msg) = slot.take() {
                if msg.text.is_empty() {
                    continue;
                }
                let version = self.alloc_version();
                // §PointerHit：沿用 streaming 期间分配的稳定 id，选区不悬空。
                let id = msg.entry_id;
                self.transcript.push(Entry::Message {
                    id,
                    line: TranscriptLine {
                        kind,
                        text: msg.text,
                        version,
                    },
                    search_cache: None,
                    semantic_cache: None,
                });
                self.note_new_content();
            }
        }
        self.trim_transcript();
    }

    /// run 结束：提交全部剩余 live 内容（流式消息 + 未完成工具卡片）。
    pub fn finalize_live(&mut self) {
        self.finalize_streaming();
        // 剩余运行中工具（run 被取消/异常结束时）也提交，保持历史可见。
        // §PointerHit ⑤：沿用 begin_tool 分配的稳定 id，选区不悬空。
        let order = std::mem::take(&mut self.live.tool_order);
        for call_id in order {
            if let Some(tool) = self.live.tools.remove(&call_id) {
                let mut card = tool.card;
                if matches!(card.state, ToolCardState::Running) {
                    card.state = ToolCardState::Done {
                        status: ToolStatus::Interrupted,
                        duration_ms: 0,
                        exit_code: None,
                    };
                }
                self.transcript.push(Entry::Tool {
                    id: tool.entry_id,
                    card,
                    search_cache: None,
                });
                self.note_new_content();
            }
        }
        self.trim_transcript();
    }

    /// 丢弃当前 attempt 的流式内容（§4.3 第三阶段：partial tool-call restart）。
    /// 整个 model turn 重新生成，已显示的 partial 不进 transcript；
    /// partial 文本由 session 的 `AssistantAttemptInterrupted` 记录（durable 事实）。
    /// 若有运行中的工具卡片则一并提交（防止中间状态丢失）。
    pub fn discard_live_turn(&mut self) {
        self.live.assistant = None;
        self.live.reasoning = None;
        let order = std::mem::take(&mut self.live.tool_order);
        for call_id in order {
            if let Some(tool) = self.live.tools.remove(&call_id) {
                self.transcript.push(Entry::Tool {
                    id: tool.entry_id,
                    card: tool.card,
                    search_cache: None,
                });
                self.note_new_content();
            }
        }
        self.trim_transcript();
    }

    /// 整改 C + TUI v2 §16：Locked 期间的新条目计数（footer 提示）。
    fn note_new_content(&mut self) {
        if self.scroll_mode != ScrollMode::Follow {
            self.pending_below = self.pending_below.saturating_add(1);
        }
    }

    /// §24：跳转到转录的绝对比例位置（scrollbar 点击/拖拽）。
    /// ratio ∈ [0,1] 对应视口顶部在内容中的位置（0 = 顶部，1 = 底部）。
    pub fn scroll_to_ratio(&mut self, ratio: f64) {
        let mut ids: Vec<EntryId> = self.transcript.iter().map(Entry::id).collect();
        // §PointerHit ⑤：live 区各对象独立 id（与 build_live_group 的组一致）。
        if let Some(msg) = &self.live.reasoning {
            ids.push(msg.entry_id);
        }
        if let Some(msg) = &self.live.assistant {
            ids.push(msg.entry_id);
        }
        for call_id in &self.live.tool_order {
            if let Some(tool) = self.live.tools.get(call_id) {
                ids.push(tool.entry_id);
            }
        }
        let heights = self.current_heights(&ids);
        let total: usize = heights.iter().sum();
        let area = self.transcript_rows as usize;
        if total <= area || area == 0 {
            self.follow_tail();
            return;
        }
        let max_start = total - area;
        let ratio = ratio.clamp(0.0, 1.0);
        let target = (ratio * max_start as f64).round() as usize;
        let (entry, row) = crate::tui::scroll::locate_row(&ids, &heights, target);
        self.lock_to(entry, row);
    }

    /// §25：Ctrl+Home 跳到历史最顶部（第一条 entry）。
    pub fn jump_to_top(&mut self) {
        if let Some(first) = self.transcript.iter().map(Entry::id).next() {
            self.lock_to(first, 0);
        }
    }

    /// 恢复 follow-tail（End/Ctrl+End，§3.1）：回到底部并清空新消息计数。
    pub fn follow_tail(&mut self) {
        self.scroll_mode = ScrollMode::Follow;
        self.pending_below = 0;
    }

    /// 打开搜索（§14）；Esc 关闭时不强制跳回底部。
    pub fn open_search(&mut self) {
        self.search = Some(SearchState::open());
    }

    /// 更新搜索词并重新计算命中；命中时锁定到第一个命中。
    pub fn update_search_query(&mut self, query: &str) {
        let Some(search) = &mut self.search else {
            return;
        };
        search.query = crate::tui::text::truncate_middle_utf8(query, MAX_SEARCH_QUERY, "…");
        search.recompute(&mut self.transcript);
        if let Some(first) = search.hits.first().copied() {
            self.lock_to(first, 0);
        }
    }

    /// 跳到下一个/上一个命中（循环；§14 Enter/F3 与 Shift+Enter）。
    pub fn search_jump(&mut self, forward: bool) {
        let Some(search) = &mut self.search else {
            return;
        };
        if search.hits.is_empty() {
            return;
        }
        if forward {
            search.index = (search.index + 1) % search.hits.len();
        } else {
            search.index = (search.index + search.hits.len() - 1) % search.hits.len();
        }
        let id = search.hits[search.index];
        self.lock_to(id, 0);
    }

    /// 跳到上一个/下一个 User turn（§13：基于 EntryId 查找，不是视觉行）。
    /// 返回是否找到（无 User 消息时 false）。
    pub fn jump_to_user_turn(&mut self, forward: bool) -> bool {
        let user_ids: Vec<EntryId> = self
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message { id, line, .. } if line.kind == LineKind::User => Some(*id),
                _ => None,
            })
            .collect();
        if user_ids.is_empty() {
            return false;
        }
        // 基准：Locked 锚点 entry > 视口顶部 entry > 末尾（不依赖布局刷新）。
        let base = match self.scroll_mode {
            ScrollMode::Locked(anchor) => Some(anchor.entry_id),
            ScrollMode::Follow => self.layout_top.map(|(e, _)| e),
        }
        .or_else(|| self.transcript.last().map(Entry::id))
        .unwrap_or(EntryId(0));
        let target = if forward {
            // 第一个位于基准之后的 User；无 → 循环到第一个。
            user_ids
                .iter()
                .find(|id| **id > base)
                .copied()
                .or_else(|| user_ids.first().copied())
        } else {
            // 最后一个位于基准之前的 User；无 → 循环到最后一个。
            user_ids
                .iter()
                .rev()
                .find(|id| **id < base)
                .copied()
                .or_else(|| user_ids.last().copied())
        };
        let Some(target) = target else {
            return false;
        };
        self.lock_to(target, 0);
        true
    }

    /// 进入 Locked 并锚定到指定行（§3.2：滚动/搜索跳转后新输出不动视口）。
    pub fn lock_to(&mut self, entry: EntryId, row_in_entry: usize) {
        self.scroll_mode = ScrollMode::Locked(ScrollAnchor {
            entry_id: entry,
            row_in_entry,
        });
    }

    /// 向历史滚动（§10：每步 delta 行）。
    /// 基准：Locked 时 = 当前锚点（连续滚动自然累积，不依赖布局刷新）；
    /// Follow 时 = 最近布局的视口顶部行（§3.2 进入 Locked）。
    pub fn scroll_up(&mut self, lines: u16) {
        let delta = lines.max(1) as usize;
        let base = self.scroll_base();
        if let Some((entry, row)) = base {
            let ids: Vec<EntryId> = self.transcript.iter().map(Entry::id).collect();
            let heights = self.current_heights(&ids);
            let top_row = crate::tui::scroll::row_of(&ids, &heights, entry, row);
            let new_top =
                crate::tui::scroll::move_by_rows(&ids, &heights, top_row, -(delta as isize));
            self.lock_to(new_top.0, new_top.1);
        }
    }

    pub fn scroll_down(&mut self, lines: u16) {
        let delta = lines.max(1) as usize;
        if self.scroll_mode == ScrollMode::Follow {
            return; // 已是最新，无可下滚。
        }
        if let Some((entry, row)) = self.scroll_base() {
            let ids: Vec<EntryId> = self.transcript.iter().map(Entry::id).collect();
            let heights = self.current_heights(&ids);
            let top_row = crate::tui::scroll::row_of(&ids, &heights, entry, row);
            let total: usize = heights.iter().sum();
            let area = self.transcript_rows.max(1) as usize;
            let new_top = crate::tui::scroll::move_by_rows(&ids, &heights, top_row, delta as isize);
            let new_row = crate::tui::scroll::row_of(&ids, &heights, new_top.0, new_top.1);
            if new_row.saturating_add(area) >= total {
                // 滚到最底部：回到 Follow（新内容自动跟随）。
                // 此前保持 Locked 会让用户滚到底后新内容不再自动跟上，
                // 反复滚动无视觉变化，表现为"滚动卡住"（§10 体验修复）。
                self.follow_tail();
                return;
            }
            self.lock_to(new_top.0, new_top.1);
        }
    }

    /// 滚动基准行：Locked = 当前锚点；Follow = 最近布局的视口顶部行
    /// （无布局信息时锚定**内容末尾**——从 Follow 底部上翻符合直觉，
    /// 而不是跳到第一条）。
    fn scroll_base(&self) -> Option<(EntryId, usize)> {
        match self.scroll_mode {
            ScrollMode::Locked(anchor) => Some((anchor.entry_id, anchor.row_in_entry)),
            ScrollMode::Follow => self.layout_top.or_else(|| {
                // 未布局：锚定末尾（Follow 视口底部 = 内容最后一行）。
                let last = self.transcript.last()?;
                let height = self.entry_heights.get(&last.id()).copied().unwrap_or(1);
                Some((last.id(), height.saturating_sub(1)))
            }),
        }
    }

    /// 平行高度表（entry_heights 缓存，缺失的 entry 按 1 行估算）。
    fn current_heights(&self, ids: &[EntryId]) -> Vec<usize> {
        ids.iter()
            .map(|id| self.entry_heights.get(id).copied().unwrap_or(1))
            .collect()
    }

    /// 工具开始（TUI v2 §7.2：进 live 区；完成时 finalize 进 transcript）。
    /// 工具开始 = 上一段流式文本结束 → 先 finalize streaming。
    pub fn begin_tool(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        target: Option<String>,
        command: Option<String>,
    ) {
        self.finalize_streaming();
        let call_id = id.into();
        if self.live.tools.contains_key(&call_id) {
            tracing::error!(%call_id, "duplicate live tool call id");
            return;
        }
        let entry_id = self.alloc_entry_id();
        let name = crate::tui::text::truncate_middle_utf8(&name.into(), MAX_TOOL_NAME, "…");
        let target = target
            .map(|value| crate::tui::text::truncate_middle_utf8(&value, MAX_TOOL_TARGET, "…"));
        let command = command
            .map(|value| crate::tui::text::truncate_middle_utf8(&value, MAX_CARD_COMMAND, "…"));
        self.live.tools.insert(
            call_id.clone(),
            LiveTool {
                entry_id,
                card: ToolCard {
                    id: call_id.clone(),
                    name,
                    target,
                    command,
                    state: ToolCardState::Running,
                    output: None,
                    diff: None,
                    output_truncated: false,
                    expanded: false,
                    tail: None,
                },
            },
        );
        self.live.tool_order.push(call_id);
    }

    /// 工具执行中的实时输出增量（有界累积；live 区运行中卡片可见）。
    pub fn append_tool_output(&mut self, id: impl Into<String>, text: impl Into<String>) {
        let id = id.into();
        let text = text.into();
        let Some(tool) = self.live.tools.get_mut(&id) else {
            // 兜底：live 区找不到（理论不会：ToolStarted 先于增量）。
            return;
        };
        let card = &mut tool.card;
        let current = card.output.get_or_insert_with(String::new);
        if current.len().saturating_add(text.len()) > MAX_CARD_OUTPUT {
            card.output_truncated = true;
            // 丢弃旧内容超出部分（保留尾部：错误相关输出通常在末尾）。
            // §24：drain 起点必须落在字符边界，否则中文/emoji 输出会 panic。
            let overflow = current
                .len()
                .saturating_add(text.len())
                .saturating_sub(MAX_CARD_OUTPUT);
            let drop = crate::tui::text::floor_char_boundary(current, overflow.min(current.len()));
            current.drain(..drop);
            // 单块超大时只保留其尾部（起点同样按字符边界对齐）。
            let remaining = MAX_CARD_OUTPUT.saturating_sub(current.len());
            let tail = crate::tui::text::suffix_by_bytes_safe(&text, remaining);
            current.push_str(tail);
        } else {
            current.push_str(&text);
        }
    }

    /// 切换某张卡片展开/折叠（显示完整输出正文）。
    pub fn toggle_expand(&mut self, id: impl Into<String>) {
        let id = id.into();
        // §PointerHit：运行中的工具在 live.tools，先查它。
        if let Some(tool) = self.live.tools.get_mut(&id) {
            tool.card.expanded = !tool.card.expanded;
            return;
        }
        for entry in self.transcript.iter_mut().rev() {
            if let Entry::Tool { card, .. } = entry
                && card.id == id
            {
                card.expanded = !card.expanded;
                return;
            }
        }
    }

    /// 切换最后一张卡片展开（Alt+E 键盘兜底）。
    pub fn toggle_last_tool_expanded(&mut self) {
        for entry in self.transcript.iter_mut().rev() {
            if let Entry::Tool { card, .. } = entry {
                card.expanded = !card.expanded;
                return;
            }
        }
    }

    /// 打开最近一张工具卡片的详情 Overlay（Alt+E）。
    pub fn open_last_tool_overlay(&mut self) {
        // §PointerHit：运行中的工具优先（最新）。
        if let Some(tool) = self.live.tool_order.last()
            && let Some(tool) = self.live.tools.get(tool)
        {
            self.overlay = Some(OverlayState::for_tool(&tool.card));
            return;
        }
        for entry in self.transcript.iter().rev() {
            if let Entry::Tool { card, .. } = entry {
                self.overlay = Some(OverlayState::for_tool(card));
                return;
            }
        }
    }

    /// P2：打开最近一张失败/被拒/取消的工具卡片 Overlay（Alt+O）。
    pub fn open_failed_tool_overlay(&mut self) {
        // §PointerHit：运行中不算失败；查 transcript。
        for entry in self.transcript.iter().rev() {
            if let Entry::Tool { card, .. } = entry {
                let failed = matches!(
                    &card.state,
                    ToolCardState::Done { status, .. } if *status != ToolStatus::Succeeded
                );
                if failed {
                    self.overlay = Some(OverlayState::for_tool(card));
                    return;
                }
            }
        }
    }

    /// P2：在工具卡片之间切换详情 Overlay（Alt+[ / Alt+]）。
    pub fn cycle_tool_overlay(&mut self, direction: i32) {
        let ids: Vec<String> = self
            .transcript
            .iter()
            .filter_map(|entry| match entry {
                Entry::Tool { card, .. } => Some(card.id.clone()),
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        let current = self.overlay.as_ref().and_then(|o| o.tool_id.clone());
        let pos = current
            .as_ref()
            .and_then(|id| ids.iter().position(|x| x == id))
            .unwrap_or(0);
        let next = (pos as i32 + direction).rem_euclid(ids.len() as i32) as usize;
        self.open_tool_overlay(ids[next].clone());
    }

    /// 按 id 打开工具卡片详情 Overlay（鼠标点击）。
    /// §PointerHit：运行中的工具在 live.tools，优先查它。
    pub fn open_tool_overlay(&mut self, id: impl Into<String>) {
        let id = id.into();
        if let Some(tool) = self.live.tools.get(&id) {
            self.overlay = Some(OverlayState::for_tool(&tool.card));
            self.active_hit = Some(crate::tui::HitTarget::Tool(id));
            return;
        }
        for entry in self.transcript.iter().rev() {
            if let Entry::Tool { card, .. } = entry
                && card.id == id
            {
                self.overlay = Some(OverlayState::for_tool(card));
                // §24：点击后高亮命中行（Overlay 打开期间）。
                self.active_hit = Some(crate::tui::HitTarget::Tool(id));
                return;
            }
        }
    }

    /// §用户诉求：thinking 按条目展开/收缩（点击该行，像 diff 一样）。
    pub fn toggle_reasoning_expanded(&mut self, id: EntryId) {
        if self.reasoning_expanded.contains(&id) {
            self.reasoning_expanded.remove(&id);
        } else {
            self.reasoning_expanded.insert(id);
        }
    }

    /// 该 entry 的 thinking 是否展开（按条目展开优先；否则跟随全局 Alt+T）。
    pub fn is_reasoning_expanded(&self, id: EntryId) -> bool {
        if self.reasoning_expanded.contains(&id) {
            return true;
        }
        self.reasoning_visible
    }

    /// 打开 reasoning 原文 Overlay（点击折叠的 reasoning 行）。
    pub fn open_reasoning_overlay(&mut self, id: EntryId) {
        let Some(line) = self.transcript.iter().find_map(|entry| match entry {
            Entry::Message { id: eid, line, .. } if *eid == id => Some(line),
            _ => None,
        }) else {
            return;
        };
        if line.kind != LineKind::Reasoning {
            return;
        }
        self.overlay = Some(OverlayState::for_reasoning(&line.text));
        // §24：点击后高亮命中行。
        self.active_hit = Some(crate::tui::HitTarget::Reasoning(id));
    }

    /// 打开操作型 Modal（§42）。
    pub fn open_modal(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.modal = Some(ModalState::new(title, body));
    }

    /// 关闭 Overlay（Esc）。
    pub fn close_overlay(&mut self) {
        self.overlay = None;
        self.active_hit = None;
    }

    /// 开始应用内选择（语义位置：entry + 逻辑文本偏移）。
    pub fn selection_start(&mut self, position: TextPosition) {
        self.selection = Some(crate::tui::interaction::TextSelection {
            anchor: position,
            focus: position,
        });
    }

    /// 拖动更新选择范围（更新 focus；anchor 保持按下点，支持反向拖动）。
    pub fn selection_update(&mut self, position: TextPosition) {
        if let Some(sel) = &mut self.selection {
            sel.focus = position;
        }
    }

    /// 释放鼠标：结束拖动（选区保留，等待复制/清除）。
    pub fn selection_end(&mut self) {}

    /// 清除选区（点击空白处 / 开始新选择时先清旧选区）。
    pub fn selection_clear(&mut self) {
        self.selection = None;
    }

    /// 提取选中文本（§PointerHit：copy 从语义文本提取，不依赖当前 viewport
    /// 快照）。选区指向 (entry, offset)，这里从 transcript 的 entry 内容重建
    /// 语义文本——resize/滚动后仍精确。streaming 期间（live 区）同样可提取，
    /// finalize 后沿用同一稳定 id（§⑥）。
    ///
    /// §成熟化：每条目的语义文本按 (kind, text, version) 缓存——entry 进入
    /// transcript 后文本不可变，复制不再每次重新跑 markdown 渲染。
    pub fn selected_text(&mut self) -> String {
        let Some(selection) = self.selection else {
            return String::new();
        };
        let (lo, hi) = selection.normalized();
        // 收集候选 (entry_id, text)：transcript + live 流式消息。
        let mut candidates: Vec<(EntryId, String)> = Vec::new();
        for entry in &mut self.transcript {
            let entry_id = entry.id();
            let text = match entry {
                Entry::Message {
                    line,
                    semantic_cache,
                    ..
                } => {
                    // ③ Canonical Semantic Text：与 renderer hit 坐标系一致——
                    // 用渲染后纯文本（markdown 去样式），而非原始 markdown。
                    // 缓存键 = line.version（文本变化 → 缓存失效）。
                    if let Some((version, cached)) = semantic_cache {
                        if *version == line.version {
                            cached.clone()
                        } else {
                            let fresh = canonical_semantic_text(line.kind, &line.text);
                            *semantic_cache = Some((line.version, fresh.clone()));
                            fresh
                        }
                    } else {
                        let fresh = canonical_semantic_text(line.kind, &line.text);
                        *semantic_cache = Some((line.version, fresh.clone()));
                        fresh
                    }
                }
                Entry::Tool { card, .. } => card_semantic_text(card),
            };
            candidates.push((entry_id, text));
        }
        if let Some(msg) = &self.live.assistant {
            candidates.push((
                msg.entry_id,
                canonical_semantic_text(LineKind::Assistant, &msg.text),
            ));
        }
        if let Some(msg) = &self.live.reasoning {
            candidates.push((
                msg.entry_id,
                canonical_semantic_text(LineKind::Reasoning, &msg.text),
            ));
        }
        // §PointerHit ⑤：运行中工具卡片也在候选（独立稳定 id）。
        for call_id in &self.live.tool_order {
            if let Some(tool) = self.live.tools.get(call_id) {
                candidates.push((tool.entry_id, card_semantic_text(&tool.card)));
            }
        }
        let mut parts: Vec<String> = Vec::new();
        for (entry_id, text) in candidates {
            if entry_id < lo.entry_id || entry_id > hi.entry_id {
                continue;
            }
            if text.is_empty() {
                continue;
            }
            let total = text.chars().count();
            let (sel_lo, sel_hi) = if entry_id == lo.entry_id && entry_id == hi.entry_id {
                (lo.offset.min(total), hi.offset.min(total))
            } else if entry_id == lo.entry_id {
                (lo.offset.min(total), total)
            } else if entry_id == hi.entry_id {
                (0, hi.offset.min(total))
            } else {
                (0, total)
            };
            if sel_lo >= sel_hi {
                continue;
            }
            let slice: String = text.chars().skip(sel_lo).take(sel_hi - sel_lo).collect();
            parts.push(slice);
        }
        parts.join("\n")
    }

    /// 关闭 Modal（Esc；优先级低于 Overlay，§49）。
    pub fn close_modal(&mut self) {
        self.modal = None;
    }

    /// 工具终态（TUI v2 §7.2）：从 live 区移除并 finalize 为 transcript 卡片
    /// （终态 immutable，历史不再因 live 输出重排）。失败保留关键 tail（有界）。
    pub fn finish_tool(
        &mut self,
        identity: impl Into<ToolIdentity>,
        status: ToolStatus,
        duration_ms: u64,
        exit_code: Option<i32>,
        tail: impl Into<String>,
        diff: Option<String>,
    ) {
        let ToolIdentity { id, name } = identity.into();
        let name = crate::tui::text::truncate_middle_utf8(&name, MAX_TOOL_NAME, "…");
        let tail = tail.into();
        // §用户诉求：TUI 卡片只显示用户可见内容——剥离面向模型的 envelope
        // 元数据头（status/revision/path/lines 等）。入库即净化：渲染与复制
        // 共用同一文本，语义 offset 不漂移；模型上下文仍读完整 model_payload。
        let tail = user_visible_output(&name, &tail);
        if let Some(mut tool) = self.live.tools.remove(&id) {
            tool.card.name = name;
            tool.card.state = ToolCardState::Done {
                status,
                duration_ms,
                exit_code,
            };
            // §用户诉求：edit/write 的 diff 独立保存（默认展开渲染）。
            tool.card.diff = diff.map(|value| bound_diff(&value));
            // 完整输出始终保留（成功也可见，Alt+E/鼠标展开）；失败时折叠态显示 tail。
            if !tail.is_empty() {
                tool.card.output = Some(bound_output(&tail));
                if tail.len() > MAX_CARD_OUTPUT {
                    tool.card.output_truncated = true;
                }
            }
            if status != ToolStatus::Succeeded && !tail.is_empty() {
                tool.card.tail = Some(bound_tail(&tail));
            }
            // §PointerHit ⑤：沿用 begin_tool 分配的稳定 id。
            let entry_id = tool.entry_id;
            self.transcript.push(Entry::Tool {
                id: entry_id,
                card: tool.card,
                search_cache: None,
            });
            self.note_new_content();
            self.trim_transcript();
            return;
        }
        // 未找到 live 卡：可能在 transcript 中（防御性重复 finish）——更新状态。
        for entry in self.transcript.iter_mut().rev() {
            if let Entry::Tool { card, .. } = entry
                && card.id == id
            {
                card.name = name;
                card.state = ToolCardState::Done {
                    status,
                    duration_ms,
                    exit_code,
                };
                card.diff = diff.map(|value| bound_diff(&value));
                if !tail.is_empty() {
                    card.output = Some(bound_output(&tail));
                    if tail.len() > MAX_CARD_OUTPUT {
                        card.output_truncated = true;
                    }
                }
                if status != ToolStatus::Succeeded && !tail.is_empty() {
                    card.tail = Some(bound_tail(&tail));
                }
                // §成熟化：卡片字段变更 → 搜索缓存失效。
                entry.invalidate_search_cache();
                return;
            }
        }
        // 仍未找到（理论上不会：ToolStarted 先于 ToolCompleted）——兜底追加终态卡片。
        let entry_id = self.alloc_entry_id();
        self.transcript.push(Entry::Tool {
            id: entry_id,
            search_cache: None,
            card: ToolCard {
                id,
                name,
                target: None,
                command: None,
                state: ToolCardState::Done {
                    status,
                    duration_ms,
                    exit_code,
                },
                output: if tail.is_empty() {
                    None
                } else {
                    Some(bound_output(&tail))
                },
                diff: diff.map(|value| bound_diff(&value)),
                output_truncated: tail.len() > MAX_CARD_OUTPUT,
                expanded: false,
                tail: if status == ToolStatus::Succeeded {
                    None
                } else {
                    Some(bound_tail(&tail))
                },
            },
        });
        self.note_new_content();
        self.trim_transcript();
    }

    /// 有界转录（防止长会话内存无限增长）。trim 不改变 EntryId（§4.1）；
    /// 锚点 entry 被 trim 后由布局侧回退到最早现存 entry（scroll.rs，§68）。
    fn trim_transcript(&mut self) {
        let mut total_bytes = self.transcript.iter().fold(0usize, |total, entry| {
            total.saturating_add(entry_memory_bytes(entry))
        });
        if self.transcript.len() > 2000 || total_bytes > MAX_TRANSCRIPT_BYTES {
            let mut keep = self.transcript.len().saturating_sub(2000);
            while keep < self.transcript.len() && total_bytes > MAX_TRANSCRIPT_BYTES {
                total_bytes =
                    total_bytes.saturating_sub(entry_memory_bytes(&self.transcript[keep]));
                keep += 1;
            }
            let removed: Vec<EntryId> = self.transcript[..keep].iter().map(Entry::id).collect();
            let removed_set: std::collections::HashSet<EntryId> = removed.iter().copied().collect();
            let removed_tool_ids: std::collections::HashSet<String> = self.transcript[..keep]
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Tool { card, .. } => Some(card.id.clone()),
                    _ => None,
                })
                .collect();
            self.transcript.drain(..keep);
            for id in &removed {
                self.entry_heights.remove(id);
            }
            // §PointerHit：trim 后修复悬空的语义状态（selection / anchor /
            // active_hit / search hits），避免跳顶或复制空。
            if let Some(sel) = &mut self.selection
                && (removed_set.contains(&sel.anchor.entry_id)
                    || removed_set.contains(&sel.focus.entry_id))
            {
                self.selection = None;
            }
            if let ScrollMode::Locked(anchor) = &mut self.scroll_mode
                && removed_set.contains(&anchor.entry_id)
            {
                // 锚点被删：回退到最早现存 entry（而非跳 0）。
                if let Some(first) = self.transcript.first() {
                    anchor.entry_id = first.id();
                    anchor.row_in_entry = 0;
                }
            }
            if self
                .layout_top
                .is_some_and(|(entry_id, _)| removed_set.contains(&entry_id))
            {
                self.layout_top = self.transcript.first().map(|entry| (entry.id(), 0));
            }
            self.reasoning_expanded
                .retain(|id| !removed_set.contains(id));
            if let Some(hit) = &mut self.active_hit {
                let valid = match hit {
                    crate::tui::HitTarget::Reasoning(id) => !removed_set.contains(id),
                    crate::tui::HitTarget::Tool(id) => !removed_tool_ids.contains(id),
                };
                if !valid {
                    self.active_hit = None;
                }
            }
            if let Some(search) = &mut self.search {
                search.hits.retain(|id| !removed_set.contains(id));
                if search.index >= search.hits.len() {
                    search.index = search.hits.len().saturating_sub(1);
                }
            }
        }
    }

    /// 累积一次 run 的 token 用量（§16.2：footer 展示）。
    pub fn add_usage(&mut self, usage: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        // §16.2：配置单价后按输入/输出 token 累计花费（每百万 token 美元）。
        if let Some(pi) = self.price_input {
            self.cost_usd += (usage.input_tokens as f64 / 1_000_000.0) * pi;
        }
        if let Some(po) = self.price_output {
            self.cost_usd += (usage.output_tokens as f64 / 1_000_000.0) * po;
        }
    }

    /// 依据当前输入重建命令补全菜单（保留选中项；无匹配则关闭）。
    pub fn refresh_command_menu(&mut self) {
        let selected = self.menu.as_ref().map(|m| m.selected).unwrap_or(0);
        self.menu = None;
        let Some(rest) = self.input.strip_prefix('/') else {
            return;
        };
        let items: Vec<(String, String)> = crate::tui::SLASH_COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(rest))
            .map(|(name, desc)| (name.to_string(), desc.to_string()))
            .collect();
        if items.is_empty() {
            return;
        }
        self.menu = Some(MenuView {
            selected: selected.min(items.len() - 1),
            items,
            kind: MenuKind::SlashCommand,
        });
    }

    /// 输入中是否存在 `@` 引用 token（refresh_at_menu 的触发条件）。
    pub fn has_at_token(&self) -> bool {
        at_token(&self.input).is_some()
    }

    /// 依据当前输入与文件索引重建 `@` 引用菜单（文件菜单覆盖斜杠菜单）。
    ///
    /// 触发条件：输入中最后一个空格分隔的 token 以 `@` 开头（如 `看看 @src/m`）。
    /// 插入时替换该 token（保留其他输入）。
    pub fn refresh_at_menu(&mut self) {
        let selected = self.menu.as_ref().map(|m| m.selected).unwrap_or(0);
        self.menu = None;
        let Some((_, token)) = at_token(&self.input) else {
            return;
        };
        let needle = token.strip_prefix('@').unwrap_or("");
        let items: Vec<(String, String)> = self
            .file_index
            .iter()
            .filter(|path| path.starts_with(needle))
            .take(20)
            .map(|path| (path.clone(), String::new()))
            .collect();
        if items.is_empty() {
            return;
        }
        self.menu = Some(MenuView {
            selected: selected.min(items.len() - 1),
            items,
            kind: MenuKind::File,
        });
    }

    /// 把输入替换为 `/name` 并重建菜单（Tab 补全 / Enter 选中用）。
    pub fn complete_menu_command(&mut self) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let Some((name, _)) = menu.items.get(menu.selected) else {
            return;
        };
        match menu.kind {
            MenuKind::SlashCommand => {
                self.input = format!("/{name}");
                self.input_cursor = self.input.len();
                self.refresh_command_menu();
            }
            MenuKind::File => {
                // 替换光标前最后一个 `@` token 为选中路径。
                let path = name.clone();
                if let Some((start, _)) = at_token(&self.input) {
                    self.input.replace_range(start.., &path);
                    self.input_cursor = self.input.len();
                }
                self.menu = None;
            }
            MenuKind::Session => {
                // 会话选择由 app 层处理（需要重建 SessionLog/history），这里只关闭菜单。
                self.menu = None;
            }
        }
    }

    /// 返回选中的菜单项（app 层处理 Session 恢复时用）。
    pub fn selected_menu_item(&self) -> Option<(String, MenuKind)> {
        let menu = self.menu.as_ref()?;
        let (label, _) = menu.items.get(menu.selected)?;
        Some((label.clone(), menu.kind))
    }
}

/// 输入中最后一个空格分隔 token 的 `@` 前缀位置（`(start, prefix)`）。
fn at_token(input: &str) -> Option<(usize, &str)> {
    let start = input.rfind([' ', '\n']).map(|i| i + 1).unwrap_or(0);
    let token = &input[start..];
    if let Some(at) = token.find('@')
        && at == 0
    {
        return Some((start, token));
    }
    None
}

/// 失败 tail 有界化（§16.2：保留关键 tail，不自动展开几十屏）。
fn bound_tail(tail: &str) -> String {
    const MAX_CHARS: usize = 240;
    if tail.chars().count() <= MAX_CHARS {
        return tail.to_string();
    }
    // 取末尾 MAX_CHARS 字符并保持原顺序（“…” 前缀标记截断）。
    let mut out = String::from("…");
    let suffix: String = tail.chars().rev().take(MAX_CHARS).collect();
    out.extend(suffix.chars().rev());
    out
}

fn bound_message(text: &str) -> String {
    crate::tui::text::truncate_middle_utf8(text, MAX_MESSAGE_CHARS, "\n…[message truncated]…\n")
}

fn bound_diff(diff: &str) -> String {
    crate::tui::text::truncate_middle_utf8(diff, MAX_CARD_DIFF, "\n…[diff truncated]…\n")
}

fn entry_memory_bytes(entry: &Entry) -> usize {
    match entry {
        Entry::Message { line, .. } => line.text.len(),
        Entry::Tool { card, .. } => card
            .id
            .len()
            .saturating_add(card.name.len())
            .saturating_add(card.target.as_ref().map_or(0, String::len))
            .saturating_add(card.command.as_ref().map_or(0, String::len))
            .saturating_add(card.output.as_ref().map_or(0, String::len))
            .saturating_add(card.diff.as_ref().map_or(0, String::len))
            .saturating_add(card.tail.as_ref().map_or(0, String::len)),
    }
}

/// ③ Canonical Semantic Text：消息的「用户看到的纯文本」（markdown 渲染后
/// 去样式）。renderer 的 hit-test 与 ViewModel::selected_text 必须基于同一份
/// 文本，否则鼠标 offset 与复制内容分叉（一个用 rendered、一个用 raw）。
fn canonical_semantic_text(kind: LineKind, raw: &str) -> String {
    match kind {
        // markdown 渲染的正文：User/Assistant 走同一 renderer。
        LineKind::User | LineKind::Assistant => {
            let rendered = crate::tui::render_markdown(raw, crate::tui::theme::Theme::omp(), None);
            rendered
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        // Reasoning/System/Tool：纯文本，无 markdown 转换。
        _ => raw.to_string(),
    }
}

/// 用户视图净化：把工具结果的模型 envelope 转为用户可见正文
/// （§用户诉求：「给用户看的信息去掉给 AI 看的信息」）。
///
/// 工具返回给模型的 `output` 以 `status:/[revision=]/path:/lines:` 等元数据
/// 头开头，这些面向模型（下一步决策/重试定位），对终端用户是噪音。TUI 卡片
/// 只展示正文与错误诊断；revision/path/lines/status/cursor 等 AI 元数据剥掉。
/// 路径已在卡片主行 target 展示，错误码在卡片 meta（`· exit N`）展示。
///
/// 在 `finish_tool` 入库时调用一次——渲染与复制共用净化后文本，offset 不漂移。
fn user_visible_output(name: &str, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    // 面向模型的元数据行（按前缀识别，保留诊断行如 error:/exit_code:）。
    let is_ai_meta = |l: &&str| {
        l.starts_with("status:")
            || l.starts_with("tool:")
            || l.starts_with("[revision=")
            || l.starts_with("path:")
            || l.starts_with("lines:")
            || l.starts_with("previous_revision:")
            || l.starts_with("current_revision:")
            || l.starts_with("revision:")
            || l.starts_with("cursor:")
    };
    match name {
        // bash/run：`…output: N bytes` 头之后才是实际输出（stdout/stderr 正文）。
        "bash" | "run" => {
            if let Some(idx) = lines.iter().position(|l| l.starts_with("output: ")) {
                let body = lines[idx + 1..]
                    .iter()
                    .skip_while(|l| l.is_empty())
                    .copied()
                    .collect::<Vec<_>>();
                if !body.is_empty() {
                    return body.join("\n");
                }
            }
            // 无标准头（rejected/cancelled/…）→ 剥元数据行，保留错误诊断。
            lines
                .iter()
                .filter(|l| !is_ai_meta(l))
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        }
        // read：`[revision]/path/lines` 头之后是文件内容。
        "read" => {
            if let Some(idx) = lines.iter().position(|l| l.is_empty()) {
                let body = lines[idx + 1..]
                    .iter()
                    .skip_while(|l| l.is_empty())
                    .copied()
                    .collect::<Vec<_>>();
                if !body.is_empty() {
                    return body.join("\n");
                }
            }
            lines
                .iter()
                .filter(|l| !is_ai_meta(l))
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        }
        // 其余（edit/write/search/list/web_search/update_plan/…）：剥元数据行。
        _ => lines
            .iter()
            .filter(|l| !is_ai_meta(l))
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// 工具卡片的语义文本（§PointerHit：copy 从内容提取，不反推渲染结果）。
/// 主行必须与渲染的 `card_semantic_header` **完全一致**（name target meta）——
/// §修复：此前漏了 meta，主行语义比渲染短，selected_text 的 offset 与
/// 渲染 char_start 错位（卡片内选中会选错字符/整卡全选）。
fn card_semantic_text(card: &ToolCard) -> String {
    let mut out = card.name.clone();
    if let Some(target) = &card.target
        && !target.is_empty()
    {
        out.push(' ');
        out.push_str(target);
    }
    // 与 tool_card.rs 的 tool_card_meta 一致（duration · exit code）。
    if let ToolCardState::Done {
        status,
        duration_ms,
        exit_code,
        ..
    } = &card.state
    {
        out.push(' ');
        out.push_str(&fmt_duration(*duration_ms));
        if *status != ToolStatus::Succeeded
            && let Some(code) = exit_code
        {
            out.push_str(&format!(" · exit {code}"));
        }
    }
    let body = card
        .diff
        .as_deref()
        .or(card.output.as_deref())
        .or(card.tail.as_deref())
        .unwrap_or_default();
    if !body.is_empty() {
        out.push('\n');
        out.push_str(body.trim_end());
    }
    out
}

/// 卡片输出有界化（保留尾部；完整输出仍可通过 read @artifact 读取）。
/// §24：尾部起点按字符边界对齐，中文/emoji 输出不 panic。
fn bound_output(output: &str) -> String {
    if output.len() <= MAX_CARD_OUTPUT {
        return output.to_string();
    }
    // “…” 是 3 字节 UTF-8，截断窗口相应减 3，保证总长不超过 MAX_CARD_OUTPUT。
    format!(
        "…{}",
        crate::tui::text::suffix_by_bytes_safe(output, MAX_CARD_OUTPUT - 3)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §用户诉求：TUI 卡片只显示用户可见正文——面向模型的 envelope 元数据头
    /// （status/revision/path/lines/cursor 等）剥掉；错误诊断保留。
    #[test]
    fn user_visible_output_strips_ai_metadata() {
        // bash：`output:` 头之后是实际输出。
        let bash = "status: succeeded\nprogram: bash\nexit_code: 0\nduration_ms: 42\noutput: 11 bytes\n\nhello world";
        assert_eq!(user_visible_output("bash", bash), "hello world");
        // read：`[revision]/path/lines` 头之后是文件内容。
        let read = "[revision=b3:abc]\npath: src/main.rs\nlines: 1-10 of 20\n\nfn main() {}";
        assert_eq!(user_visible_output("read", read), "fn main() {}");
        // edit：剥 revision 元数据，保留 applied 摘要（path 已在卡片 target）。
        let edit = "status: succeeded\ntool: edit\npath: src/a.rs\napplied: replaced 2 of 2\nprevious_revision: b3:aaa\ncurrent_revision: b3:bbb";
        assert_eq!(
            user_visible_output("edit", edit),
            "applied: replaced 2 of 2"
        );
        // search：剥 status，保留扫描统计与命中正文。
        let search = "status: succeeded\nscanned_files: 42\nscanned_bytes: 12345\nelapsed_ms: 3\nstop_reason: complete\nitems: 2 shown of 2\n\na.txt:1: hit";
        let out = user_visible_output("search", search);
        assert!(
            out.contains("scanned_files: 42") && out.contains("hit"),
            "扫描统计与命中保留: {out:?}"
        );
        assert!(!out.contains("status:"), "status 头必须剥掉: {out:?}");
        // 失败诊断保留（error 行不剥）。
        let err = "status: failed\ntool: read\nerror: artifact_not_found";
        let out = user_visible_output("read", err);
        assert!(out.contains("error: artifact_not_found"), "{out:?}");
        assert!(!out.contains("status:"));
        // 无元数据的纯文本原样返回（只归一化末尾换行）。
        assert_eq!(
            user_visible_output("bash", "line-1\nline-2\n"),
            "line-1\nline-2"
        );
    }

    #[test]
    fn streaming_chunks_form_one_logical_message() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "你");
        view.push_stream_delta(LineKind::Assistant, "好");
        assert_eq!(
            view.transcript.len(),
            0,
            "finalize 前不进 transcript（§7.2）"
        );
        let msg = view.live.assistant.as_ref().expect("live 区必须有消息");
        assert_eq!(msg.text, "你好");
        // finalize 后进入 transcript。
        view.finalize_streaming();
        assert_eq!(view.transcript.len(), 1);
        let Entry::Message { line, .. } = &view.transcript[0] else {
            panic!("finalize 后必须是消息条目");
        };
        assert_eq!(line.text, "你好");
    }

    #[test]
    fn stream_delta_bumps_version_for_render_cache() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "a");
        let v1 = view.live.assistant.as_ref().unwrap().version;
        view.push_stream_delta(LineKind::Assistant, "b");
        let v2 = view.live.assistant.as_ref().unwrap().version;
        assert_ne!(v1, v2);
    }

    /// §4.3 第三阶段：partial tool-call restart 时丢弃 live 区 partial
    /// （不进 transcript）——已显示的流式内容被清空，等待整个 turn 重新生成。
    #[test]
    fn discard_live_turn_drops_partial() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Reasoning, "思考中…");
        view.push_stream_delta(LineKind::Assistant, "部分回答");
        assert!(view.live.assistant.is_some() && view.live.reasoning.is_some());
        assert_eq!(view.transcript.len(), 0, "finalize 前不进 transcript");

        view.discard_live_turn();

        assert!(
            view.live.assistant.is_none() && view.live.reasoning.is_none(),
            "restart 必须丢弃 partial 流式内容（整个 turn 重新生成）"
        );
        assert_eq!(
            view.transcript.len(),
            0,
            "丢弃的 partial 不得进入 transcript（durable 事实在 session）"
        );
    }

    #[test]
    fn new_output_returns_to_follow_mode() {
        // 整改 C + TUI v2：scroll lock 期间新输出不再强制拉回底部，只计数。
        let mut view = ViewModel::default();
        // 模拟已布局（layout_top + 高度表；scroll_up 基于它移动锚点）。
        view.push_line(LineKind::Assistant, "a");
        view.push_line(LineKind::Assistant, "b");
        view.layout_top = Some((EntryId(1), 0));
        view.entry_heights.insert(EntryId(1), 1);
        view.entry_heights.insert(EntryId(2), 1);
        view.scroll_up(20);
        assert_eq!(
            view.scroll_mode,
            ScrollMode::Locked(ScrollAnchor {
                entry_id: EntryId(1),
                row_in_entry: 0,
            }),
            "scroll lock 保持（锚定最早行）"
        );
        view.push_line(LineKind::Assistant, "new");
        assert_eq!(view.pending_below, 1, "新条目计数");
        // Ctrl+End 恢复跟随并清空计数。
        view.follow_tail();
        assert_eq!(view.scroll_mode, ScrollMode::Follow);
        assert_eq!(view.pending_below, 0);
    }

    #[test]
    fn tool_card_lifecycle_matches_by_call_id() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", Some("bash: cargo test".into()), None);
        view.begin_tool("call-2", "read", Some("read src/main.rs".into()), None);
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Failed,
            1234,
            Some(2),
            "exit_code: 1\n错误详情",
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!("call-1 必须是卡片");
        };
        assert_eq!(card.name, "bash");
        assert_eq!(
            card.target.as_deref(),
            Some("bash: cargo test"),
            "detail 保留实际命令"
        );
        assert_eq!(
            card.state,
            ToolCardState::Done {
                status: ToolStatus::Failed,
                duration_ms: 1234,
                exit_code: Some(2)
            }
        );
        assert!(card.tail.as_deref().unwrap_or("").contains("错误详情"));
        // 未完成的 call-2 仍在 live 区（§7.2），保持 Running。
        let card2 = view.live.tools.get("call-2").expect("call-2 必须在 live");
        assert_eq!(card2.card.state, ToolCardState::Running);
        assert_eq!(
            view.transcript.len(),
            1,
            "只有 call-1 finalize 进 transcript"
        );
    }

    #[test]
    fn success_card_has_no_tail() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", None, None);
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Succeeded,
            42,
            Some(0),
            "大量输出",
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert_eq!(
            card.state,
            ToolCardState::Done {
                status: ToolStatus::Succeeded,
                duration_ms: 42,
                exit_code: Some(0)
            }
        );
        assert!(card.tail.is_none());
    }

    #[test]
    fn tail_is_bounded() {
        let mut view = ViewModel::default();
        view.begin_tool("c", "bash", None, None);
        view.finish_tool(
            ("c", "bash"),
            ToolStatus::Failed,
            0,
            None,
            "x".repeat(10_000),
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert!(card.tail.as_ref().unwrap().chars().count() <= 241);
    }

    #[test]
    fn usage_accumulates() {
        let mut view = ViewModel::default();
        view.add_usage(&Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
        });
        view.add_usage(&Usage {
            input_tokens: 200,
            output_tokens: 60,
            cache_read_tokens: 20,
        });
        assert_eq!(view.input_tokens, 300);
        assert_eq!(view.output_tokens, 110);
        assert_eq!(view.cache_read_tokens, 30, "缓存命中 token 必须累积");
    }

    /// §16.2：配置单价后按 token 累计花费（每百万 token 美元）。
    #[test]
    fn usage_accumulates_cost_with_pricing() {
        let mut view = ViewModel {
            price_input: Some(1.0),  // $1/1M input
            price_output: Some(2.0), // $2/1M output
            ..Default::default()
        };
        // 1M input tokens + 1M output tokens → 1.0 + 2.0 = 3.0。
        view.add_usage(&Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 100,
        });
        assert!(
            (view.cost_usd - 3.0).abs() < 1e-9,
            "cost = {:.4}",
            view.cost_usd
        );
        // 缓存命中不影响花费（只展示命中量）。
        assert_eq!(view.cache_read_tokens, 100);

        // 无 pricing → cost 恒 0（不显示花费）。
        let mut view2 = ViewModel::default();
        view2.add_usage(&Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
        });
        assert_eq!(view2.cost_usd, 0.0, "未配置单价不计算花费");
    }

    #[test]
    fn command_menu_filters_and_preserves_selection() {
        let mut view = ViewModel {
            input: "/set".into(),
            ..Default::default()
        };
        view.refresh_command_menu();
        let menu = view.menu.as_ref().expect("菜单应打开");
        assert_eq!(menu.items.len(), 1);
        assert_eq!(menu.items[0].0, "settings");

        // 完全匹配时保留一条。
        view.input = "/help".into();
        view.refresh_command_menu();
        assert_eq!(view.menu.as_ref().unwrap().items[0].0, "help");

        // 无前缀 '/' 时关闭。
        view.input = "hello".into();
        view.refresh_command_menu();
        assert!(view.menu.is_none());

        // 前缀无匹配时关闭（未知命令走普通消息）。
        view.input = "/xyz".into();
        view.refresh_command_menu();
        assert!(view.menu.is_none());
    }

    #[test]
    fn menu_completion_replaces_input() {
        let mut view = ViewModel {
            input: "/set".into(),
            ..Default::default()
        };
        view.refresh_command_menu();
        view.complete_menu_command();
        assert_eq!(view.input, "/settings");
        assert_eq!(view.input_cursor, "/settings".len());
    }

    #[test]
    fn reasoning_fold_defaults_visible() {
        // 整改 A1：reasoning 默认折叠（不形成正文墙）。
        let view = ViewModel::default();
        assert!(!view.reasoning_visible);
    }

    #[test]
    fn finish_tool_keeps_success_output_expandable() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", None, None);
        // 运行中实时输出累积。
        view.append_tool_output("call-1", "line-1\n");
        view.append_tool_output("call-1", "line-2\n");
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Succeeded,
            42,
            Some(0),
            "line-1\nline-2\n",
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        // 成功也保留完整输出（此前被丢弃）；净化只剥元数据头，正文原样
        //（lines() 归一化会去掉末尾换行）。
        assert_eq!(card.output.as_deref(), Some("line-1\nline-2"));
        assert_eq!(card.tail, None, "成功卡片折叠态不显示红色 tail");
        assert!(!card.expanded);
        // 展开切换。
        view.toggle_expand("call-1");
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert!(card.expanded);
        view.toggle_last_tool_expanded();
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert!(!card.expanded, "Alt+E 再次切换回折叠");
    }

    #[test]
    fn append_tool_output_is_bounded() {
        let mut view = ViewModel::default();
        view.begin_tool("c", "bash", None, None);
        let big = "x".repeat(40 * 1024);
        view.append_tool_output("c", &big);
        let tool = view.live.tools.get("c").expect("live 区必须有卡片");
        let card = &tool.card;
        assert!(card.output_truncated, "超过预算必须标记截断");
        assert!(card.output.as_ref().unwrap().len() <= MAX_CARD_OUTPUT);
        // 尾部保留（错误相关输出通常在末尾）。
        assert!(card.output.as_ref().unwrap().ends_with('x'));
    }

    #[test]
    fn at_menu_filters_file_index() {
        let mut view = ViewModel {
            file_index: vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                "Cargo.toml".into(),
            ],
            ..Default::default()
        };
        view.input = "看看 @src/".into();
        assert!(view.has_at_token());
        view.refresh_at_menu();
        let menu = view.menu.as_ref().expect("@ 菜单应打开");
        assert_eq!(menu.kind, crate::tui::model::MenuKind::File);
        assert_eq!(menu.items.len(), 2, "只显示前缀匹配的文件");
        // Enter 补全：@token 被替换为选中路径。
        view.menu.as_mut().unwrap().selected = 0;
        view.complete_menu_command();
        assert_eq!(view.input, "看看 src/main.rs");
        assert_eq!(view.menu, None, "补全后菜单关闭");
    }

    #[test]
    fn at_menu_closes_without_match_or_token() {
        let mut view = ViewModel {
            file_index: vec!["src/main.rs".into()],
            ..Default::default()
        };
        // 无 @ token：不弹菜单。
        view.input = "hello".into();
        view.refresh_at_menu();
        assert!(view.menu.is_none());
        // 有 token 但无匹配：关闭。
        view.input = "@no-such-file".into();
        view.refresh_at_menu();
        assert!(view.menu.is_none());
    }
}

#[cfg(test)]
mod p1_message_cap_tests {
    use super::*;

    /// P1-9：assistant 流式消息超过 MAX_MESSAGE_CHARS 时必须截断并标记，
    /// 不能无限膨胀（此前工具输出有上限而消息没有）。
    #[test]
    fn stream_delta_is_bounded_for_messages() {
        let mut view = ViewModel::default();
        let chunk = "a".repeat(1024);
        let total_chunks = (MAX_MESSAGE_CHARS / 1024) + 10;
        for _ in 0..total_chunks {
            view.push_stream_delta(LineKind::Assistant, &chunk);
        }
        let msg = view.live.assistant.as_ref().expect("live 区必须有消息");
        assert!(
            msg.text.len() <= MAX_MESSAGE_CHARS + 32,
            "消息必须被截断: {} > {}",
            msg.text.len(),
            MAX_MESSAGE_CHARS
        );
        assert!(msg.text.contains("truncated"), "截断必须带标记");
    }

    /// P1-9：reasoning 同样有界。
    #[test]
    fn stream_reasoning_is_bounded() {
        let mut view = ViewModel::default();
        for _ in 0..(MAX_MESSAGE_CHARS / 1024 + 10) {
            view.push_stream_delta(LineKind::Reasoning, &"r".repeat(1024));
        }
        let msg = view.live.reasoning.as_ref().expect("live 区必须有消息");
        assert!(msg.text.len() <= MAX_MESSAGE_CHARS + 32);
    }
}

#[cfg(test)]
mod p2_card_nav_tests {
    use super::*;

    fn view_with_two_cards() -> ViewModel {
        let mut view = ViewModel::default();
        // 卡 1：成功。
        view.begin_tool(
            String::from("call-1"),
            String::from("read"),
            Some(String::from("a.rs")),
            None,
        );
        view.finish_tool(
            (String::from("call-1"), String::from("read")),
            ToolStatus::Succeeded,
            10,
            None,
            String::from("ok"),
            None,
        );
        // 卡 2：失败。
        view.begin_tool(
            String::from("call-2"),
            String::from("bash"),
            Some(String::from("ls")),
            Some(String::from("ls")),
        );
        view.finish_tool(
            (String::from("call-2"), String::from("bash")),
            ToolStatus::Failed,
            20,
            Some(1),
            String::from("boom"),
            None,
        );
        view
    }

    /// P2：Alt+O 打开最近一张失败卡片（跳过成功卡片）。
    #[test]
    fn failed_tool_overlay_skips_success_cards() {
        let mut view = view_with_two_cards();
        view.open_failed_tool_overlay();
        let overlay = view.overlay.expect("失败卡片必须可打开");
        assert_eq!(overlay.tool_id.as_deref(), Some("call-2"));
        assert!(overlay.title.contains("failed"), "{overlay:?}");
    }

    /// P2：Alt+[ / Alt+] 在卡片间循环切换。
    #[test]
    fn cycle_tool_overlay_wraps_around() {
        let mut view = view_with_two_cards();
        view.open_tool_overlay("call-1");
        view.cycle_tool_overlay(1);
        assert_eq!(
            view.overlay
                .as_ref()
                .and_then(|o| o.tool_id.clone())
                .as_deref(),
            Some("call-2")
        );
        view.cycle_tool_overlay(1);
        assert_eq!(
            view.overlay
                .as_ref()
                .and_then(|o| o.tool_id.clone())
                .as_deref(),
            Some("call-1"),
            "循环回绕"
        );
        view.cycle_tool_overlay(-1);
        assert_eq!(
            view.overlay
                .as_ref()
                .and_then(|o| o.tool_id.clone())
                .as_deref(),
            Some("call-2"),
            "反向"
        );
    }

    /// BUG-006：/new 后屏幕投影必须全部清空（模型上下文已清空）。
    #[test]
    fn reset_for_new_session_clears_all_projection() {
        let mut view = ViewModel::default();
        view.push_line(LineKind::User, "旧会话问题");
        view.push_line(LineKind::Assistant, "旧会话回答");
        view.push_stream_delta(LineKind::Assistant, "流式中");
        view.plan = Some(crate::tool::plan::Plan {
            explanation: Some("旧计划".into()),
            items: Vec::new(),
        });
        view.input_tokens = 123;
        view.output_tokens = 456;
        view.context_usage = Some((10, 100));
        view.open_modal("/old", "旧 modal");
        view.lock_to(crate::tui::scroll::EntryId(1), 0);
        view.reset_for_new_session();
        assert!(view.transcript.is_empty(), "transcript 必须清空");
        assert!(view.live.assistant.is_none() && view.live.reasoning.is_none());
        assert!(view.plan.is_none(), "旧计划必须清空");
        assert_eq!(view.input_tokens, 0);
        assert_eq!(view.output_tokens, 0);
        assert!(view.context_usage.is_none());
        assert!(view.modal.is_none());
        assert_eq!(view.scroll_mode, ScrollMode::Follow, "滚动必须回到底部");
    }
    /// BUG-006：恢复 session 后必须把 history 重建到屏幕（User/Assistant/工具摘要）。
    #[test]
    fn load_history_rebuilds_transcript_from_context() {
        let mut view = ViewModel::default();
        view.push_line(LineKind::User, "旧内容"); // 先有旧屏幕
        let history = vec![
            crate::provider::ChatMessage::User("新会话问题".into()),
            crate::provider::ChatMessage::Assistant {
                content: "新会话回答".into(),
                tool_calls: Vec::new(),
            },
            crate::provider::ChatMessage::Tool {
                tool_call_id: "call-1".into(),
                name: "bash".into(),
                content: "status: failed\\nerror: x".into(),
            },
        ];
        view.load_history(&history);
        assert_eq!(view.transcript.len(), 3, "旧屏幕被替换为重建历史");
        let kinds: Vec<LineKind> = view
            .transcript
            .iter()
            .map(|e| match e {
                Entry::Message { line, .. } => line.kind,
                Entry::Tool { .. } => LineKind::Tool,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![LineKind::User, LineKind::Assistant, LineKind::Tool]
        );
    }

    /// §24：点击工具卡片设置 active_hit（高亮反馈），关闭 Overlay 清除。
    #[test]
    fn tool_overlay_sets_and_clears_active_hit() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", Some("run".into()), None);
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Succeeded,
            10,
            Some(0),
            "",
            None,
        );
        assert!(view.active_hit.is_none(), "初始无高亮");

        view.open_tool_overlay("call-1");
        assert!(
            matches!(&view.active_hit, Some(crate::tui::HitTarget::Tool(id)) if id == "call-1"),
            "点击后必须设置 active_hit: {:?}",
            view.active_hit
        );
        assert!(view.overlay.is_some(), "Overlay 打开");

        view.close_overlay();
        assert!(view.active_hit.is_none(), "关闭 Overlay 清除高亮");
        assert!(view.overlay.is_none());
    }

    /// §用户诉求：应用内选择——语义位置（entry + 偏移），开始/更新/清除。
    /// 语义选区指向内容而非屏幕坐标；反向拖动由 normalized() 处理。
    #[test]
    fn selection_state_lifecycle() {
        use crate::tui::interaction::TextPosition;
        use crate::tui::scroll::EntryId;
        let mut view = ViewModel::default();
        assert!(view.selection.is_none());

        let p2 = TextPosition {
            entry_id: EntryId(1),
            offset: 2,
        };
        let p3 = TextPosition {
            entry_id: EntryId(1),
            offset: 3,
        };
        let p7 = TextPosition {
            entry_id: EntryId(1),
            offset: 7,
        };
        view.selection_start(p2);
        let sel = view.selection.as_ref().expect("开始选择");
        assert_eq!(sel.anchor, p2);
        assert_eq!(sel.focus, p2);

        view.selection_update(p7);
        let sel = view.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, p2, "anchor 保持按下点");
        assert_eq!(sel.focus, p7);
        assert_eq!(sel.normalized(), (p2, p7));

        // 反向拖动：anchor 不变，focus 更新。
        view.selection_update(p3);
        let sel = view.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, p2);
        assert_eq!(sel.focus, p3);
        // 值排序：offset 2 < offset 3 → normalized 为 (p2, p3)。
        assert_eq!(sel.normalized(), (p2, p3));

        view.selection_end();
        assert!(view.selection.is_some(), "释放后选区保留");

        view.selection_clear();
        assert!(view.selection.is_none());
    }

    /// §PointerHit：selected_text 从 ViewModel 语义文本提取（不依赖 viewport）。
    /// 选区指向 (entry, offset)，resize/滚动后仍精确；无选区返回空。
    #[test]
    fn selected_text_extracts_from_transcript_entries() {
        use crate::tui::interaction::TextPosition;
        use crate::tui::scroll::EntryId;
        let mut view = ViewModel::default();
        view.push_line(LineKind::User, "hello world");
        view.push_line(LineKind::Assistant, "second line");

        // 无选区 → 空。
        assert_eq!(view.selected_text(), "");

        // 选区：entry 1 的 offset 0..5 → "hello"。
        view.selection_start(TextPosition {
            entry_id: EntryId(1),
            offset: 0,
        });
        view.selection_update(TextPosition {
            entry_id: EntryId(1),
            offset: 5,
        });
        assert_eq!(view.selected_text(), "hello");

        // 跨 entry：entry 1 offset 6 → entry 2 offset 6 → "world\nsecond"。
        view.selection_start(TextPosition {
            entry_id: EntryId(1),
            offset: 6,
        });
        view.selection_update(TextPosition {
            entry_id: EntryId(2),
            offset: 6,
        });
        assert_eq!(view.selected_text(), "world\nsecond");
    }

    /// §PointerHit：streaming 期间选中的稳定 id 在 finalize 后仍有效——
    /// 运行中拖选 → Agent 输出结束 → Ctrl+C 复制不悬空。
    #[test]
    fn streaming_selection_survives_finalize() {
        use crate::tui::interaction::TextPosition;
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "streaming content");
        // streaming 期间选中（entry_id 已分配）。
        let live_id = view
            .live
            .assistant
            .as_ref()
            .expect("streaming 消息必须有 entry_id")
            .entry_id;
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 0,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 9,
        });
        assert_eq!(view.selected_text(), "streaming");
        // finalize：沿用同一 id，选区不悬空。
        view.finalize_streaming();
        let finalized = view.transcript.last().expect("finalize 后必须提交").id();
        assert_eq!(finalized, live_id, "finalize 必须沿用 streaming 的稳定 id");
        assert_eq!(
            view.selected_text(),
            "streaming",
            "finalize 后选区仍指向同一内容"
        );
    }

    /// §PointerHit ⑤：运行中工具卡片有独立稳定 id，finalize 后选区不悬空，
    /// selected_text 能覆盖 tool 输出（情况 A：仅 tool 运行时）。
    #[test]
    fn live_tool_selection_survives_finalize() {
        use crate::tui::interaction::TextPosition;
        use crate::tui::scroll::EntryId;
        let mut view = ViewModel::default();
        view.begin_tool("c1", "bash", Some("cargo test".into()), None);
        view.append_tool_output("c1", "running tests...");
        // 运行中 tool 有独立 id（不是 assistant/reasoning 的，也不是哨兵）。
        let live_id = view.live.tools.get("c1").expect("c1 必须在 live").entry_id;
        assert_ne!(live_id, EntryId(u64::MAX), "live tool 不得用哨兵 id");
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 0,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 7,
        });
        // tool 语义文本 = "bash cargo test\nrunning tests..."；offset 0..7 = "bash ca"。
        assert_eq!(view.selected_text(), "bash ca");
        // 选 body 部分：offset 跳过 "bash cargo test\n"（16 chars）后到 "running"。
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 16,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 23,
        });
        assert_eq!(view.selected_text(), "running");
        // finish_tool：沿用同一 id，选区不悬空。
        view.finish_tool(("c1", "bash"), ToolStatus::Succeeded, 10, None, "", None);
        let finalized = view.transcript.last().expect("finish 后必须提交").id();
        assert_eq!(finalized, live_id, "finish_tool 必须沿用 begin 的稳定 id");
        // §修复：header 语义含 meta（与渲染 card_semantic_header 一致）。
        // Running（无 meta）→ Done（+ " 10ms"）：header 从 16 chars 变 21，
        // body 起始偏移后移——这是内容真实变化（meta 加入），选区 offset
        // 不再指向旧位置。验证新 offset 正确落到 body："bash cargo test 10ms"
        // (21 chars) + "running"。
        assert_eq!(
            view.selected_text(),
            "10ms\nru",
            "finalize 后 header 语义加入 meta，偏移指向新内容（预期变化）"
        );
        // 用新 offset 重新选中 body 的 "running"：内容正确定位。
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 21,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 28,
        });
        assert_eq!(view.selected_text(), "running", "body 内容不受 meta 影响");
    }
}
