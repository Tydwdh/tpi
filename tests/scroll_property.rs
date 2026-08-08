//! 滚动引擎属性测试（§31：random heights / random area_h / random scroll）。
//!
//! 不变量：
//! - 任意高度表 + 视口下，window_start_row 必须在 [0, total-area_h] 内；
//! - locate_row/row_of 往返一致；
//! - Follow = 尾部窗口。

use proptest::prelude::*;
use tpi::tui::scroll::{EntryId, ScrollAnchor, ScrollMode, locate_row, row_of, window_start_row};

proptest! {
    #[test]
    fn window_start_row_and_locate_roundtrip_never_overflow(
        heights in proptest::collection::vec(0usize..=50, 0..=20),
        area_h in 1usize..=100,
        anchor_row in 0usize..=100,
    ) {
        let ids: Vec<EntryId> = (0..heights.len()).map(|i| EntryId(i as u64)).collect();
        let total: usize = heights.iter().sum();
        if ids.is_empty() {
            return Ok(());
        }
        let scroll = ScrollMode::Locked(ScrollAnchor {
            entry_id: EntryId(0),
            row_in_entry: anchor_row,
        });
        let start = window_start_row(&ids, &heights, &scroll, area_h);
        let max_start = total.saturating_sub(area_h.max(1));
        prop_assert!(start <= max_start, "start={} max={}", start, max_start);

        // locate_row/row_of 往返：任意全局行 r 定位后应回到同一行。
        for r in 0..=total.saturating_sub(1) {
            let (id, row) = locate_row(&ids, &heights, r);
            prop_assert_eq!(row_of(&ids, &heights, id, row), r, "roundtrip row={}", r);
        }

        // Follow 模式 = 尾部窗口。
        let follow = window_start_row(&ids, &heights, &ScrollMode::Follow, area_h);
        prop_assert_eq!(follow, total.saturating_sub(area_h.max(1)));
    }
}
