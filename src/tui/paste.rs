//! 粘贴突发检测：bracketed paste 序列解析 + 不可用时的降级路径（§用户诉求）。
//!
//! **主路径（支持 bracketed paste 的终端）**：`EnableBracketedPaste` 下，
//! 终端把粘贴注入为 `\x1b[200~ … \x1b[201~` 按键流。crossterm Windows 后端
//! 不解析该序列（parse.rs 逐键产生 `KeyEvent`），因此 app 键盘线程按
//! `Esc + '['` 检测粘贴开始、消费前缀 `[200~`、把内容按键全部缓冲，直到
//! `[201~` 结束，整段作为一次 `Event::Paste` 上屏——不依赖间隔猜测、不读
//! 剪贴板，行尾 Enter 经 [`paste_char`] 转 `\n`，不会命中 `submit`。
//! 空闲超时 [`PASTE_IDLE_TIMEOUT`] 见底异常/误判。
//!
//! **降级路径（旧 conhost/部分 SSH/嵌入终端）**：不发送 bracketed 序列，
//! 粘贴内容被拆成连续 `KeyEvent` 流，其中的 Enter 会命中 `submit` 绑定，
//! 造成「粘贴多行文本时换行直接发送」。
//!
//! 本模块提供**间隔感知**的洪流判定（[`is_dense`]）+ **整次 burst 合并**：
//! - 普通按键（与上一键间隔 ≥ [`DENSE_GAP`]）由键盘线程**立即转发，0 等待**——
//!   打字路径不承担任何粘贴检测开销。人最快打字也 ≥50ms/键，[`DENSE_GAP`]=16ms
//!   只有终端批量转发（conhost/conpty 节流、SSH 粘贴注入）能达到；
//! - 一旦某键与上一键间隔 < [`DENSE_GAP`]（粘贴洪流信号），键盘线程把后续
//!   可插入按键收进缓冲区，跨过终端短暂节流间隔后再作为一次
//!   `Event::Paste` 上屏；
//!   洪流中的 Enter 经 [`paste_char`] 转字面 `\n`（不触发提交）；
//! - 缓冲区内出现修饰键组合、特殊键或 Repeat 事件即停止收集，先把已收内容
//!   flush，再按原样转发该键——不吞掉任何合法输入。
//!
//! 判定安全边界：检测代价只发生在「连续两键间隔 < 16ms」之后，而该间隔
//! 人类打字不可能达到——所以打字 0 延迟、粘贴（现代终端）走 Paste 快路径
//! 也 0 延迟，两者互不干扰。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// 洪流判定间隔：相邻两次按键间隔小于该值即视为粘贴洪流起点。
///
/// 人类最快打字也 ≥50ms/键（100ms/键 已属快速连打），中文输入法候选上屏
/// 间隔更大；16ms 只有终端批量转发（conhost/conpty 节流、SSH 客户端粘贴
/// 注入）能达到。普通输入永远不会进入收集 → 打字路径零等待。
pub const DENSE_GAP: Duration = Duration::from_millis(16);
/// 普通 Enter 的短前瞻窗口。仅用于判定一个孤立 Enter 后是否紧跟粘贴字符。
pub const FLUSH_GAP: Duration = Duration::from_millis(40);
/// 降级粘贴 burst 的空闲边界。大于常见 conhost/SSH 30~50ms 批间节流，
/// 避免把一个大粘贴拆小，导致换行落到两个 Paste 事件之间而触发提交。
pub const FALLBACK_IDLE_GAP: Duration = Duration::from_millis(140);
/// 单块缓冲上限。正常大粘贴尽量保持为一个事件，使 reducer 能统一走占位符。
pub const MAX_BUF: usize = 512 * 1024;
/// 降级 burst flush 后的提交保护窗。该窗只由“按键洪流”路径武装；现代终端
/// 的 Event::Paste 与 Ctrl+V 剪贴板快路径不会影响用户随后按 Enter 提交。
pub const FALLBACK_ENTER_GUARD: Duration = Duration::from_millis(750);

/// bracketed paste 在初始 `Esc [` 之后剩余的前缀，以及完整结束尾串。
pub const BRACKETED_START_TAIL: [char; 4] = ['2', '0', '0', '~'];
pub const BRACKETED_END_TAIL: [char; 5] = ['[', '2', '0', '1', '~'];

/// Esc 后探测下一键的窗口：`[`（bracketed paste 前缀 `\x1b[200~` 的第 2 键）
/// 与结束序列 `\x1b[201~`（Esc 后紧接 `[,2,0,1,~`）。终端逐键注入时序列内
/// 间隔 <1ms；40ms 只影响 Esc 后的等待——普通 Esc（低频）无后续键时
/// poll 超时即转发，感知为零。
pub const BRACKETED_PROBE_GAP: Duration = Duration::from_millis(40);
/// bracketed paste 内容收集的空闲超时：连续无新键这么久即视为粘贴流结束
/// （终端异常/误判兜底；真粘贴批间间隔远小于此）。
pub const PASTE_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// 大粘贴判定阈值（§用户诉求：大粘贴用占位符代替真实渲染）。
///
/// 超过 [`LARGE_PASTE_LINES`] 行（含换行）或达到 [`LARGE_PASTE_CHARS`] 字符时，
/// 真实内容不插入输入框，而是以 `[Pasted Text: N chars #id]` 占位符上屏，
/// 全文存入旁路（`UiState.pasted`），提交时一次性展开发送——避免大文本
/// 在输入框内分块渲染、被 `MAX_INPUT_BYTES` 截断、以及行尾 Enter 在
/// 批次边界被误判提交。阈值来自用户指定（5 行 / 300 字符，gemini-cli 同款）。
pub const LARGE_PASTE_LINES: usize = 5;
pub const LARGE_PASTE_CHARS: usize = 300;

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

/// 大粘贴占位符文本：输入框只渲染它；`id` 是 [`UiState::store_paste`] 返回的自增 id。
pub fn paste_placeholder(id: u64, text: &str) -> String {
    format!("[Pasted Text: {} chars #{id}]", text.chars().count())
}

/// 占位符正则（`[Pasted Text: N chars #<id>]`；捕获 id）。
fn placeholder_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\[Pasted Text: \d+ chars #(\d+)\]").unwrap())
}

/// 提交时把文本中的大粘贴占位符展开为真实内容（§用户诉求：发送时一起发送）。
///
/// 占位符 id 在 `pasted` 中查不到（被清理等异常）时保留原样，不吞内容。
pub fn expand_paste_placeholders(text: &str, pasted: &HashMap<u64, String>) -> String {
    placeholder_regex()
        .replace_all(text, |caps: &regex::Captures| {
            let id: u64 = caps[1].parse().unwrap_or(0);
            match pasted.get(&id) {
                Some(content) => content.clone(),
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

/// 相邻两次按键间隔是否构成粘贴洪流（间隔 < [`DENSE_GAP`]）。
///
/// `last_press = None`（首次按键、或被 Paste/Mouse/Resize 打断后）不构成
/// 洪流——洪流判定只依据相邻按键间隔，不依赖任何时间窗等待。
pub fn is_dense(last_press: Option<Instant>, now: Instant) -> bool {
    last_press.is_some_and(|t| now.duration_since(t) < DENSE_GAP)
}

/// 无修饰 Enter（提交键）判定（§bug 修复：先输入再粘贴会误发送）。
///
/// 降级路径下粘贴按键流里的 Enter 若位于粘贴开头/批次边界，与上一键
/// 间隔 ≥[`DENSE_GAP`] 会被当普通按键逐键转发 → 命中 submit 提交。键盘
/// 线程只对「无修饰 Enter」做探测（粘贴内容不含修饰键；Shift+Enter 是
/// 换行、Ctrl+Enter 等是用户自定义提交，都不探测）。
pub fn is_plain_enter(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.code == KeyCode::Enter
        && key.modifiers == KeyModifiers::NONE
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

/// 降级粘贴结束后的 Enter 保护。防止终端把最后一个换行延迟到 burst flush
/// 之后，随后被 reducer 当成 submit。保护只消费一次又一次落在窗内的 Enter；
/// 超时后自动恢复正常提交。
#[derive(Debug, Default)]
pub struct FallbackPasteGuard {
    until: Option<Instant>,
}

impl FallbackPasteGuard {
    pub fn arm(&mut self, now: Instant) {
        self.until = Some(now + FALLBACK_ENTER_GUARD);
    }

    pub fn absorb_enter(&mut self, now: Instant) -> bool {
        if self.until.is_some_and(|until| now <= until) {
            self.arm(now);
            true
        } else {
            self.until = None;
            false
        }
    }
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
    use std::time::Duration;

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn is_dense_false_without_previous_press() {
        // 首次按键：无间隔可判，不构成洪流（不得等待任何探测窗）。
        assert!(!is_dense(None, Instant::now()));
    }

    #[test]
    fn is_dense_threshold_boundary() {
        let now = Instant::now();
        // 间隔 = DENSE_GAP 不算洪流（>= 阈值，普通输入边界）。
        let last = now - DENSE_GAP;
        assert!(!is_dense(Some(last), now));
        // 间隔略小于 DENSE_GAP 才是洪流（终端批量转发才可能）。
        let last = now - DENSE_GAP + Duration::from_micros(1);
        assert!(is_dense(Some(last), now));
        // 人类打字间隔（≥50ms）远高于阈值，永不误判。
        let last = now - Duration::from_millis(50);
        assert!(!is_dense(Some(last), now));
    }

    #[test]
    fn is_plain_enter_only_unmodified() {
        // 无修饰 Enter 才做粘贴流探测（提交键）。
        assert!(is_plain_enter(&press(KeyCode::Enter, KeyModifiers::NONE)));
        // Shift+Enter 是换行（不提交）、Ctrl/Alt+Enter 是用户自定义绑定：不探测。
        assert!(!is_plain_enter(&press(KeyCode::Enter, KeyModifiers::SHIFT)));
        assert!(!is_plain_enter(&press(
            KeyCode::Enter,
            KeyModifiers::CONTROL
        )));
        assert!(!is_plain_enter(&press(KeyCode::Enter, KeyModifiers::ALT)));
        // 非 Enter 键不探测。
        assert!(!is_plain_enter(&press(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
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
    fn fallback_guard_absorbs_delayed_enter_but_expires() {
        let start = Instant::now();
        let mut guard = FallbackPasteGuard::default();
        assert!(
            !guard.absorb_enter(start),
            "未发生兜底粘贴时 Enter 正常提交"
        );
        guard.arm(start);
        assert!(
            guard.absorb_enter(start + Duration::from_millis(300)),
            "burst 后迟到的 Enter 必须作为粘贴换行"
        );
        assert!(!guard.absorb_enter(
            start + Duration::from_millis(300) + FALLBACK_ENTER_GUARD + Duration::from_millis(1)
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
    fn paste_placeholder_contains_id_and_counts() {
        let text = "第一行\n第二行";
        let p = paste_placeholder(7, text);
        assert_eq!(p, "[Pasted Text: 7 chars #7]");
    }

    #[test]
    fn expand_replaces_all_placeholders() {
        let mut pasted = HashMap::new();
        pasted.insert(1, "很长的内容".to_string());
        pasted.insert(2, "second".to_string());
        let text = "前缀[Pasted Text: 5 chars #1]中缀[Pasted Text: 6 chars #2]后缀";
        assert_eq!(
            expand_paste_placeholders(text, &pasted),
            "前缀很长的内容中缀second后缀"
        );
    }

    #[test]
    fn expand_keeps_missing_id_verbatim() {
        // id 不在 pasted 中（已清理/序列重置）：保留占位符原样，不吞内容。
        let pasted = HashMap::new();
        let text = "[Pasted Text: 5 chars #99]";
        assert_eq!(expand_paste_placeholders(text, &pasted), text);
    }

    #[test]
    fn expand_no_placeholder_is_noop() {
        let pasted = HashMap::new();
        assert_eq!(expand_paste_placeholders("普通输入", &pasted), "普通输入");
    }
}
