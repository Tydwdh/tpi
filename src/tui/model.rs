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
    /// 输出被截断（超过 MAX_CARD_OUTPUT 丢弃中段）。
    pub output_truncated: bool,
    /// 展开状态（active 卡片内联显示完整输出；scrollback 卡片走 overlay）。
    pub expanded: bool,
    /// 失败/超时等终态的关键 tail（有界，折叠态显示最多 4 行）。
    pub tail: Option<String>,
}

/// 卡片输出上限（UI 内存与渲染预算；完整输出仍可通过 read @artifact 读取）。
pub const MAX_CARD_OUTPUT: usize = 32 * 1024;
/// P1-9：assistant/reasoning 单条消息上限（工具卡输出另有 MAX_CARD_OUTPUT）。
/// 超出丢弃中段并标记 truncated，防止 transcript 无限膨胀。
pub const MAX_MESSAGE_CHARS: usize = 256 * 1024;
/// 命令上限（overlay 展示用；正常命令远小于此）。
pub const MAX_CARD_COMMAND: usize = 8 * 1024;

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
    pub scroll: u16,
    /// 对应工具卡 id（P2：Alt+[/Alt+] 卡片间切换用；reasoning overlay 为 None）。
    pub tool_id: Option<String>,
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
        }
    }

    /// reasoning 原文 overlay。
    pub fn for_reasoning(text: &str) -> Self {
        Self {
            title: "思考（reasoning）".into(),
            command: None,
            body: text.to_string(),
            body_truncated: false,
            scroll: 0,
            tool_id: None,
        }
    }
}

/// 转录条目：消息或工具卡片（保持原始出现顺序）。
///
/// 每个条目带稳定 [`EntryId`]（TUI v2 §4.1）：trim/折叠不会改变 id，
/// 滚动锚点与搜索命中都基于它，而不是 Vec index。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Message { id: EntryId, line: TranscriptLine },
    Tool { id: EntryId, card: ToolCard },
}

impl Entry {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::Message { id, .. } | Entry::Tool { id, .. } => *id,
        }
    }
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

/// 只读渲染输入（§16.1：renderer 的唯一输入）。
#[derive(Debug, Clone)]
pub struct ViewModel {
    pub transcript: Vec<Entry>,
    pub input: String,
    pub input_cursor: usize,
    pub plan: Option<Plan>,
    pub status: StatusLine,
    pub model_name: String,
    /// workspace 目录名（footer 展示）。
    pub workspace: String,
    /// 当前正在运行的 turn 数。
    pub turn: u32,
    /// 滚动模式（TUI v2 §3）：Follow = 尾部；Locked = 锚定 EntryId + row。
    pub scroll_mode: ScrollMode,
    /// 兼容字段：距转录末尾的逻辑行数（0 = 跟随）。TUI v2 起
    /// Locked 的事实源是 [`scroll_mode`]，本字段仅保留给旧测试/旧语义。
    pub transcript_scroll: u16,
    /// scroll lock 期间到达的新条目数（footer 提示；End/Ctrl+End 清空）。
    pub pending_below: u64,
    /// 最近一次布局的视口顶部行（renderer 写回；滚动操作的基础，§4）。
    pub layout_top: Option<(EntryId, usize)>,
    /// 各 entry 最近一次布局的 visual 高度（renderer 写回；滚动跨 entry 用）。
    pub entry_heights: HashMap<EntryId, usize>,
    /// 最近一次布局的转录区高度（PageUp/PageDown 按 viewport-2 移动，§10）。
    pub transcript_rows: u16,
    /// 思考内容是否展开（Alt+T 切换，§16.2：thinking 可折叠）。
    pub reasoning_visible: bool,
    /// 本会话累计 token 用量（AgentOutcome.usage 累积，§16.2：无 pricing 时显示 usage）。
    pub input_tokens: u64,
    pub output_tokens: u64,
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
    pub next_version: u64,
    pub next_entry_id: u64,
}

impl Default for ViewModel {
    fn default() -> Self {
        Self {
            transcript: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            plan: None,
            status: StatusLine::Idle,
            model_name: "?".into(),
            workspace: String::new(),
            turn: 0,
            scroll_mode: ScrollMode::Follow,
            transcript_scroll: 0,
            pending_below: 0,
            layout_top: None,
            entry_heights: HashMap::new(),
            transcript_rows: 0,
            reasoning_visible: false,
            input_tokens: 0,
            output_tokens: 0,
            anim_tick: 0,
            menu: None,
            context_usage: None,
            file_index: Vec::new(),
            overlay: None,
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

    /// 追加转录行。
    pub fn push_line(&mut self, kind: LineKind, text: impl Into<String>) {
        let version = self.alloc_version();
        let id = self.alloc_entry_id();
        self.transcript.push(Entry::Message {
            id,
            line: TranscriptLine {
                kind,
                text: text.into(),
                version,
            },
        });
        self.note_new_content();
        self.trim_transcript();
    }

    /// 追加同一条流式消息。provider 的 token chunk 不是视觉行，不能逐 chunk
    /// 创建 transcript item，否则会把“你好”渲染成多条带前缀的碎片行。
    pub fn push_stream_delta(&mut self, kind: LineKind, text: &str) {
        let version = self.alloc_version();
        match self.transcript.last_mut() {
            Some(Entry::Message { line, .. }) if line.kind == kind => {
                // P1-9：单条消息有界（超出丢弃中段并标记，防 transcript 膨胀）。
                if line.text.len() < MAX_MESSAGE_CHARS {
                    let room = MAX_MESSAGE_CHARS - line.text.len();
                    if text.len() <= room {
                        line.text.push_str(text);
                    } else {
                        // 截断到 UTF-8 字符边界，避免切出非法字节（§24 统一 helper）。
                        line.text
                            .push_str(&text[..crate::tui::text::floor_char_boundary(text, room)]);
                        line.text.push_str("…[truncated]");
                    }
                } else if !line.text.contains("truncated") {
                    // 已满后仍来内容：标记一次，后续静默丢弃。
                    line.text.push_str("\n…[truncated]");
                }
                line.version = version; // 文本变化 → 渲染缓存失效
            }
            _ => {
                let id = self.alloc_entry_id();
                self.transcript.push(Entry::Message {
                    id,
                    line: TranscriptLine {
                        kind,
                        text: text.to_string(),
                        version,
                    },
                });
                self.note_new_content();
                self.trim_transcript();
            }
        }
        // 整改 C：不再强制拉回底部——用户滚动查看历史时新输出只计数。
        // Follow 模式保持跟随；Locked 时累计 pending（§16）。
    }

    /// 整改 C + TUI v2 §16：Locked 期间的新条目计数（footer 提示）。
    fn note_new_content(&mut self) {
        if self.scroll_mode != ScrollMode::Follow {
            self.pending_below += 1;
        }
        // 兼容投影：Locked 时保持非零（旧测试/旧渲染读 transcript_scroll）。
        self.transcript_scroll = if self.scroll_mode == ScrollMode::Follow {
            0
        } else {
            self.transcript_scroll.max(1)
        };
    }

    /// 恢复 follow-tail（End/Ctrl+End，§3.1）：回到底部并清空新消息计数。
    pub fn follow_tail(&mut self) {
        self.scroll_mode = ScrollMode::Follow;
        self.transcript_scroll = 0;
        self.pending_below = 0;
    }

    /// 进入 Locked 并锚定到指定行（§3.2：滚动/搜索跳转后新输出不动视口）。
    pub fn lock_to(&mut self, entry: EntryId, row_in_entry: usize) {
        self.scroll_mode = ScrollMode::Locked(ScrollAnchor {
            entry_id: entry,
            row_in_entry,
        });
        self.transcript_scroll = self.transcript_scroll.max(1);
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
            let area = self.transcript_rows as usize;
            // 到底后保持 Locked（End 显式回 Follow，§10）。
            let new_top = crate::tui::scroll::move_by_rows(&ids, &heights, top_row, delta as isize);
            let new_row = crate::tui::scroll::row_of(&ids, &heights, new_top.0, new_top.1);
            if new_row + area >= total {
                // 已到最底：仍 Locked（不自动 Follow），但清空 pending。
                self.pending_below = 0;
            }
            self.lock_to(new_top.0, new_top.1);
        }
    }

    /// 滚动基准行：Locked = 当前锚点；Follow = 最近布局的视口顶部行
    /// （无布局信息时锚定最早条目）。
    fn scroll_base(&self) -> Option<(EntryId, usize)> {
        match self.scroll_mode {
            ScrollMode::Locked(anchor) => Some((anchor.entry_id, anchor.row_in_entry)),
            ScrollMode::Follow => self
                .layout_top
                .or_else(|| self.transcript.first().map(|e| (e.id(), 0))),
        }
    }

    /// 平行高度表（entry_heights 缓存，缺失的 entry 按 1 行估算）。
    fn current_heights(&self, ids: &[EntryId]) -> Vec<usize> {
        ids.iter()
            .map(|id| self.entry_heights.get(id).copied().unwrap_or(1))
            .collect()
    }

    /// 工具开始：追加一张运行中的卡片（整改 A2：一个 call 一张卡，原地更新）。
    pub fn begin_tool(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        target: Option<String>,
        command: Option<String>,
    ) {
        let entry_id = self.alloc_entry_id();
        self.transcript.push(Entry::Tool {
            id: entry_id,
            card: ToolCard {
                id: id.into(),
                name: name.into(),
                target,
                command,
                state: ToolCardState::Running,
                output: None,
                output_truncated: false,
                expanded: false,
                tail: None,
            },
        });
        self.note_new_content();
        self.trim_transcript();
    }

    /// 工具执行中的实时输出增量（有界累积；运行中卡片可见）。
    pub fn append_tool_output(&mut self, id: impl Into<String>, text: impl Into<String>) {
        let id = id.into();
        let text = text.into();
        if let Some(Entry::Tool { card, .. }) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry, Entry::Tool { card, .. } if card.id == id))
        {
            let current = card.output.get_or_insert_with(String::new);
            if current.len() + text.len() > MAX_CARD_OUTPUT {
                card.output_truncated = true;
                // 丢弃旧内容超出部分（保留尾部：错误相关输出通常在末尾）。
                // §24：drain 起点必须落在字符边界，否则中文/emoji 输出会 panic。
                let overflow = current.len() + text.len() - MAX_CARD_OUTPUT;
                let drop =
                    crate::tui::text::floor_char_boundary(current, overflow.min(current.len()));
                current.drain(..drop);
                // 单块超大时只保留其尾部（起点同样按字符边界对齐）。
                let remaining = MAX_CARD_OUTPUT.saturating_sub(current.len());
                let tail = crate::tui::text::suffix_by_bytes_safe(&text, remaining);
                current.push_str(tail);
            } else {
                current.push_str(&text);
            }
        }
    }

    /// 切换某张卡片展开/折叠（显示完整输出正文）。
    pub fn toggle_expand(&mut self, id: impl Into<String>) {
        let id = id.into();
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
        for entry in self.transcript.iter().rev() {
            if let Entry::Tool { card, .. } = entry {
                self.overlay = Some(OverlayState::for_tool(card));
                return;
            }
        }
    }

    /// P2：打开最近一张失败/被拒/取消的工具卡片 Overlay（Alt+O）。
    pub fn open_failed_tool_overlay(&mut self) {
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
    pub fn open_tool_overlay(&mut self, id: impl Into<String>) {
        let id = id.into();
        for entry in self.transcript.iter().rev() {
            if let Entry::Tool { card, .. } = entry
                && card.id == id
            {
                self.overlay = Some(OverlayState::for_tool(card));
                return;
            }
        }
    }

    /// 打开 reasoning 原文 Overlay（点击折叠的 reasoning 行）。
    pub fn open_reasoning_overlay(&mut self, id: EntryId) {
        let Some(line) = self.transcript.iter().find_map(|entry| match entry {
            Entry::Message { id: eid, line } if *eid == id => Some(line),
            _ => None,
        }) else {
            return;
        };
        if line.kind != LineKind::Reasoning {
            return;
        }
        self.overlay = Some(OverlayState::for_reasoning(&line.text));
    }

    /// 关闭 Overlay（Esc）。
    pub fn close_overlay(&mut self) {
        self.overlay = None;
    }

    /// 工具终态：按 call_id 定位卡片，更新状态与耗时；失败时保留关键 tail（有界）。
    pub fn finish_tool(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        status: ToolStatus,
        duration_ms: u64,
        exit_code: Option<i32>,
        tail: impl Into<String>,
    ) {
        let id = id.into();
        let name = name.into();
        let tail = tail.into();
        // 从后往前找（同一 run 内 call_id 唯一；防御性支持乱序完成）。
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
                // 完整输出始终保留（成功也可见，Alt+E/鼠标展开）；失败时折叠态显示 tail。
                if !tail.is_empty() {
                    card.output = Some(bound_output(&tail));
                    if tail.len() > MAX_CARD_OUTPUT {
                        card.output_truncated = true;
                    }
                }
                if status != ToolStatus::Succeeded && !tail.is_empty() {
                    card.tail = Some(bound_tail(&tail));
                }
                return;
            }
        }
        // 未找到（理论上不会：ToolStarted 先于 ToolCompleted）——兜底追加终态卡片。
        let entry_id = self.alloc_entry_id();
        self.transcript.push(Entry::Tool {
            id: entry_id,
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
                output_truncated: tail.len() > MAX_CARD_OUTPUT,
                expanded: false,
                tail: if status == ToolStatus::Succeeded {
                    None
                } else {
                    Some(bound_tail(&tail))
                },
            },
        });
        self.trim_transcript();
    }

    /// 有界转录（防止长会话内存无限增长）。trim 不改变 EntryId（§4.1）；
    /// 锚点 entry 被 trim 后由布局侧回退到最早现存 entry（scroll.rs，§68）。
    fn trim_transcript(&mut self) {
        if self.transcript.len() > 2000 {
            let keep = self.transcript.len() - 2000;
            let removed: Vec<EntryId> = self.transcript[..keep].iter().map(Entry::id).collect();
            self.transcript.drain(..keep);
            for id in removed {
                self.entry_heights.remove(&id);
            }
        }
    }

    /// 累积一次 run 的 token 用量（§16.2：footer 展示）。
    pub fn add_usage(&mut self, usage: &Usage) {
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
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

    #[test]
    fn streaming_chunks_form_one_logical_message() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "你");
        view.push_stream_delta(LineKind::Assistant, "好");
        assert_eq!(view.transcript.len(), 1);
        let Entry::Message { line, .. } = &view.transcript[0] else {
            panic!("assistant delta 必须是消息条目");
        };
        assert_eq!(line.text, "你好");
    }

    #[test]
    fn stream_delta_bumps_version_for_render_cache() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "a");
        let v1 = match &view.transcript[0] {
            Entry::Message { line, .. } => line.version,
            _ => panic!(),
        };
        view.push_stream_delta(LineKind::Assistant, "b");
        let v2 = match &view.transcript[0] {
            Entry::Message { line, .. } => line.version,
            _ => panic!(),
        };
        assert_ne!(v1, v2);
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
        assert_eq!(view.transcript_scroll, 1, "兼容投影：Locked 非零");
        view.push_line(LineKind::Assistant, "new");
        assert_eq!(view.pending_below, 1, "新条目计数");
        // Ctrl+End 恢复跟随并清空计数。
        view.follow_tail();
        assert_eq!(view.scroll_mode, ScrollMode::Follow);
        assert_eq!(view.transcript_scroll, 0);
        assert_eq!(view.pending_below, 0);
    }

    #[test]
    fn tool_card_lifecycle_matches_by_call_id() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", Some("bash: cargo test".into()), None);
        view.begin_tool("call-2", "read", Some("read src/main.rs".into()), None);
        view.finish_tool(
            "call-1",
            "bash",
            ToolStatus::Failed,
            1234,
            Some(2),
            "exit_code: 1\n错误详情",
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
        // 未完成的 call-2 保持 Running。
        let Entry::Tool { card: card2, .. } = &view.transcript[1] else {
            panic!("call-2 必须是卡片");
        };
        assert_eq!(card2.state, ToolCardState::Running);
    }

    #[test]
    fn success_card_has_no_tail() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", None, None);
        view.finish_tool(
            "call-1",
            "bash",
            ToolStatus::Succeeded,
            42,
            Some(0),
            "大量输出",
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
        view.finish_tool("c", "bash", ToolStatus::Failed, 0, None, "x".repeat(10_000));
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
        });
        view.add_usage(&Usage {
            input_tokens: 200,
            output_tokens: 60,
        });
        assert_eq!(view.input_tokens, 300);
        assert_eq!(view.output_tokens, 110);
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
            "call-1",
            "bash",
            ToolStatus::Succeeded,
            42,
            Some(0),
            "line-1\nline-2\n",
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        // 成功也保留完整输出（此前被丢弃）。
        assert_eq!(card.output.as_deref(), Some("line-1\nline-2\n"));
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
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
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
        let entry = view.transcript.last().unwrap();
        let Entry::Message { line, .. } = entry else {
            panic!("最后一条应是消息");
        };
        assert!(
            line.text.len() <= MAX_MESSAGE_CHARS + 32,
            "消息必须被截断: {} > {}",
            line.text.len(),
            MAX_MESSAGE_CHARS
        );
        assert!(line.text.contains("truncated"), "截断必须带标记");
    }

    /// P1-9：reasoning 同样有界。
    #[test]
    fn stream_reasoning_is_bounded() {
        let mut view = ViewModel::default();
        for _ in 0..(MAX_MESSAGE_CHARS / 1024 + 10) {
            view.push_stream_delta(LineKind::Reasoning, &"r".repeat(1024));
        }
        let entry = view.transcript.last().unwrap();
        let Entry::Message { line, .. } = entry else {
            panic!("最后一条应是消息");
        };
        assert!(line.text.len() <= MAX_MESSAGE_CHARS + 32);
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
            String::from("call-1"),
            String::from("read"),
            ToolStatus::Succeeded,
            10,
            None,
            String::from("ok"),
        );
        // 卡 2：失败。
        view.begin_tool(
            String::from("call-2"),
            String::from("bash"),
            Some(String::from("ls")),
            Some(String::from("ls")),
        );
        view.finish_tool(
            String::from("call-2"),
            String::from("bash"),
            ToolStatus::Failed,
            20,
            Some(1),
            String::from("boom"),
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
}
