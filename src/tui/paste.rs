//! 粘贴突发检测：bracketed paste 不可用时的降级路径（§用户诉求）。
//!
//! 支持 bracketed paste 的终端会把整段粘贴作为单个 `Event::Paste` 送达，
//! 多行文本里的换行由 reducer 原样插入，不会触发提交。不支持的终端
//! （旧 conhost、部分 SSH/嵌入终端）则把粘贴内容拆成连续 `KeyEvent` 流，
//! 其中的 Enter 会命中 `submit` 绑定，造成「粘贴多行文本时换行直接发送」。
//!
//! 本模块提供**间隔感知**的洪流判定（[`is_dense`]）+ **分块流式 flush**：
//! - 普通按键（与上一键间隔 ≥ [`DENSE_GAP`]）由键盘线程**立即转发，0 等待**——
//!   打字路径不承担任何粘贴检测开销。人最快打字也 ≥50ms/键，[`DENSE_GAP`]=16ms
//!   只有终端批量转发（conhost/conpty 节流、SSH 粘贴注入）能达到；
//! - 一旦某键与上一键间隔 < [`DENSE_GAP`]（粘贴洪流信号），键盘线程把后续
//!   可插入按键收进缓冲区，**相邻批次间隔 > [`FLUSH_GAP`] 即 flush 为一次
//!   `Event::Paste` 上屏**——字符连续流入、无「整批等超时后一起出现」的顿感；
//!   洪流中的 Enter 经 [`paste_char`] 转字面 `\n`（不触发提交）；
//! - 缓冲区内出现修饰键组合、特殊键或 Repeat 事件即停止收集，先把已收内容
//!   flush，再按原样转发该键——不吞掉任何合法输入。
//!
//! 判定安全边界：检测代价只发生在「连续两键间隔 < 16ms」之后，而该间隔
//! 人类打字不可能达到——所以打字 0 延迟、粘贴（现代终端）走 Paste 快路径
//! 也 0 延迟，两者互不干扰。

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// 洪流判定间隔：相邻两次按键间隔小于该值即视为粘贴洪流起点。
///
/// 人类最快打字也 ≥50ms/键（100ms/键 已属快速连打），中文输入法候选上屏
/// 间隔更大；16ms 只有终端批量转发（conhost/conpty 节流、SSH 客户端粘贴
/// 注入）能达到。普通输入永远不会进入收集 → 打字路径零等待。
pub const DENSE_GAP: Duration = Duration::from_millis(16);
/// 洪流内 flush 间隔：收集中的相邻按键间隔超过该值，视为一批结束，立即把
/// 已收内容 flush 为一次 `Event::Paste` 上屏。
///
/// 旧终端按批次/节流转发按键流，批次内间隔 <16ms（洪流），批次间间隔
/// 30~50ms（节流）。40ms 会把 1~2 个批次收进同一块（每块约 10~20 字符，
/// 每次 flush 一次 draw），同时比「整批等 80ms 超时」连续得多，感知无停顿。
pub const FLUSH_GAP: Duration = Duration::from_millis(40);
/// 单块缓冲字符数上限：防异常长流无限累积（超限先 flush 再继续，语义不变）。
pub const MAX_BUF: usize = 8192;

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
}
