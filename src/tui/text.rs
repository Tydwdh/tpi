//! UTF-8 安全截断统一 helper（TPI_TUI_V2_TASK §24：P0，绝不 panic）。
//!
//! 所有按字节上限截断 Text 的代码必须经过这里；禁止直接
//! `&text[offset..]` / `drain(..offset)`（除非已保证 char boundary）。

/// 向下取整到 UTF-8 字符边界：返回 `<= index` 的最大 char boundary。
///
/// `index >= len` 时返回 `len`。
pub fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 向上取整到 UTF-8 字符边界：返回 `>= index` 的最小 char boundary。
///
/// `index >= len` 时返回 `len`。
pub fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 保留尾部窗口的安全切片（`max_bytes` 字节以内，端点落在字符边界）。
///
/// 语义：从 `s.len() - max_bytes` 处**向上**取整到字符边界作为起点，
/// 保证窗口严格 `<= max_bytes`（宁可丢弃半个字符也不超预算）。
/// 整个 `s` 不超过 `max_bytes` 时原样返回。
pub fn suffix_by_bytes_safe(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let start = ceil_char_boundary(s, s.len() - max_bytes);
    &s[start..]
}

/// 中段截断：保留头部与尾部，中间用 `marker` 连接；总长不超过
/// `max_bytes`（marker 本身计入预算），两端均落在字符边界。
///
/// 语义：头部 `head` 字符 + marker + 尾部窗口，保证最终字节数
/// `<= max_bytes`。`s` 不超过 `max_bytes` 时原样返回（无 marker）。
pub fn truncate_middle_utf8(s: &str, max_bytes: usize, marker: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let marker_bytes = marker.len();
    if max_bytes <= marker_bytes {
        // 预算连 marker 都放不下：退化为纯尾部（仍不超预算）。
        return suffix_by_bytes_safe(s, max_bytes).to_string();
    }
    let budget = max_bytes - marker_bytes;
    // 头部取预算的一半（字符边界），剩余给尾部；
    // 尾部预算可能为 0（退化为纯头部 + marker）。
    let head_limit = budget / 2;
    let head_end = floor_char_boundary(s, head_limit);
    let tail_budget = budget.saturating_sub(head_end);
    let mut out = String::with_capacity(max_bytes);
    out.push_str(&s[..head_end]);
    out.push_str(marker);
    out.push_str(suffix_by_bytes_safe(&s[head_end..], tail_budget));
    debug_assert!(out.len() <= max_bytes, "truncate_middle_utf8 超出预算");
    out
}

/// 单个字符的显示宽度（0 = 零宽：组合符/ZWJ/控制符）。
///
/// 之前各处用 `width(ch).unwrap_or(0).max(1)`，把 ZWJ/组合符也算 1 列，
/// 导致 emoji 序列（如 👨‍💻）在折行/光标列计算时漂移。终端实际按 0 列渲染。
pub fn char_cell_width(ch: char) -> usize {
    if matches!(ch, '\u{FF9E}' | '\u{FF9F}') {
        // 半角假名浊音点（ｶﾞ 的 ﾞ）：Ratatui 按 1 cell 计，unicode-width 却算 0。
        // 移植自 codex width.rs（Apache-2.0）。
        1
    } else {
        unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

/// 字符串的终端显示宽度（`usize` 精度，匹配 Ratatui cell 语义）。
/// 与 [`char_cell_width`] 一致：半角音标点按 1 cell。
pub fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
        + text
            .chars()
            .filter(|ch| matches!(ch, '\u{FF9E}' | '\u{FF9F}'))
            .count()
}

/// 扣除固定前缀列后的可用内容宽度；`None` = 前缀耗尽全部宽度。
/// 极窄终端下避免 0 宽渲染产生空/不稳定输出（移植自 codex width.rs）。
pub fn usable_content_width(total_width: usize, reserved_cols: usize) -> Option<usize> {
    total_width
        .checked_sub(reserved_cols)
        .filter(|remaining| *remaining > 0)
}

/// 按显示宽度（terminal cells）截断：保留头部，超预算部分以 `marker` 结尾；
/// 总显示宽度不超过 `max_cells`（marker 计入预算；CJK 等双宽字符按 2 cells）。
///
/// 与字节版 [`truncate_middle_utf8`] 不同：预算单位是终端列宽而非字节，
/// 供已知渲染列宽的场景使用（如侧边栏 todo 项——文本以开头信息为主，
/// 中段截断会头尾割裂）。
pub fn truncate_head_to_cell_width(s: &str, max_cells: usize, marker: &str) -> String {
    if display_width(s) <= max_cells {
        return s.to_string();
    }
    let budget = max_cells.saturating_sub(display_width(marker));
    let mut end = 0usize;
    let mut acc = 0usize;
    for (idx, ch) in s.char_indices() {
        let w = char_cell_width(ch);
        if acc + w > budget {
            break;
        }
        acc += w;
        end = idx + ch.len_utf8();
    }
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..end]);
    out.push_str(marker);
    out
}

/// 按显示宽度（terminal cells）中段截断：头部与尾部各留约一半，中间以
/// `marker` 连接；总显示宽度不超过 `max_cells`（marker 计入预算）。
/// 用于开头与结尾都重要的长文本（如侧边栏用户消息大纲）。
pub fn truncate_to_cell_width(s: &str, max_cells: usize, marker: &str) -> String {
    if display_width(s) <= max_cells {
        return s.to_string();
    }
    let marker_cells = display_width(marker);
    if max_cells <= marker_cells {
        // 预算连 marker 都放不下：退化为纯头部（仍不超预算）。
        return truncate_head_to_cell_width(s, max_cells, "");
    }
    let budget = max_cells - marker_cells;
    let head_limit = budget / 2;
    let mut head_end = 0usize;
    let mut head_cells = 0usize;
    for (idx, ch) in s.char_indices() {
        let w = char_cell_width(ch);
        if head_cells + w > head_limit {
            break;
        }
        head_cells += w;
        head_end = idx + ch.len_utf8();
    }
    // 尾部：从尾部反向累计到 <= 剩余预算（起点落在字符边界）。
    let tail_budget = budget.saturating_sub(head_cells);
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..head_end]);
    out.push_str(marker);
    if tail_budget > 0 {
        let mut start = s.len();
        let mut acc = 0usize;
        for (idx, ch) in s.char_indices().rev() {
            let w = char_cell_width(ch);
            if acc + w > tail_budget {
                break;
            }
            acc += w;
            start = idx;
        }
        if start < s.len() {
            out.push_str(&s[start..]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZWJ: &str = "👨\u{200d}💻"; // 8 字节
    const COMBINING: &str = "e\u{301}"; // 3 字节

    #[test]
    fn floor_boundary_at_char_starts_is_identity() {
        assert_eq!(floor_char_boundary("你好世界", 0), 0);
        assert_eq!(floor_char_boundary("你好世界", 3), 3);
        assert_eq!(floor_char_boundary("你好世界", 12), 12);
        assert_eq!(floor_char_boundary("你好世界", 99), 12);
    }

    #[test]
    fn floor_boundary_retreats_inside_multibyte() {
        // "你" = 3 字节；index 1、2 都落回 0。
        assert_eq!(floor_char_boundary("你好", 1), 0);
        assert_eq!(floor_char_boundary("你好", 2), 0);
        assert_eq!(floor_char_boundary("你好", 4), 3);
    }

    #[test]
    fn ceil_boundary_advances_inside_multibyte() {
        assert_eq!(ceil_char_boundary("你好", 1), 3);
        assert_eq!(ceil_char_boundary("你好", 2), 3);
        assert_eq!(ceil_char_boundary("你好", 3), 3);
        assert_eq!(ceil_char_boundary("你好", 6), 6);
    }

    #[test]
    fn suffix_keeps_tail_within_budget() {
        let s = "abc你好世界xyz";
        let tail = suffix_by_bytes_safe(s, 10);
        assert!(tail.len() <= 10);
        assert!(s.ends_with(tail));
        assert!(tail.is_char_boundary(0));
        assert_eq!(suffix_by_bytes_safe(s, 999), s);
        assert_eq!(suffix_by_bytes_safe("", 4), "");
    }

    #[test]
    fn suffix_strictly_never_exceeds_budget_even_inside_multibyte() {
        // “你”=3 字节：len-预算 落在字符中间时必须再多丢一个字符。
        let s = "你好世界";
        for budget in 1..=12 {
            let tail = suffix_by_bytes_safe(s, budget);
            assert!(tail.len() <= budget, "budget={budget} len={}", tail.len());
            assert!(s.ends_with(tail));
        }
    }

    #[test]
    fn suffix_never_splits_zwj_or_combining() {
        let s = format!("{ZWJ}{COMBINING}尾巴");
        let tail = suffix_by_bytes_safe(&s, 7);
        assert!(tail.len() <= 7);
        assert!(s.ends_with(tail));
        // 组合字符在 emoji 之后：切在 ZWJ 序列中间时宁可多丢一个字符。
        assert!(!tail.starts_with('\u{200d}'));
    }

    #[test]
    fn middle_truncation_stays_within_budget_and_both_ends_valid() {
        let s = "头部内容头部内容".repeat(10) + "中段" + &"尾部内容".repeat(10);
        for budget in [1usize, 2, 3, 4, 7, 8, 12, 16, 31, 32, 33, 64, 100] {
            let out = truncate_middle_utf8(&s, budget, "…[truncated]");
            assert!(out.len() <= budget, "budget={budget} len={}", out.len());
            assert!(out.is_char_boundary(out.len()));
            if budget > "…[truncated]".len() + 6 {
                assert!(out.contains("…[truncated]"));
                // 尾部必须是原文后缀（从某个字符边界起的连续子串）。
                assert!(
                    s.ends_with(&out[out.find("…[truncated]").unwrap() + "…[truncated]".len()..])
                );
            }
        }
    }

    #[test]
    fn middle_truncation_small_budget_falls_back_to_tail() {
        let out = truncate_middle_utf8("你好世界", 2, "…");
        assert!(out.len() <= 2);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn middle_truncation_short_input_is_unchanged() {
        assert_eq!(truncate_middle_utf8("你好", 100, "…"), "你好");
    }

    #[test]
    fn head_truncation_keeps_head_within_cells() {
        // “第一项：重构侧边栏布局与渲染管线” = 15 个双宽字符 = 32 cells。
        let s = "第一项：重构侧边栏布局与渲染管线";
        assert_eq!(display_width(s), 32);
        let out = truncate_head_to_cell_width(s, 20, "…");
        assert!(
            display_width(&out) <= 20,
            "out={out:?} cells={}",
            display_width(&out)
        );
        assert!(out.starts_with("第一项："));
        assert!(out.ends_with("…"));
        // 短文本原样返回。
        assert_eq!(truncate_head_to_cell_width(s, 100, "…"), s);
    }

    #[test]
    fn head_truncation_budget_only_marker_yields_empty_head() {
        let out = truncate_head_to_cell_width("abc", 1, "…");
        assert_eq!(out, "…");
        assert!(display_width(&out) <= 1);
    }

    #[test]
    fn middle_truncation_keeps_both_ends_within_cells() {
        // 长文本：头部与尾部各留约一半，中间以 marker 连接，总宽不超预算。
        let s = "头部内容头部内容".repeat(4) + "中段" + &"尾部内容".repeat(4);
        for budget in [2usize, 3, 4, 7, 8, 12, 16, 20, 32] {
            let out = truncate_to_cell_width(&s, budget, "…");
            assert!(
                display_width(&out) <= budget,
                "budget={budget} out={out:?} cells={}",
                display_width(&out)
            );
            assert!(out.is_char_boundary(out.len()));
            // 尾部必须是原文后缀（从某字符边界起的连续子串）。
            if let Some(pos) = out.find("…")
                && budget > 6
            {
                let tail = &out[pos + "…".len()..];
                assert!(s.ends_with(tail), "tail={tail:?}");
            }
        }
    }
}
