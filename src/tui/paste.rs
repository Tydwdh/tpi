//! 粘贴处理：bracketed paste 序列解析、换行规范化与大文本占位符。
//!
//! **主路径（支持 bracketed paste 的终端）**：`EnableBracketedPaste` 下，
//! 终端把粘贴注入为 `\x1b[200~ … \x1b[201~` 按键流。crossterm Windows 后端
//! 不解析该序列（parse.rs 逐键产生 `KeyEvent`），因此 app 键盘线程按
//! `Esc + '['` 检测粘贴开始、消费前缀 `[200~`、把内容按键全部缓冲，直到
//! `[201~` 结束，整段作为一次 `Event::Paste` 上屏——不依赖间隔猜测、不读
//! 剪贴板，行尾 Enter 经 [`paste_char`] 转 `\n`，不会命中 `submit`。
//! 空闲超时 [`PASTE_IDLE_TIMEOUT`] 见底异常/误判。
//!
//! 普通粘贴优先接受确定信号：crossterm 的 `Event::Paste`、应用直读剪贴板，或
//! 完整的 bracketed-paste 控制序列。旧终端若只能逐键注入，则使用一个刻意受限
//! 的兼容兜底：达到长文本阈值前字符仍立即上屏，只把紧跟可插入字符的普通
//! Enter 改写成 Shift+Enter；达到阈值后，把已显示的精确后缀原子折叠为占位符，
//! 仅将余下尾部收进旁路全文。普通输入不等待、不回溯，不增加可感知延迟。

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// bracketed paste 在初始 `Esc [` 之后剩余的前缀，以及完整结束尾串。
pub const BRACKETED_START_TAIL: [char; 4] = ['2', '0', '0', '~'];
pub const BRACKETED_END_TAIL: [char; 5] = ['[', '2', '0', '1', '~'];

/// Esc 后探测下一键的窗口：`[`（bracketed paste 前缀 `\x1b[200~` 的第 2 键）
/// 与结束序列 `\x1b[201~`（Esc 后紧接 `[,2,0,1,~`）。终端逐键注入时序列内
/// 控制序列由终端作为同一批输入写入；2ms 足以跨过调度抖动，同时让普通
/// Esc（取消/关闭）保持近乎即时，不再承担原来的 40ms 固定延迟。
pub const BRACKETED_PROBE_GAP: Duration = Duration::from_millis(2);
/// bracketed paste 内容收集的空闲超时：连续无新键这么久即视为粘贴流结束
/// （终端异常/误判兜底；真粘贴批间间隔远小于此）。
pub const PASTE_IDLE_TIMEOUT: Duration = Duration::from_millis(500);

/// 旧终端逐键粘贴的 Enter 保护窗口。
///
/// 与 Gemini CLI 的兼容策略一致：普通 Enter 若紧跟可插入字符到达，就只将这个
/// Enter 解释为换行。窗口只影响 Enter，不延迟或吞掉任何字符。
pub const FAST_PASTE_ENTER_GAP: Duration = Duration::from_millis(30);

/// 旧终端无 bracketed-paste 时的最小兜底状态。
///
/// 时间戳在前一个键成功送入 UI channel 后刷新，避免长粘贴造成 channel 背压时
/// 把本来连续的“字符 + Enter”错误地量成很大的间隔。
#[derive(Debug, Default)]
pub struct FastPasteEnterGuard {
    last_insertable_forwarded_at: Option<Instant>,
    candidate: String,
    collapsed_initial: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedKeyStreamPaste {
    pub initial_text: String,
    pub full_text: String,
}

impl FastPasteEnterGuard {
    /// 只改写会触发提交的无修饰 Enter；其他键原样返回。
    ///
    /// 返回值中的 `bool` 供 [`Self::record_forwarded`] 区分被保护的 Enter，
    /// 从而连续空行也能保持为粘贴内容。
    pub fn rewrite_key(&self, mut key: KeyEvent, now: Instant) -> (KeyEvent, bool) {
        let is_plain_enter = matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && key.code == KeyCode::Enter
            && key.modifiers == KeyModifiers::NONE;
        let follows_insertable = self.last_insertable_forwarded_at.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed <= FAST_PASTE_ENTER_GAP)
        });
        let protected = is_plain_enter && follows_insertable;
        if protected {
            key.modifiers = KeyModifiers::SHIFT;
        }
        (key, protected)
    }

    /// 在键成功送入 UI channel 后记录状态；普通字符与已保护 Enter 延续窗口。
    pub fn record_forwarded(
        &mut self,
        key: &KeyEvent,
        protected_enter: bool,
        now: Instant,
    ) -> Option<String> {
        let Some(ch) = key_stream_char(key, protected_enter) else {
            self.clear();
            return None;
        };
        let continues = self.last_insertable_forwarded_at.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed <= FAST_PASTE_ENTER_GAP)
        });
        if !continues {
            self.candidate.clear();
        }
        self.candidate.push(ch);
        self.last_insertable_forwarded_at = Some(now);
        if self.collapsed_initial.is_none() && is_large_paste(&self.candidate) {
            self.collapsed_initial = Some(self.candidate.clone());
            return Some(self.candidate.clone());
        }
        None
    }

    /// 达到阈值后，后续同一逐键流不再送进 Editor，而是只追加到旁路全文。
    pub fn capture_if_collapsed(
        &mut self,
        key: &KeyEvent,
        protected_enter: bool,
        now: Instant,
    ) -> bool {
        if self.collapsed_initial.is_none() {
            return false;
        }
        let Some(ch) = key_stream_char(key, protected_enter) else {
            return false;
        };
        let continues = self.last_insertable_forwarded_at.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed <= FAST_PASTE_ENTER_GAP)
        });
        if !continues {
            return false;
        }
        self.candidate.push(ch);
        self.last_insertable_forwarded_at = Some(now);
        true
    }

    pub fn is_collapsed(&self) -> bool {
        self.collapsed_initial.is_some()
    }

    /// 折叠事件本身可能因 UI channel 背压短暂阻塞；以成功投递完成时为新的
    /// 连续流基线，避免把同一长粘贴的下一字符误判为新输入。
    pub fn mark_delivery_complete(&mut self, now: Instant) {
        if self.last_insertable_forwarded_at.is_some() {
            self.last_insertable_forwarded_at = Some(now);
        }
    }

    /// 新事件到达前结束已经空闲的逐键粘贴；普通候选流只清理状态。
    pub fn finish_if_stale(&mut self, now: Instant) -> Option<FinishedKeyStreamPaste> {
        let stale = self.last_insertable_forwarded_at.is_some_and(|last| {
            now.checked_duration_since(last)
                .is_none_or(|elapsed| elapsed > FAST_PASTE_ENTER_GAP)
        });
        if stale { self.finish() } else { None }
    }

    /// 强制结束当前流；若已经折叠，必须把阈值后的完整尾部交回 reducer。
    pub fn finish(&mut self) -> Option<FinishedKeyStreamPaste> {
        let initial_text = self.collapsed_initial.take();
        let full_text = std::mem::take(&mut self.candidate);
        self.last_insertable_forwarded_at = None;
        initial_text.map(|initial_text| FinishedKeyStreamPaste {
            initial_text,
            full_text,
        })
    }

    /// 确定的 Paste、鼠标/Resize 或其他输入边界会终止旧的保护窗口。
    pub fn clear(&mut self) {
        self.last_insertable_forwarded_at = None;
        self.candidate.clear();
        self.collapsed_initial = None;
    }
}

fn key_stream_char(key: &KeyEvent, protected_enter: bool) -> Option<char> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let plain = !key.modifiers.intersects(
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::META,
    );
    match key.code {
        KeyCode::Char(ch) if plain => Some(ch),
        KeyCode::Enter if protected_enter => Some('\n'),
        KeyCode::Tab if key.modifiers == KeyModifiers::NONE => Some('\t'),
        _ => None,
    }
}

/// 大粘贴判定阈值（§用户诉求：大粘贴用占位符代替真实渲染）。
///
/// 超过 [`LARGE_PASTE_LINES`] 行（含换行）或达到 [`LARGE_PASTE_CHARS`] 字符时，
/// 真实内容不插入输入框，而是以 `[Pasted Text: N chars #id]` 占位符上屏，
/// 全文存入旁路（`UiState.pasted`），提交时一次性展开发送——避免大文本
/// 在输入框内分块渲染、被 `MAX_INPUT_BYTES` 截断、以及行尾 Enter 在
/// 批次边界被误判提交。阈值来自用户指定（5 行 / 300 字符，gemini-cli 同款）。
pub const LARGE_PASTE_LINES: usize = 5;
pub const LARGE_PASTE_CHARS: usize = 300;

/// 统一粘贴换行。Windows 剪贴板通常使用 CRLF；裸 CR 也按逻辑换行处理。
/// 保留 LF、Tab 与其余 Unicode 内容，避免 `\r` 进入编辑器后造成光标错位。
pub fn normalize_newlines(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 是否大粘贴：`text` 超过 5 行 或 ≥300 字符。
pub fn is_large_paste(text: &str) -> bool {
    let mut chars = 0usize;
    let mut lines = 1usize;
    for c in text.chars() {
        chars += 1;
        if c == '\n' {
            lines += 1;
        }
    }
    chars >= LARGE_PASTE_CHARS || lines > LARGE_PASTE_LINES
}

/// 为大粘贴生成简洁、可读且唯一的占位符。
///
/// 首个同长度内容显示为 `[Pasted Content N chars]`；只有同长度占位符重名时
/// 才追加 `#2`、`#3`。内部存储键不再暴露给用户。
pub fn next_paste_placeholder(pasted: &HashMap<String, String>, text: &str) -> String {
    let base = format!("[Pasted Content {} chars]", text.chars().count());
    if !pasted.contains_key(&base) {
        return base;
    }
    for suffix in 2usize.. {
        let candidate = format!("{base} #{suffix}");
        if !pasted.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded placeholder suffix iterator")
}

/// 提交时把文本中的大粘贴占位符展开为真实内容（§用户诉求：发送时一起发送）。
///
/// 占位符 id 在 `pasted` 中查不到（被清理等异常）时保留原样，不吞内容。
pub fn expand_paste_placeholders(text: &str, pasted: &HashMap<String, String>) -> String {
    if pasted.is_empty() {
        return text.to_owned();
    }
    // 长占位符优先，避免 `[...chars] #2` 先命中它的无后缀前缀。
    let mut placeholders = pasted.keys().collect::<Vec<_>>();
    placeholders.sort_unstable_by_key(|placeholder| std::cmp::Reverse(placeholder.len()));
    let pattern = placeholders
        .into_iter()
        .map(|placeholder| regex::escape(placeholder))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(regex) = regex::Regex::new(&pattern) else {
        return text.to_owned();
    };
    regex
        .replace_all(text, |caps: &regex::Captures| {
            pasted
                .get(&caps[0])
                .cloned()
                .unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

/// 无修饰 Esc（bracketed paste 前缀探测起点；粘贴内容不含修饰键）。
pub fn is_plain_escape(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.code == KeyCode::Esc
        && key.modifiers == KeyModifiers::NONE
}

/// bracketed paste 控制序列按字符匹配。终端注入的 `~` 等键可能携带 Shift，
/// 因此这里只检查 Press 与字符码。
pub fn is_sequence_char(key: &KeyEvent, expected: char) -> bool {
    key.kind == KeyEventKind::Press && key.code == KeyCode::Char(expected)
}

/// 按键 → 可插入字符；不可插入（非 Press / Ctrl+Alt 组合 / 特殊键）返回 `None`。
///
/// 洪流中只有本函数返回 `Some` 的键才算粘贴内容：字符原样、Enter → `\n`、
/// Tab → `\t`。返回 `None` 的键终止收集并按原样逐键转发。
pub fn paste_char(key: &KeyEvent) -> Option<char> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let mods = key.modifiers;
    let plain = !(mods.contains(KeyModifiers::CONTROL)
        || mods.contains(KeyModifiers::ALT)
        || mods.contains(KeyModifiers::SUPER)
        || mods.contains(KeyModifiers::META));
    match key.code {
        KeyCode::Char(c) if plain => Some(c),
        KeyCode::Enter if plain => Some('\n'),
        KeyCode::Tab if mods == KeyModifiers::NONE => Some('\t'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyModifiers};

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn paste_char_keeps_shift_case_and_tab() {
        assert_eq!(
            paste_char(&press(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some('G')
        );
        assert_eq!(
            paste_char(&press(KeyCode::Char('i'), KeyModifiers::NONE)),
            Some('i')
        );
        assert_eq!(
            paste_char(&press(KeyCode::Tab, KeyModifiers::NONE)),
            Some('\t')
        );
    }

    #[test]
    fn paste_char_maps_enter_to_newline() {
        // 洪流中的 Enter 转字面换行（不触发提交）。
        assert_eq!(
            paste_char(&press(KeyCode::Enter, KeyModifiers::NONE)),
            Some('\n')
        );
    }

    #[test]
    fn is_plain_escape_only_unmodified_esc() {
        assert!(is_plain_escape(&press(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!is_plain_escape(&press(
            KeyCode::Esc,
            KeyModifiers::CONTROL
        )));
        assert!(!is_plain_escape(&press(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert!(!is_plain_escape(&press(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn bracketed_sequences_include_the_bracket_and_are_exact() {
        assert_eq!(BRACKETED_START_TAIL, ['2', '0', '0', '~']);
        assert_eq!(BRACKETED_END_TAIL, ['[', '2', '0', '1', '~']);
        for expected in BRACKETED_END_TAIL {
            assert!(is_sequence_char(
                &press(KeyCode::Char(expected), KeyModifiers::SHIFT),
                expected
            ));
        }
        assert!(!is_sequence_char(
            &press(KeyCode::Char('0'), KeyModifiers::NONE),
            '1'
        ));
    }

    #[test]
    fn paste_char_rejects_modifier_and_special_keys() {
        // Ctrl 组合 / 特殊键不是粘贴内容：None 终止收集并逐键转发。
        assert_eq!(
            paste_char(&press(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            paste_char(&press(KeyCode::Enter, KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(paste_char(&press(KeyCode::Esc, KeyModifiers::NONE)), None);
        assert_eq!(paste_char(&press(KeyCode::Up, KeyModifiers::NONE)), None);
    }

    #[test]
    fn paste_char_rejects_repeat_events() {
        // Repeat/Release 是按住自动重复，不属于粘贴内容。
        let repeat =
            KeyEvent::new_with_kind(KeyCode::Char('b'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(paste_char(&repeat), None);
    }

    #[test]
    fn fast_plain_enter_is_rewritten_without_buffering_chars() {
        let mut guard = FastPasteEnterGuard::default();
        let t0 = Instant::now();
        let typed = press(KeyCode::Char('a'), KeyModifiers::NONE);
        let (unchanged, protected) = guard.rewrite_key(typed, t0);
        assert_eq!(unchanged, typed);
        assert!(!protected);
        guard.record_forwarded(&typed, false, t0);

        let enter = press(KeyCode::Enter, KeyModifiers::NONE);
        let (rewritten, protected) = guard.rewrite_key(enter, t0 + FAST_PASTE_ENTER_GAP);
        assert!(protected);
        assert_eq!(rewritten.code, KeyCode::Enter);
        assert_eq!(rewritten.modifiers, KeyModifiers::SHIFT);
    }

    #[test]
    fn slow_or_modified_enter_keeps_normal_semantics() {
        let mut guard = FastPasteEnterGuard::default();
        let t0 = Instant::now();
        let typed = press(KeyCode::Char('a'), KeyModifiers::NONE);
        guard.record_forwarded(&typed, false, t0);

        let plain_enter = press(KeyCode::Enter, KeyModifiers::NONE);
        let (slow, protected) = guard.rewrite_key(
            plain_enter,
            t0 + FAST_PASTE_ENTER_GAP + Duration::from_millis(1),
        );
        assert_eq!(slow, plain_enter);
        assert!(!protected);

        let shifted_enter = press(KeyCode::Enter, KeyModifiers::SHIFT);
        let (shifted, protected) = guard.rewrite_key(shifted_enter, t0 + Duration::from_millis(1));
        assert_eq!(shifted, shifted_enter);
        assert!(!protected);
    }

    #[test]
    fn protected_enter_keeps_blank_lines_safe_and_other_keys_clear_guard() {
        let mut guard = FastPasteEnterGuard::default();
        let t0 = Instant::now();
        let typed = press(KeyCode::Char('a'), KeyModifiers::NONE);
        guard.record_forwarded(&typed, false, t0);

        let enter = press(KeyCode::Enter, KeyModifiers::NONE);
        let (first, first_protected) = guard.rewrite_key(enter, t0 + Duration::from_millis(1));
        assert!(first_protected);
        guard.record_forwarded(&first, first_protected, t0 + Duration::from_millis(2));
        let (_, second_protected) = guard.rewrite_key(enter, t0 + Duration::from_millis(3));
        assert!(second_protected, "粘贴中的连续空行不能触发提交");

        guard.record_forwarded(
            &press(KeyCode::Left, KeyModifiers::NONE),
            false,
            t0 + Duration::from_millis(4),
        );
        let (_, protected_after_left) = guard.rewrite_key(enter, t0 + Duration::from_millis(5));
        assert!(!protected_after_left);
    }

    #[test]
    fn multiline_key_stream_collapses_at_six_lines_and_keeps_the_tail() {
        let mut guard = FastPasteEnterGuard::default();
        let start = Instant::now();
        let mut collapse = None;
        let mut tick = 0u64;
        for ch in "a\nb\nc\nd\ne\nf-tail".chars() {
            tick += 1;
            let now = start + Duration::from_millis(tick);
            let raw = if ch == '\n' {
                press(KeyCode::Enter, KeyModifiers::NONE)
            } else {
                press(KeyCode::Char(ch), KeyModifiers::NONE)
            };
            let (key, protected) = guard.rewrite_key(raw, now);
            if guard.capture_if_collapsed(&key, protected, now) {
                continue;
            }
            collapse = guard.record_forwarded(&key, protected, now).or(collapse);
        }

        let initial = collapse.expect("第 6 行必须触发折叠");
        assert_eq!(initial, "a\nb\nc\nd\ne\n");
        let finished = guard.finish().expect("折叠流必须有完成事件");
        assert_eq!(finished.initial_text, initial);
        assert_eq!(finished.full_text, "a\nb\nc\nd\ne\nf-tail");
    }

    #[test]
    fn ordinary_slow_typing_never_becomes_a_collapse_candidate() {
        let mut guard = FastPasteEnterGuard::default();
        let start = Instant::now();
        for (index, ch) in "x".repeat(400).chars().enumerate() {
            let now = start + Duration::from_millis(index as u64 * 40);
            let key = press(KeyCode::Char(ch), KeyModifiers::NONE);
            let _ = guard.finish_if_stale(now);
            assert!(guard.record_forwarded(&key, false, now).is_none());
        }
        assert!(!guard.is_collapsed());
    }

    #[test]
    fn large_paste_threshold_boundary() {
        // 短文本 / 少行：不触发占位符。
        assert!(!is_large_paste("hello"));
        assert!(!is_large_paste("line1\nline2\nline3\nline4\nline5"));
        // 恰好 300 字符：触发。
        assert!(is_large_paste(&"x".repeat(300)));
        // 5 行不触发；6 行触发。
        assert!(!is_large_paste("a\nb\nc\nd\ne"));
        assert!(is_large_paste("a\nb\nc\nd\ne\nf"));
        // 长度与行数都不足但组合接近：以各自独立阈值判定。
        assert!(!is_large_paste(&"a\n".repeat(4)));
    }

    #[test]
    fn paste_newlines_are_normalized_without_touching_unicode_or_tabs() {
        assert_eq!(
            normalize_newlines("第一行\r\n第二行\r第三行\n\t结尾".into()),
            "第一行\n第二行\n第三行\n\t结尾"
        );
        assert_eq!(normalize_newlines("中文😀".into()), "中文😀");
    }

    #[test]
    fn paste_placeholder_is_clean_and_only_suffixes_duplicates() {
        let text = "第一行\n第二行";
        let mut pasted = HashMap::new();
        let first = next_paste_placeholder(&pasted, text);
        assert_eq!(first, "[Pasted Content 7 chars]");
        pasted.insert(first, text.to_string());
        assert_eq!(
            next_paste_placeholder(&pasted, text),
            "[Pasted Content 7 chars] #2"
        );
    }

    #[test]
    fn expand_replaces_all_placeholders() {
        let mut pasted = HashMap::new();
        pasted.insert("[Pasted Content 5 chars]".into(), "很长的内容".into());
        pasted.insert("[Pasted Content 6 chars]".into(), "second".into());
        let text = "前缀[Pasted Content 5 chars]中缀[Pasted Content 6 chars]后缀";
        assert_eq!(
            expand_paste_placeholders(text, &pasted),
            "前缀很长的内容中缀second后缀"
        );
    }

    #[test]
    fn expand_keeps_missing_id_verbatim() {
        // id 不在 pasted 中（已清理/序列重置）：保留占位符原样，不吞内容。
        let pasted = HashMap::new();
        let text = "[Pasted Content 5 chars]";
        assert_eq!(expand_paste_placeholders(text, &pasted), text);
    }

    #[test]
    fn expand_distinguishes_same_length_paste_suffixes() {
        let mut pasted = HashMap::new();
        pasted.insert("[Pasted Content 3 chars]".into(), "one".into());
        pasted.insert("[Pasted Content 3 chars] #2".into(), "two".into());
        assert_eq!(
            expand_paste_placeholders(
                "[Pasted Content 3 chars] + [Pasted Content 3 chars] #2",
                &pasted,
            ),
            "one + two"
        );
    }

    #[test]
    fn expand_no_placeholder_is_noop() {
        let pasted = HashMap::new();
        assert_eq!(expand_paste_placeholders("普通输入", &pasted), "普通输入");
    }
}
