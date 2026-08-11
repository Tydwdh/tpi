//! 粘贴突发检测：bracketed paste 不可用时的降级路径（§用户诉求）。
//!
//! 支持 bracketed paste 的终端会把整段粘贴作为单个 `Event::Paste` 送达，
//! 多行文本里的换行由 reducer 原样插入，不会触发提交。不支持的终端
//! （旧 conhost、部分 SSH/嵌入终端）则把粘贴内容拆成连续 `KeyEvent` 流，
//! 其中的 Enter 会命中 `submit` 绑定，造成「粘贴多行文本时换行直接发送」。
//!
//! 本模块提供时间窗合并：[`merge`] 把一次突发按键合并为一个字符串，由键盘
//! 读取线程在相邻按键间隔 ≤ [`GAP`]（探测）且 ≥ [`MIN_BURST`] 个可插入
//! 按键时触发，输出走 `Event::Paste` → reducer 的 Paste 分支（字面换行，
//! 不触发提交）。
//!
//! 判定安全边界：
//! - 人类最快打字也 ≥ 100ms/键，[`GAP`]=50ms 的连续按键只可能来自粘贴/按键宏；
//! - 一旦确认是突发（探测窗内有后续），收集窗放宽到 [`COLLECT_GAP`]=150ms——
//!   大文本粘贴时终端（conhost/conpty 无 bracketed paste）按批次/节流转发
//!   按键流，批次间隔可能超过 30ms；若仍用短窗会在 1~2 个键后断流，`merge`
//!   因不足 [`MIN_BURST`] 返回 None → 逐键转发 → 输入框逐字符出现；
//! - 少于 [`MIN_BURST`] 个按键不合并，保护「快速连按 Enter 连发多条」；
//! - 出现修饰键组合、特殊键或 Repeat 事件即放弃合并，按原样逐键转发，
//!   不吞掉任何合法输入。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// 突发判定间隔（探测窗）：首个按键后等待该时长，无后续视为普通按键。
/// 50ms 对打字基本无感（人连按也 ≥100ms/键），但能覆盖大文本粘贴时
/// 终端（conhost/conpty 节流）按 30~50ms 间隔分批转发的按键流——探测窗
/// 若小于批次间隔，第一个键就探测超时 → 逐键转发 → 逐字符出现。
pub const GAP: std::time::Duration = std::time::Duration::from_millis(50);
/// 收集窗：已确认是突发后，相邻按键间隔 ≤ 该值继续收进同一批。
/// 放宽到 150ms 以覆盖大文本粘贴时终端的节流/分批转发。
pub const COLLECT_GAP: std::time::Duration = std::time::Duration::from_millis(150);
/// 突发最短按键数：低于此值按普通按键逐键处理。
pub const MIN_BURST: usize = 3;
/// 单次突发按键数上限：防止异常长流无限累积（超限拆成多次 Paste，语义不变）。
pub const MAX_BURST: usize = 8192;

/// 把一次突发按键合并为粘贴文本；不是粘贴突发时返回 `None`（逐键转发）。
///
/// 合并规则：
/// - 只统计 `Press` 事件（Repeat/Release 是按住自动重复，不属于粘贴内容）；
/// - 突发必须全部由可插入按键组成：无 Ctrl/Alt 修饰的字符（允许 Shift
///   保留大小写）、Enter（→`\n`）、Tab（→`\t`）；
/// - 出现任何修饰组合、特殊键（Esc/方向键/功能键等）或少于 [`MIN_BURST`]
///   个按键 → `None`。
pub fn merge(keys: &[KeyEvent]) -> Option<String> {
    if keys.len() < MIN_BURST {
        return None;
    }
    let mut out = String::with_capacity(keys.len());
    for key in keys {
        out.push(paste_char(key)?);
    }
    Some(out)
}

/// 按键 → 可插入字符；不可插入（非 Press / Ctrl+Alt 组合 / 特殊键）返回 `None`。
fn paste_char(key: &KeyEvent) -> Option<char> {
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
    fn merge_multiline_burst_preserves_newlines() {
        // 粘贴 "第一行\n第二行"：字符 + Enter 组成的快速突发。
        let burst = [
            press(KeyCode::Char('第'), KeyModifiers::NONE),
            press(KeyCode::Char('一'), KeyModifiers::NONE),
            press(KeyCode::Char('行'), KeyModifiers::NONE),
            press(KeyCode::Enter, KeyModifiers::NONE),
            press(KeyCode::Char('第'), KeyModifiers::NONE),
            press(KeyCode::Char('二'), KeyModifiers::NONE),
            press(KeyCode::Char('行'), KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst).as_deref(), Some("第一行\n第二行"));
    }

    #[test]
    fn merge_keeps_shift_case_and_tab() {
        let burst = [
            press(KeyCode::Char('G'), KeyModifiers::SHIFT),
            press(KeyCode::Char('i'), KeyModifiers::NONE),
            press(KeyCode::Char('t'), KeyModifiers::NONE),
            press(KeyCode::Tab, KeyModifiers::NONE),
            press(KeyCode::Enter, KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst).as_deref(), Some("Git\t\n"));
    }

    #[test]
    fn merge_rejects_short_burst() {
        // 少于 MIN_BURST：逐键处理（保护快速连按 Enter 等合法输入）。
        let burst = [
            press(KeyCode::Char('a'), KeyModifiers::NONE),
            press(KeyCode::Char('b'), KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst), None);
    }

    #[test]
    fn merge_rejects_modifier_keys() {
        let burst = [
            press(KeyCode::Char('a'), KeyModifiers::NONE),
            press(KeyCode::Char('b'), KeyModifiers::CONTROL),
            press(KeyCode::Char('c'), KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst), None, "Ctrl 组合不是粘贴内容，不得合并");
    }

    #[test]
    fn merge_rejects_ctrl_enter() {
        let burst = [
            press(KeyCode::Char('a'), KeyModifiers::NONE),
            press(KeyCode::Enter, KeyModifiers::CONTROL),
            press(KeyCode::Char('b'), KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst), None);
    }

    #[test]
    fn merge_rejects_special_keys() {
        let burst = [
            press(KeyCode::Char('a'), KeyModifiers::NONE),
            press(KeyCode::Esc, KeyModifiers::NONE),
            press(KeyCode::Char('b'), KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst), None, "特殊键按原样逐键转发");
    }

    #[test]
    fn merge_rejects_repeat_events() {
        // Repeat/Release 是按住自动重复，不属于粘贴内容。
        let burst = [
            press(KeyCode::Char('a'), KeyModifiers::NONE),
            KeyEvent::new_with_kind(KeyCode::Char('b'), KeyModifiers::NONE, KeyEventKind::Repeat),
            press(KeyCode::Char('c'), KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst), None);
    }

    /// §用户诉求（大文本粘贴逐字符）：即使批次间隔较大（≤ COLLECT_GAP），
    /// 整个突发仍必须合并为单个 Paste——否则 merge 不足 MIN_BURST 逐键转发。
    #[test]
    fn merge_handles_large_throttled_burst() {
        // 模拟 5000 字符的大文本粘贴（含换行），全部可插入。
        let mut burst = Vec::new();
        for i in 0..5000 {
            burst.push(press(
                if i % 50 == 0 {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(char::from(b'a' + (i % 26) as u8))
                },
                KeyModifiers::NONE,
            ));
        }
        let merged = merge(&burst).expect("大文本突发必须合并，不得逐键转发");
        assert_eq!(merged.len(), 5000);
        assert_eq!(merged.matches('\n').count(), 100, "换行保留为字面 \n");
    }

    #[test]
    fn merge_single_line_burst_is_paste_semantics() {
        // 单行粘贴（无换行）合并后与逐键输入结果一致，不影响后续 Enter 提交。
        let burst = [
            press(KeyCode::Char('h'), KeyModifiers::NONE),
            press(KeyCode::Char('i'), KeyModifiers::NONE),
            press(KeyCode::Char('!'), KeyModifiers::NONE),
        ];
        assert_eq!(merge(&burst).as_deref(), Some("hi!"));
    }
}
