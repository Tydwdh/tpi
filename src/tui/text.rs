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
}

/// 单个字符的显示宽度（0 = 零宽：组合符/ZWJ/控制符）。
///
/// 之前各处用 `width(ch).unwrap_or(0).max(1)`，把 ZWJ/组合符也算 1 列，
/// 导致 emoji 序列（如 👨‍💻）在折行/光标列计算时漂移。终端实际按 0 列渲染。
pub fn char_cell_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}
