//! 滚动引擎（TPI_TUI_V2_TASK §3-4、§10、§57-58、§65）。
//!
//! Follow / Locked(anchor) 双模式；Locked 的锚点是**稳定 EntryId + entry
//! 内 visual row**，不是“距底部行数”。所有定位/移动都是纯函数
//! （§65：entries + width + height + anchor → 窗口），可单测。

/// 转录条目的稳定 ID（§4.1）：不用 Vec index——trim/filter 都可能改 index。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub u64);

/// 滚动锚点（§4）：entry + entry 内 visual row。
/// 语义：锚点行是视口顶部行；resize 后同一语义位置仍为视口顶部（§4.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollAnchor {
    pub entry_id: EntryId,
    pub row_in_entry: usize,
}

/// 滚动模式（§3）：Follow = 始终显示尾部；Locked = 锚定，新输出不动视口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollMode {
    #[default]
    Follow,
    Locked(ScrollAnchor),
}

/// 返回窗口起始行（0-based 全局 visual row）。
///
/// - Follow：`total - area_h`（尾部窗口）。
/// - Locked：锚点行本身（锚点 = 视口顶部行）；锚点 entry 已被 trim 时
///   回退到最早仍存在的 entry（§68）；窗口越界时 clamp 到 `total - area_h`
///   （内容减少时仍显示完整窗口）。
pub fn window_start_row(
    ids: &[EntryId],
    heights: &[usize],
    scroll: &ScrollMode,
    area_h: usize,
) -> usize {
    debug_assert_eq!(ids.len(), heights.len());
    let total: usize = heights.iter().sum();
    let area_h = area_h.max(1);
    let follow_start = total.saturating_sub(area_h);
    match scroll {
        ScrollMode::Follow => follow_start,
        ScrollMode::Locked(anchor) => {
            // 锚点 entry 已被 trim：回退到最早仍存在的 entry（§68）。
            if !ids.contains(&anchor.entry_id) {
                return 0;
            }
            let anchor_row = row_of(ids, heights, anchor.entry_id, anchor.row_in_entry);
            let max_start = total.saturating_sub(area_h);
            anchor_row.min(max_start)
        }
    }
}

/// 锚点 entry 的绝对起始行（clamp 到内容末尾）。
pub fn entry_start_row(ids: &[EntryId], heights: &[usize], entry: EntryId) -> usize {
    let mut row = 0;
    for (id, height) in ids.iter().zip(heights.iter()) {
        if *id == entry {
            return row;
        }
        row += height;
    }
    row // 未找到：内容末尾（调用方按 §68 回退）
}

/// 把全局行号定位到 (entry, row_in_entry)；行号超出内容末尾时
/// clamp 到最后一行的位置。entry 未找到时（已被 trim）定位到最早 entry。
pub fn locate_row(ids: &[EntryId], heights: &[usize], row: usize) -> (EntryId, usize) {
    let mut row = row;
    for (id, height) in ids.iter().zip(heights.iter()) {
        if row < *height {
            return (*id, row);
        }
        row -= height;
    }
    // 末尾（可能正好在最后 entry 的最后一行之后一行）。
    let last = ids.len().saturating_sub(1);
    if ids.is_empty() {
        // 无内容：给一个哨兵（调用方应走 Follow）。
        return (EntryId(0), 0);
    }
    let last_height = heights[last];
    (ids[last], last_height.saturating_sub(1))
}

/// 锚点 (entry, row_in_entry) 的全局行号（entry 不存在时返回内容末尾行；
/// row_in_entry 超 entry 高度时 clamp 到 entry 末行）。
pub fn row_of(ids: &[EntryId], heights: &[usize], entry: EntryId, row_in_entry: usize) -> usize {
    let start = entry_start_row(ids, heights, entry);
    let total: usize = heights.iter().sum();
    // entry 高度（未找到时按 0 处理，start 已为末尾）。
    let height = ids
        .iter()
        .zip(heights.iter())
        .find(|(id, _)| **id == entry)
        .map(|(_, h)| *h)
        .unwrap_or(0);
    let row = row_in_entry.min(height.saturating_sub(1));
    (start + row).min(total.saturating_sub(1))
}

/// 从指定全局行向上/向下移动 `delta` 行（§10：PageUp 移动 viewport-2 行）。
///
/// 返回移动后的 (entry, row_in_entry)。向上到顶/向下到底时 clamp。
pub fn move_by_rows(
    ids: &[EntryId],
    heights: &[usize],
    from_row: usize,
    delta: isize,
) -> (EntryId, usize) {
    let total: usize = heights.iter().sum();
    let target = if delta >= 0 {
        (from_row + delta as usize).min(total.saturating_sub(1))
    } else {
        from_row.saturating_sub((-delta) as usize)
    };
    locate_row(ids, heights, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 3 个 entry，高度 2/3/4 → 总 9 行。
    fn fixtures() -> (Vec<EntryId>, Vec<usize>) {
        (vec![EntryId(1), EntryId(2), EntryId(3)], vec![2, 3, 4])
    }

    #[test]
    fn follow_window_is_tail() {
        let (ids, heights) = fixtures();
        assert_eq!(window_start_row(&ids, &heights, &ScrollMode::Follow, 5), 4);
        assert_eq!(window_start_row(&ids, &heights, &ScrollMode::Follow, 99), 0);
    }

    #[test]
    fn locked_window_anchors_at_entry_row() {
        let (ids, heights) = fixtures();
        // 锚定 entry 2 的第 1 行 → 全局行 2+1=3。
        let anchor = ScrollAnchor {
            entry_id: EntryId(2),
            row_in_entry: 1,
        };
        assert_eq!(
            window_start_row(&ids, &heights, &ScrollMode::Locked(anchor), 5),
            3
        );
        // 窗口越界时 clamp：锚定行 8（最后一行），窗口 5 行 → start=4。
        let anchor = ScrollAnchor {
            entry_id: EntryId(3),
            row_in_entry: 3,
        };
        assert_eq!(
            window_start_row(&ids, &heights, &ScrollMode::Locked(anchor), 5),
            4
        );
    }

    #[test]
    fn missing_anchor_entry_falls_back_to_first_entry() {
        let (ids, heights) = fixtures();
        // entry 99 已被 trim：定位回退最早 entry（全局行 0）。
        let anchor = ScrollAnchor {
            entry_id: EntryId(99),
            row_in_entry: 0,
        };
        assert_eq!(
            window_start_row(&ids, &heights, &ScrollMode::Locked(anchor), 3),
            0
        );
    }

    #[test]
    fn locate_row_maps_global_rows_to_entries() {
        let (ids, heights) = fixtures();
        assert_eq!(locate_row(&ids, &heights, 0), (EntryId(1), 0));
        assert_eq!(locate_row(&ids, &heights, 1), (EntryId(1), 1));
        assert_eq!(locate_row(&ids, &heights, 2), (EntryId(2), 0));
        assert_eq!(locate_row(&ids, &heights, 4), (EntryId(2), 2));
        assert_eq!(locate_row(&ids, &heights, 5), (EntryId(3), 0));
        assert_eq!(locate_row(&ids, &heights, 8), (EntryId(3), 3));
        // 越界 → clamp 到末尾。
        assert_eq!(locate_row(&ids, &heights, 99), (EntryId(3), 3));
    }

    #[test]
    fn move_by_rows_walks_across_entries() {
        let (ids, heights) = fixtures();
        // 从 entry2 第 1 行（全局 3）向上 2 行 → 全局 1 = entry1 第 1 行。
        assert_eq!(move_by_rows(&ids, &heights, 3, -2), (EntryId(1), 1));
        // 从全局 3 向上 4 行 → 全局 0（clamp 到顶）。
        assert_eq!(move_by_rows(&ids, &heights, 3, -4), (EntryId(1), 0));
        // 从全局 3 向下 3 行 → 全局 6 = entry3 第 1 行。
        assert_eq!(move_by_rows(&ids, &heights, 3, 3), (EntryId(3), 1));
        // 从全局 3 向下 99 → clamp 末尾（entry3 第 3 行）。
        assert_eq!(move_by_rows(&ids, &heights, 3, 99), (EntryId(3), 3));
    }
}
