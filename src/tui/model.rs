//! TUI 渲染模型（§16.2 信息层级）。
//!
//! ViewModel 是只读渲染输入：由 app 从 ephemeral 事件构建，
//! renderer 不直接访问 Agent/tool 内部状态。
//!
//! M6+：转录条目分为「消息」（User/Assistant/Reasoning/Tool/System）与
//! 「工具卡片」（单行 icon name duration status，§16.2），
//! 支持思考折叠（Alt+T）、命令补全菜单与累积 token 用量。

use crate::session::Usage;
use crate::tool::outcome::ToolStatus;
use crate::tool::plan::Plan;

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

/// 工具卡片（§16.2：单行 `icon name duration status`，运行中动画）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCard {
    /// ToolCallId 的字符串形式。
    pub id: String,
    pub name: String,
    /// 可读命令/参数摘要（如 `bash: cargo test`）；None 表示只有工具名。
    pub detail: Option<String>,
    pub state: ToolCardState,
    /// 失败/超时等终态时携带的关键输出 tail（有界，渲染为红色尾注）。
    pub tail: Option<String>,
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

/// 转录条目：消息或工具卡片（保持原始出现顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Message(TranscriptLine),
    Tool(ToolCard),
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
    /// 与输入前缀匹配的命令（name, 中文说明）。
    pub items: Vec<(&'static str, &'static str)>,
    pub selected: usize,
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
    /// 距离转录末尾的逻辑行数。0 表示跟随最新输出；翻页时增大。
    pub transcript_scroll: u16,
    /// 思考内容是否展开（Alt+T 切换，§16.2：thinking 可折叠）。
    pub reasoning_visible: bool,
    /// 本会话累计 token 用量（AgentOutcome.usage 累积，§16.2：无 pricing 时显示 usage）。
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 动画帧计数（spinner；由 app 的 ticker 推进，§16.1：动画时钟独立）。
    pub anim_tick: u64,
    /// 命令补全菜单（None = 关闭）。
    pub menu: Option<MenuView>,
    pub next_version: u64,
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
            transcript_scroll: 0,
            reasoning_visible: true,
            input_tokens: 0,
            output_tokens: 0,
            anim_tick: 0,
            menu: None,
            next_version: 1,
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

    /// 追加转录行。
    pub fn push_line(&mut self, kind: LineKind, text: impl Into<String>) {
        let version = self.alloc_version();
        self.transcript.push(Entry::Message(TranscriptLine {
            kind,
            text: text.into(),
            version,
        }));
        self.trim_transcript();
    }

    /// 追加同一条流式消息。provider 的 token chunk 不是视觉行，不能逐 chunk
    /// 创建 transcript item，否则会把“你好”渲染成多条带前缀的碎片行。
    pub fn push_stream_delta(&mut self, kind: LineKind, text: &str) {
        let version = self.alloc_version();
        match self.transcript.last_mut() {
            Some(Entry::Message(line)) if line.kind == kind => {
                line.text.push_str(text);
                line.version = version; // 文本变化 → 渲染缓存失效
            }
            _ => {
                self.transcript.push(Entry::Message(TranscriptLine {
                    kind,
                    text: text.to_string(),
                    version,
                }));
                self.trim_transcript();
            }
        }
        // 新输出到达时恢复跟随模式；用户显式翻页后可再次上翻。
        self.transcript_scroll = 0;
    }

    /// 工具开始：追加一张运行中的卡片（§16.2：运行中单行动画）。
    pub fn begin_tool(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        detail: Option<String>,
    ) {
        self.transcript.push(Entry::Tool(ToolCard {
            id: id.into(),
            name: name.into(),
            detail,
            state: ToolCardState::Running,
            tail: None,
        }));
        self.trim_transcript();
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
            if let Entry::Tool(card) = entry
                && card.id == id
            {
                card.name = name;
                card.state = ToolCardState::Done {
                    status,
                    duration_ms,
                    exit_code,
                };
                if status != ToolStatus::Succeeded && !tail.is_empty() {
                    card.tail = Some(bound_tail(&tail));
                }
                return;
            }
        }
        // 未找到（理论上不会：ToolStarted 先于 ToolCompleted）——兜底追加终态卡片。
        self.transcript.push(Entry::Tool(ToolCard {
            id,
            name,
            detail: None,
            state: ToolCardState::Done {
                status,
                duration_ms,
                exit_code,
            },
            tail: if status == ToolStatus::Succeeded {
                None
            } else {
                Some(bound_tail(&tail))
            },
        }));
        self.trim_transcript();
    }

    /// 有界转录（防止长会话内存无限增长）。
    fn trim_transcript(&mut self) {
        if self.transcript.len() > 2000 {
            let keep = self.transcript.len() - 2000;
            self.transcript.drain(..keep);
        }
    }

    /// 向历史滚动。滚动量是离底部的距离，因而新输出可以自然回到底部。
    pub fn scroll_up(&mut self, lines: u16) {
        self.transcript_scroll = self.transcript_scroll.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        self.transcript_scroll = self.transcript_scroll.saturating_sub(lines);
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
        let items: Vec<(&'static str, &'static str)> = crate::tui::SLASH_COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(rest))
            .copied()
            .collect();
        if items.is_empty() {
            return;
        }
        self.menu = Some(MenuView {
            selected: selected.min(items.len() - 1),
            items,
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
        self.input = format!("/{name}");
        self.input_cursor = self.input.len();
        self.refresh_command_menu();
    }
}

/// 失败 tail 有界化（§16.2：保留关键 tail，不自动展开几十屏）。
fn bound_tail(tail: &str) -> String {
    const MAX_CHARS: usize = 240;
    if tail.chars().count() <= MAX_CHARS {
        return tail.to_string();
    }
    let mut out = String::from("…");
    for ch in tail.chars().rev().take(MAX_CHARS) {
        out.push(ch);
    }
    out
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
        let Entry::Message(line) = &view.transcript[0] else {
            panic!("assistant delta 必须是消息条目");
        };
        assert_eq!(line.text, "你好");
    }

    #[test]
    fn stream_delta_bumps_version_for_render_cache() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "a");
        let v1 = match &view.transcript[0] {
            Entry::Message(line) => line.version,
            _ => panic!(),
        };
        view.push_stream_delta(LineKind::Assistant, "b");
        let v2 = match &view.transcript[0] {
            Entry::Message(line) => line.version,
            _ => panic!(),
        };
        assert_ne!(v1, v2);
    }

    #[test]
    fn new_output_returns_to_follow_mode() {
        let mut view = ViewModel::default();
        view.scroll_up(20);
        view.push_stream_delta(LineKind::Assistant, "new");
        assert_eq!(view.transcript_scroll, 0);
    }

    #[test]
    fn tool_card_lifecycle_matches_by_call_id() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "run", Some("run: cargo test".into()));
        view.begin_tool("call-2", "read", Some("read src/main.rs".into()));
        view.finish_tool(
            "call-1",
            "run",
            ToolStatus::Failed,
            1234,
            Some(2),
            "exit_code: 1\n错误详情",
        );
        let Entry::Tool(card) = &view.transcript[0] else {
            panic!("call-1 必须是卡片");
        };
        assert_eq!(card.name, "run");
        assert_eq!(
            card.detail.as_deref(),
            Some("run: cargo test"),
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
        let Entry::Tool(card2) = &view.transcript[1] else {
            panic!("call-2 必须是卡片");
        };
        assert_eq!(card2.state, ToolCardState::Running);
    }

    #[test]
    fn success_card_has_no_tail() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "run", None);
        view.finish_tool(
            "call-1",
            "run",
            ToolStatus::Succeeded,
            42,
            Some(0),
            "大量输出",
        );
        let Entry::Tool(card) = &view.transcript[0] else {
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
        view.begin_tool("c", "run", None);
        view.finish_tool("c", "run", ToolStatus::Failed, 0, None, "x".repeat(10_000));
        let Entry::Tool(card) = &view.transcript[0] else {
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
        let view = ViewModel::default();
        assert!(view.reasoning_visible);
    }
}
