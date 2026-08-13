//! 键位映射（TPI 成熟化：完整 TOML keymap）。
//!
//! 键位不再硬编码在 reducer：`[ui.keymap]` 可覆盖任意动作的绑定，
//! 未配置的动作保持内建默认（与迁移前行为完全一致）。
//!
//! - `KeyChord = (KeyCode, 规范化修饰键)` 作为绑定键；仅保留
//!   SHIFT/CONTROL/ALT（SUPER/HYPER/META 不参与绑定）。
//! - 解析语法：`"ctrl+shift+enter"`、`"alt+t"`、`"f3"`、`"space"`，
//!   修饰键与按键小写、`+` 分隔、顺序任意；单字符 token 视为字符键。
//! - 未命中精确绑定且是无修饰的字符键（允许 Shift）→ 默认插入该字符；
//!   其余未绑定按键被忽略（与迁移前 `_ => {}` 一致）。

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 一个可绑定动作（reducer 的按键语义全集；名称同时是 `[ui.keymap]` 的 key）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    /// 提交输入（Enter；命令菜单打开时先补全选中项）。
    Submit,
    /// 插入换行（Alt+Enter / Shift+Enter / Ctrl+J 等价语义）。
    InsertNewline,
    /// 命令菜单循环补全（无菜单时无操作）。
    MenuNext,
    /// 分级关闭：Overlay > Modal > Menu > 取消 run > 清空输入。
    Escape,
    /// 退格删除（字符边界）。
    Backspace,
    /// 删除光标处字符。
    Delete,
    /// 光标左移（字符边界）。
    MoveLeft,
    /// 光标右移（字符边界）。
    MoveRight,
    /// 按词左移。
    MoveWordLeft,
    /// 按词右移。
    MoveWordRight,
    /// 当前 logical line 行首（单行 = 全文开头）。
    LineStart,
    /// 当前 logical line 行尾。
    LineEnd,
    /// 上移：菜单/Modal/Overlay 导航优先，否则多行光标上移，
    /// 到顶后进入 prompt 历史。
    MoveUp,
    /// 下移（对称语义）。
    MoveDown,
    /// 上翻页（Modal/Overlay 内部滚动优先，否则 transcript 翻页）。
    PageUp,
    /// 下翻页（对称语义）。
    PageDown,
    /// 跳到 transcript 最顶部（Ctrl+Home）。
    JumpTranscriptTop,
    /// 返回 follow-tail 最新（Ctrl+End）。
    FollowTail,
    /// 跳到上一条用户消息（Alt+Up）。
    JumpPrevUserTurn,
    /// 跳到下一条用户消息（Alt+Down）。
    JumpNextUserTurn,
    /// 复制选中文本到剪贴板（Ctrl+C 只做复制；无选区静默忽略）。
    Copy,
    /// 退出 TPI（Ctrl+D；§用户诉求：退出与复制分离）。
    QuitApp,
    /// 清空输入（Ctrl+U）。
    ClearInput,
    /// 删除光标前一个词（Ctrl+W）。
    DeleteWordBack,
    /// 删除光标到当前 logical line 行尾（Ctrl+K）。
    DeleteToLineEnd,
    /// 打开 transcript 搜索（Ctrl+F）。
    OpenSearch,
    /// 切换 reasoning 显示（Alt+T）。
    ToggleReasoning,
    /// 打开最近一张工具卡片详情（Alt+E）。
    OpenLastTool,
    /// 打开最近失败的工具卡片详情（Alt+O）。
    OpenFailedTool,
    /// 循环到上一张工具卡片 Overlay（Alt+[）。
    CycleToolPrev,
    /// 循环到下一张工具卡片 Overlay（Alt+]）。
    CycleToolNext,
    /// 撤销上一次编辑（Ctrl+Z）。
    Undo,
    /// 重做撤销的编辑（Ctrl+Y）。
    Redo,
    /// 切换右侧边栏（§用户诉求；默认 Ctrl+B）。
    ToggleSidebar,
    /// 插入一个字符（普通字符键的动态回退；不入绑定表）。
    TypedChar(char),
}

impl KeyAction {
    /// 配置名（kebab-case；`[ui.keymap]` 的 key）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::InsertNewline => "insert_newline",
            Self::MenuNext => "menu_next",
            Self::Escape => "escape",
            Self::Backspace => "backspace",
            Self::Delete => "delete",
            Self::MoveLeft => "move_left",
            Self::MoveRight => "move_right",
            Self::MoveWordLeft => "move_word_left",
            Self::MoveWordRight => "move_word_right",
            Self::LineStart => "line_start",
            Self::LineEnd => "line_end",
            Self::MoveUp => "move_up",
            Self::MoveDown => "move_down",
            Self::PageUp => "page_up",
            Self::PageDown => "page_down",
            Self::JumpTranscriptTop => "jump_transcript_top",
            Self::FollowTail => "follow_tail",
            Self::JumpPrevUserTurn => "jump_prev_user_turn",
            Self::JumpNextUserTurn => "jump_next_user_turn",
            Self::Copy => "copy",
            Self::QuitApp => "quit",
            Self::ClearInput => "clear_input",
            Self::DeleteWordBack => "delete_word_back",
            Self::DeleteToLineEnd => "delete_to_line_end",
            Self::OpenSearch => "open_search",
            Self::ToggleReasoning => "toggle_reasoning",
            Self::OpenLastTool => "open_last_tool",
            Self::OpenFailedTool => "open_failed_tool",
            Self::CycleToolPrev => "cycle_tool_prev",
            Self::CycleToolNext => "cycle_tool_next",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::ToggleSidebar => "toggle_sidebar",
            Self::TypedChar(_) => "insert_char",
        }
    }

    /// 从配置名解析（未知名返回 None）。
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "submit" => Self::Submit,
            "insert_newline" => Self::InsertNewline,
            "menu_next" => Self::MenuNext,
            "escape" => Self::Escape,
            "backspace" => Self::Backspace,
            "delete" => Self::Delete,
            "move_left" => Self::MoveLeft,
            "move_right" => Self::MoveRight,
            "move_word_left" => Self::MoveWordLeft,
            "move_word_right" => Self::MoveWordRight,
            "line_start" => Self::LineStart,
            "line_end" => Self::LineEnd,
            "move_up" => Self::MoveUp,
            "move_down" => Self::MoveDown,
            "page_up" => Self::PageUp,
            "page_down" => Self::PageDown,
            "jump_transcript_top" => Self::JumpTranscriptTop,
            "follow_tail" => Self::FollowTail,
            "jump_prev_user_turn" => Self::JumpPrevUserTurn,
            "jump_next_user_turn" => Self::JumpNextUserTurn,
            // 兼容旧配置名（此前 Ctrl+C 的复合语义名）。
            "copy" | "copy_or_interrupt" => Self::Copy,
            "quit" | "quit_app" => Self::QuitApp,
            "clear_input" => Self::ClearInput,
            "delete_word_back" => Self::DeleteWordBack,
            "delete_to_line_end" => Self::DeleteToLineEnd,
            "open_search" => Self::OpenSearch,
            "toggle_reasoning" => Self::ToggleReasoning,
            "open_last_tool" => Self::OpenLastTool,
            "open_failed_tool" => Self::OpenFailedTool,
            "cycle_tool_prev" => Self::CycleToolPrev,
            "cycle_tool_next" => Self::CycleToolNext,
            "undo" => Self::Undo,
            "redo" => Self::Redo,
            "toggle_sidebar" => Self::ToggleSidebar,
            _ => return None,
        })
    }
}

/// 规范化按键：保留 SHIFT/CONTROL/ALT 修饰位。
fn normalized_mods(mods: KeyModifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    if mods.contains(KeyModifiers::SHIFT) {
        out |= KeyModifiers::SHIFT;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        out |= KeyModifiers::CONTROL;
    }
    if mods.contains(KeyModifiers::ALT) {
        out |= KeyModifiers::ALT;
    }
    out
}

/// 解析 "ctrl+shift+enter" 式按键字符串。
///
/// 修饰键 token：ctrl/control、alt、shift、super、meta（大小写不敏感）；
/// 按键 token：单字符、enter、esc/escape、tab、backtab、backspace、
/// space、home、end、up/down/left/right、pageup/pagedown、delete、
/// insert、f1..f12。未知 token 返回 None。
pub fn parse_key(spec: &str) -> Option<KeyEvent> {
    let mut mods = KeyModifiers::NONE;
    let mut parts: Vec<&str> = spec.split('+').map(str::trim).collect();
    if parts.is_empty() {
        return None;
    }
    let last = parts.pop()?.to_ascii_lowercase();
    let code = match last.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            KeyCode::F(last.as_str()[1..].parse::<u8>().ok()?)
        }
        _ => {
            let mut chars = last.chars();
            let c = chars.next()?;
            if chars.next().is_some() || c == '+' {
                // 多字符或 '+' 本身不是合法按键 token。
                return None;
            }
            KeyCode::Char(c)
        }
    };
    for part in parts {
        let p = part.to_ascii_lowercase();
        match p.as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "option" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "super" | "meta" | "win" => {}
            _ => return None,
        }
    }
    Some(KeyEvent::new(code, mods))
}

/// 反向格式化（`/settings` 展示与文档用）。
pub fn format_key(key: KeyEvent) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    let code = match key.code {
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        _ => return format!("{key:?}"),
    };
    if parts.is_empty() {
        code
    } else {
        parts.push(&code);
        parts.join("+")
    }
}

/// 键位映射表：`(code, mods) → action`。
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: HashMap<(KeyCode, KeyModifiers), KeyAction>,
}

impl Keymap {
    /// 内建默认绑定（与 TUI v2 迁移前的硬编码完全一致，另加 Undo/Redo）。
    /// 命名 `builtin` 避免与 `Default::default()`（空绑定表）混淆。
    pub fn builtin() -> Self {
        let mut km = Keymap {
            bindings: HashMap::new(),
        };
        let mut bind = |spec: &str, action: KeyAction| {
            if let Some(key) = parse_key(spec) {
                km.bindings
                    .insert((key.code, normalized_mods(key.modifiers)), action);
            }
        };
        bind("enter", KeyAction::Submit);
        bind("alt+enter", KeyAction::InsertNewline);
        bind("shift+enter", KeyAction::InsertNewline);
        bind("ctrl+j", KeyAction::InsertNewline);
        bind("tab", KeyAction::MenuNext);
        bind("esc", KeyAction::Escape);
        bind("backspace", KeyAction::Backspace);
        bind("delete", KeyAction::Delete);
        bind("left", KeyAction::MoveLeft);
        bind("right", KeyAction::MoveRight);
        bind("alt+left", KeyAction::MoveWordLeft);
        bind("alt+right", KeyAction::MoveWordRight);
        bind("home", KeyAction::LineStart);
        bind("end", KeyAction::LineEnd);
        bind("ctrl+home", KeyAction::JumpTranscriptTop);
        bind("ctrl+end", KeyAction::FollowTail);
        bind("alt+up", KeyAction::JumpPrevUserTurn);
        bind("alt+down", KeyAction::JumpNextUserTurn);
        bind("up", KeyAction::MoveUp);
        bind("down", KeyAction::MoveDown);
        bind("pageup", KeyAction::PageUp);
        bind("pagedown", KeyAction::PageDown);
        bind("ctrl+c", KeyAction::Copy);
        // §用户诉求：退出用 Ctrl+D（Ctrl+C 只负责复制）。
        bind("ctrl+d", KeyAction::QuitApp);
        bind("ctrl+u", KeyAction::ClearInput);
        bind("ctrl+a", KeyAction::LineStart);
        bind("ctrl+e", KeyAction::LineEnd);
        bind("ctrl+w", KeyAction::DeleteWordBack);
        bind("ctrl+k", KeyAction::DeleteToLineEnd);
        bind("ctrl+f", KeyAction::OpenSearch);
        bind("alt+t", KeyAction::ToggleReasoning);
        bind("alt+e", KeyAction::OpenLastTool);
        bind("alt+o", KeyAction::OpenFailedTool);
        bind("alt+[", KeyAction::CycleToolPrev);
        bind("alt+]", KeyAction::CycleToolNext);
        // TPI 成熟化：编辑器撤销/重做（此前未映射）。
        bind("ctrl+z", KeyAction::Undo);
        bind("ctrl+y", KeyAction::Redo);
        // §用户诉求：右侧边栏（todo + 用户消息大纲）。
        bind("ctrl+b", KeyAction::ToggleSidebar);
        km
    }

    /// 按键 → 动作。未命中精确绑定但可输入的字符键（无修饰或仅 Shift）
    /// 视为直接插入；其余按键返回 None（忽略）。
    pub fn action(&self, key: KeyEvent) -> Option<KeyAction> {
        let mods = normalized_mods(key.modifiers);
        if let Some(action) = self.bindings.get(&(key.code, mods)) {
            return Some(*action);
        }
        if let KeyCode::Char(c) = key.code {
            // Windows/国际键盘的 AltGr 通常上报为 Ctrl+Alt。精确自定义绑定
            // 仍优先；未绑定但终端已给出字符时应插入，不能静默吞掉 @/€ 等。
            let alt_gr = mods.contains(KeyModifiers::CONTROL) && mods.contains(KeyModifiers::ALT);
            let printable = mods == KeyModifiers::NONE || mods == KeyModifiers::SHIFT || alt_gr;
            if printable {
                return Some(KeyAction::insert_char(c));
            }
        }
        None
    }

    /// 是否存在精确绑定（含默认），供 `action` 之外的中断式判断用。
    pub fn has_binding(&self, key: KeyEvent) -> bool {
        self.bindings
            .contains_key(&(key.code, normalized_mods(key.modifiers)))
    }

    /// 某动作是否至少有 1 个绑定（P0-5 关键键校验）。
    pub fn has_action(&self, action: KeyAction) -> bool {
        self.bindings.values().any(|a| *a == action)
    }

    /// 某动作的全部绑定（格式化；/doctor 展示用）。
    pub fn keys_for(&self, action: KeyAction) -> String {
        let mut keys: Vec<String> = self
            .bindings
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|((code, mods), _)| format_key(KeyEvent::new(*code, *mods)))
            .collect();
        keys.sort();
        keys.join(" / ")
    }

    /// P0-5：关键动作必须各有至少一个绑定（否则用户可能无法提交/换行/退出）。
    ///
    /// 覆盖语义会移除动作的原默认键——若用户误配置导致 `submit`/`insert_newline`/
    /// `escape` 完全没有绑定，这里补回内置默认键并返回被恢复的动作名。
    /// 检查顺序：escape（退出/取消最重要）> submit > insert_newline。
    /// 被占用的默认键会被重新绑定（关键动作优先于误配的冲突绑定）。
    pub fn ensure_critical_bindings(&mut self) -> Vec<&'static str> {
        let defaults = Keymap::builtin();
        let mut restored: Vec<&'static str> = Vec::new();
        for action in [
            KeyAction::Escape,
            KeyAction::Submit,
            KeyAction::InsertNewline,
        ] {
            if self.has_action(action) {
                continue;
            }
            let default_keys: Vec<(KeyCode, KeyModifiers)> = defaults
                .bindings
                .iter()
                .filter(|(_, a)| **a == action)
                .map(|((code, mods), _)| (*code, *mods))
                .collect();
            for chord in default_keys {
                self.bindings.insert(chord, action);
            }
            restored.push(action.name());
        }
        restored
    }

    /// 从 `[ui.keymap]` 表构建：以默认绑定为基底，覆盖项替换动作的全部绑定。
    ///
    /// 覆盖语义：用户为某动作指定新键后，该动作的原默认键一并移除
    /// （避免同一动作多键导致 Esc 等高频键歧义）；键冲突时后者覆盖前者
    /// 并记日志（P0-5：冲突提示）。
    /// 未知动作名/非法按键字符串跳过并记日志，不阻断启动。
    ///
    /// P0-5：构建后校验关键动作（submit/insert_newline/escape）——缺失时
    /// 补回内置默认键并记警告，杜绝"普通 Enter 不再提交 / 无法换行 / 无法退出"。
    pub fn from_config(table: &toml::Table) -> Self {
        let mut km = Keymap::builtin();
        for (action_name, value) in table {
            let Some(action) = KeyAction::from_name(action_name) else {
                tracing::warn!(action = %action_name, "tui keymap: 未知动作名，已忽略");
                continue;
            };
            let keys: Vec<&str> = match value {
                toml::Value::String(s) => vec![s.as_str()],
                toml::Value::Array(items) => {
                    items.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
                }
                _ => {
                    tracing::warn!(action = %action_name, "tui keymap: 绑定值必须是字符串或字符串数组，已忽略");
                    continue;
                }
            };
            if keys.is_empty() {
                continue;
            }
            // 先解析全部新键：至少一个合法才替换（全部非法时保留默认绑定，
            // 不因误配置丢失可用键）。
            let mut parsed: Vec<KeyEvent> = Vec::with_capacity(keys.len());
            for spec in keys {
                let Some(key) = parse_key(spec) else {
                    tracing::warn!(action = %action_name, spec, "tui keymap: 非法按键，已忽略");
                    continue;
                };
                parsed.push(key);
            }
            if parsed.is_empty() {
                continue;
            }
            // 移除该动作现有绑定（默认键先清掉），再逐一添加新键。
            km.bindings.retain(|_, a| *a != action);
            for key in parsed {
                let chord = (key.code, normalized_mods(key.modifiers));
                // P0-5：冲突提示——该键已被其他动作占用（后者覆盖前者，但告知用户）。
                if let Some(other) = km.bindings.get(&chord)
                    && *other != action
                {
                    tracing::warn!(
                        action = %action_name,
                        key = %format_key(key),
                        occupied_by = %other.name(),
                        "tui keymap: 按键冲突，新绑定覆盖原动作"
                    );
                }
                km.bindings.insert(chord, action);
            }
        }
        // P0-5：关键绑定兜底（缺失补默认键）。
        let restored = km.ensure_critical_bindings();
        for action in restored {
            tracing::warn!(action, "tui keymap: 关键动作没有绑定，已回退内置默认键");
        }
        km
    }

    /// 全部精确绑定（`/settings` 展示用）：按动作名分组。
    pub fn display_bindings(&self) -> Vec<(&'static str, String)> {
        let mut by_action: HashMap<KeyAction, Vec<String>> = HashMap::new();
        for ((code, mods), action) in &self.bindings {
            let key = KeyEvent::new(*code, *mods);
            by_action.entry(*action).or_default().push(format_key(key));
        }
        let mut out: Vec<(&'static str, String)> = by_action
            .into_iter()
            .map(|(action, mut keys)| {
                keys.sort();
                (action.name(), keys.join(" / "))
            })
            .collect();
        out.sort_by_key(|(name, _)| *name);
        out
    }
}

/// `InsertChar` 是动态回退动作，不入绑定表；为穷尽 match 提供内部表示。
impl KeyAction {
    pub(crate) fn insert_char(c: char) -> Self {
        KeyAction::TypedChar(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &str) -> KeyEvent {
        parse_key(spec).expect("test key spec must parse")
    }

    #[test]
    fn parse_roundtrips_named_and_modified_keys() {
        assert_eq!(format_key(key("ctrl+shift+enter")), "ctrl+shift+enter");
        assert_eq!(format_key(key("alt+t")), "alt+t");
        assert_eq!(format_key(key("f3")), "f3");
        assert_eq!(format_key(key("space")), "space");
        assert_eq!(format_key(key("pageup")), "pageup");
        assert_eq!(format_key(key("esc")), "esc");
        assert_eq!(format_key(key("ctrl+home")), "ctrl+home");
        assert_eq!(format_key(key("alt+]")), "alt+]");
        assert_eq!(
            format_key(key("ctrl+alt+shift+delete")),
            "ctrl+alt+shift+delete"
        );
    }

    #[test]
    fn parse_rejects_invalid_specs() {
        assert!(parse_key("").is_none());
        assert!(parse_key("ctrl+").is_none());
        assert!(parse_key("ctrl+bogus").is_none());
        assert!(parse_key("boguskey").is_none());
        assert!(parse_key("f99").is_none());
        assert!(parse_key("ctrl+longword").is_none());
    }

    #[test]
    fn default_bindings_cover_core_actions() {
        let km = Keymap::builtin();
        assert_eq!(km.action(key("enter")), Some(KeyAction::Submit));
        assert_eq!(
            km.action(key("shift+enter")),
            Some(KeyAction::InsertNewline)
        );
        assert_eq!(km.action(key("ctrl+f")), Some(KeyAction::OpenSearch));
        assert_eq!(km.action(key("esc")), Some(KeyAction::Escape));
        // §用户诉求：复制 Ctrl+C、退出 Ctrl+D，语义分离。
        assert_eq!(km.action(key("ctrl+c")), Some(KeyAction::Copy));
        assert_eq!(km.action(key("ctrl+d")), Some(KeyAction::QuitApp));
        assert_eq!(KeyAction::Copy.name(), "copy", "配置名必须稳定");
        assert_eq!(KeyAction::QuitApp.name(), "quit", "配置名必须稳定");
        assert_eq!(km.action(key("ctrl+z")), Some(KeyAction::Undo));
        assert_eq!(km.action(key("ctrl+y")), Some(KeyAction::Redo));
        // §用户诉求：右侧边栏默认 Ctrl+B 绑定。
        assert_eq!(km.action(key("ctrl+b")), Some(KeyAction::ToggleSidebar));
        assert_eq!(
            KeyAction::ToggleSidebar.name(),
            "toggle_sidebar",
            "配置名必须稳定"
        );
        // 普通字符回退为插入。
        assert_eq!(km.action(key("h")), Some(KeyAction::TypedChar('h')));
        assert_eq!(km.action(key("ctrl+h")), None, "Ctrl+h 未绑定则应忽略");
    }

    #[test]
    fn unbound_altgr_character_is_inserted() {
        let km = Keymap::builtin();
        let altgr_at = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(km.action(altgr_at), Some(KeyAction::TypedChar('@')));
    }

    #[test]
    fn custom_bindings_replace_defaults() {
        let table: toml::Table = toml::from_str(
            r#"
            submit = "ctrl+enter"
            move_up = ["k", "ctrl+p"]
            "#,
        )
        .unwrap();
        let km = Keymap::from_config(&table);
        // 自定义键生效。
        assert_eq!(km.action(key("ctrl+enter")), Some(KeyAction::Submit));
        assert_eq!(km.action(key("k")), Some(KeyAction::MoveUp));
        assert_eq!(km.action(key("ctrl+p")), Some(KeyAction::MoveUp));
        // 原默认键被替换移除。
        assert_eq!(km.action(key("enter")), None);
        assert_eq!(km.action(key("up")), None);
        // 未覆盖的动作保留默认。
        assert_eq!(km.action(key("esc")), Some(KeyAction::Escape));
    }

    #[test]
    fn invalid_custom_bindings_are_skipped() {
        let table: toml::Table =
            toml::from_str("no_such_action = \"ctrl+x\"\nsubmit = \"ctrl+??\"").unwrap();
        let km = Keymap::from_config(&table);
        // 无效项不阻断，默认绑定保留。
        assert_eq!(km.action(key("enter")), Some(KeyAction::Submit));
    }

    #[test]
    fn display_bindings_lists_defaults() {
        let km = Keymap::builtin();
        let shown = km.display_bindings();
        assert!(shown.iter().any(|(name, _)| *name == "open_search"));
        let (_, keys) = shown.iter().find(|(n, _)| *n == "open_search").unwrap();
        assert_eq!(keys, "ctrl+f");
    }

    /// P0-5：关键动作（submit/insert_newline/escape）缺失绑定 → 补回默认键。
    /// 构造：escape 被 submit 的键冲突覆盖后无绑定，ensure 必须恢复 esc。
    #[test]
    fn critical_bindings_restored_when_overridden_away() {
        // `submit = "esc"` 会移除 escape 的默认键并占用 esc：
        // 覆盖语义下 escape 无绑定（用户无法退出/取消 run）。
        let table: toml::Table = toml::from_str("submit = \"esc\"").unwrap();
        let km = Keymap::from_config(&table);
        // submit 有绑定（esc 被它占用），不缺失；但 escape 必须先于 submit 恢复。
        assert!(
            km.has_action(KeyAction::Submit),
            "submit 仍应可用（esc 已被它占用）"
        );
        assert!(
            km.has_action(KeyAction::Escape),
            "escape 必须被兜底恢复（不能没有退出键）"
        );
        assert_eq!(
            km.action(key("esc")),
            Some(KeyAction::Escape),
            "esc 应恢复为 escape（关键动作优先于冲突占用）"
        );
        // submit 的兜底键（enter）也应恢复——不能把 submit 挤没。
        assert_eq!(km.action(key("enter")), Some(KeyAction::Submit));
        assert!(km.has_action(KeyAction::InsertNewline));
    }

    /// P0-5：insert_newline 被覆盖移除后必须补回默认换行键。
    #[test]
    fn insert_newline_restored_when_config_removes_it() {
        // 配置把 insert_newline 换成与 submit 相同的键 → 后者覆盖前者 →
        // insert_newline 无绑定（用户无法换行）。
        let table: toml::Table =
            toml::from_str("insert_newline = \"enter\"\nsubmit = \"enter\"").unwrap();
        let km = Keymap::from_config(&table);
        assert!(
            km.has_action(KeyAction::InsertNewline),
            "insert_newline 必须被兜底恢复（否则无法换行）"
        );
        assert_eq!(
            km.action(key("shift+enter")),
            Some(KeyAction::InsertNewline),
            "shift+enter 默认换行键应恢复"
        );
    }

    /// P0-5：keys_for / has_action 供 doctor 展示关键绑定。
    #[test]
    fn keys_for_lists_bindings_for_action() {
        let km = Keymap::builtin();
        assert_eq!(km.keys_for(KeyAction::Submit), "enter");
        assert!(km.keys_for(KeyAction::Escape).contains("esc"));
        assert!(
            km.keys_for(KeyAction::InsertNewline)
                .contains("shift+enter")
        );
        assert!(km.has_action(KeyAction::Escape));
    }
}
