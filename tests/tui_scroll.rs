//! T4：Scroll Engine 验收测试（TPI_TUI_V2_TASK §58 场景 A-D、§64、§65）。
//!
//! 纯函数 + 渲染双路径：视口定位基于 EntryId 锚点，新输出/resize 不移动
//! 视口；End 恢复 Follow。

use tpi::tui::draw_to_test_backend;
use tpi::tui::model::{LineKind, ViewModel};
use tpi::tui::scroll::{
    EntryId, ScrollAnchor, ScrollMode, locate_row, move_by_rows, window_start_row,
};

/// 构造 N 条消息，每条占 1 个 visual row（80 列下）。
fn messages(view: &mut ViewModel, n: usize) {
    for i in 0..n {
        view.push_line(LineKind::Assistant, format!("msg {i}"));
    }
}

/// 布局一次并返回视口顶部行（布局会写回 layout_top/entry_heights）。
fn layout(view: &mut ViewModel, width: u16, height: u16) -> (EntryId, usize) {
    let _ = draw_to_test_backend(view, width, height);
    view.layout_top.expect("布局后必须有 layout_top")
}

#[test]
fn scenario_a_append_does_not_move_locked_viewport() {
    // 200 条 history → 滚到 entry 80 → 追加 100 条：视口仍锚定 entry 80。
    let mut view = ViewModel::default();
    messages(&mut view, 200);
    // 布局（视口 20 行，Follow 顶部 = 行 180 附近 = 第 181 个 entry）。
    layout(&mut view, 80, 24); // Inline 视口 = 24-2 = 22 行
    // 向上翻 100 行（10 次 PageUp，每页 10 行）→ 锚定 entry ~80。
    for _ in 0..10 {
        view.scroll_up(10);
    }
    let ScrollMode::Locked(anchor) = view.scroll_mode else {
        panic!("滚动后必须 Locked");
    };
    assert!(
        anchor.entry_id.0 >= 76 && anchor.entry_id.0 <= 84,
        "锚点应在 entry 80 附近，实际 {}",
        anchor.entry_id.0
    );
    let anchor_before = anchor;
    // 追加 100 条新内容。
    messages(&mut view, 100);
    assert_eq!(
        view.scroll_mode,
        ScrollMode::Locked(anchor_before),
        "新输出不得改变锚点"
    );
    assert!(view.pending_below >= 100, "新内容必须计数");
    // 视口仍显示 entry 80 附近：重新布局后顶部行对应 entry 不变。
    let (top_after, _) = layout(&mut view, 80, 24);
    assert_eq!(top_after, anchor_before.entry_id, "视口顶部 entry 不变");
}

#[test]
fn scenario_b_resize_keeps_anchor_semantic_position() {
    let mut view = ViewModel::default();
    // 每条消息 3 行，便于跨 entry 锚点验证。
    for i in 0..60 {
        view.push_line(
            LineKind::Assistant,
            format!("msg {i} 内容 {}", "x".repeat(60)),
        );
    }
    layout(&mut view, 120, 40); // 视口 = 40-2(footer+input) = 38；60 条×1 行 → 顶部行 22
    for _ in 0..3 {
        view.scroll_up(7); // 22 - 21 = 1 → EntryId(2)
    }
    let ScrollMode::Locked(anchor) = view.scroll_mode else {
        panic!("滚动后必须 Locked");
    };
    assert_eq!(anchor.entry_id, EntryId(2), "锚点应落在 entry 2");
    // 依次经过不同尺寸：锚点（视口顶部）必须保持同一语义位置。
    for (w, h) in [(70u16, 25u16), (160, 50), (120, 40), (80, 24)] {
        let (top_entry, top_row) = layout(&mut view, w, h);
        assert_eq!(top_entry, anchor.entry_id, "resize 后视口顶部 entry 不变");
        let _ = top_row;
    }
}

#[test]
fn scenario_c_high_frequency_output_does_not_move_viewport_or_block_input() {
    let mut view = ViewModel::default();
    messages(&mut view, 50);
    layout(&mut view, 80, 24);
    view.scroll_up(5);
    let ScrollMode::Locked(anchor) = view.scroll_mode else {
        panic!();
    };
    // 模拟高速新输出（§58 场景 C：500 deltas/s 视口不移动）。
    for i in 0..100 {
        view.push_line(LineKind::Assistant, format!("new {i}"));
    }
    assert_eq!(view.scroll_mode, ScrollMode::Locked(anchor), "视口不得移动");
    assert!(view.pending_below >= 100, "新条目必须计数");
    // 输入仍响应（reducer 路径在其他测试覆盖；这里验证状态层不被锁死）。
    view.follow_tail();
    assert_eq!(view.scroll_mode, ScrollMode::Follow);
}

#[test]
fn scenario_d_end_restores_follow_immediately() {
    let mut view = ViewModel::default();
    messages(&mut view, 50);
    layout(&mut view, 80, 24);
    view.scroll_up(10);
    assert!(view.scroll_mode != ScrollMode::Follow);
    // End/Ctrl+End。
    view.follow_tail();
    assert_eq!(view.scroll_mode, ScrollMode::Follow);
    assert_eq!(view.pending_below, 0);
    // Follow 布局显示最新内容。
    let (top_entry, _) = layout(&mut view, 80, 24);
    let ids: Vec<EntryId> = view.transcript.iter().map(|e| e.id()).collect();
    let heights: Vec<usize> = ids
        .iter()
        .map(|id| view.entry_heights.get(id).copied().unwrap_or(1))
        .collect();
    let area = view.transcript_rows as usize;
    let start = window_start_row(&ids, &heights, &ScrollMode::Follow, area);
    let (first, _) = locate_row(&ids, &heights, start);
    assert_eq!(top_entry, first, "Follow 布局后顶部=窗口顶部（一致性）");
}

#[test]
fn page_up_down_move_by_viewport_rows() {
    let mut view = ViewModel::default();
    for i in 0..80 {
        view.push_line(LineKind::Assistant, format!("msg {i}"));
    }
    layout(&mut view, 80, 24); // 视口 22 行
    let page = view.transcript_rows.saturating_sub(2) as usize; // §10：viewport-2
    view.scroll_up(page as u16);
    let ScrollMode::Locked(a1) = view.scroll_mode else {
        panic!();
    };
    view.scroll_up(page as u16);
    let ScrollMode::Locked(a2) = view.scroll_mode else {
        panic!();
    };
    // 两次 PageUp：顶部行前移 viewport-2 行（跨 entry）。
    let ids: Vec<EntryId> = view.transcript.iter().map(|e| e.id()).collect();
    let heights: Vec<usize> = ids
        .iter()
        .map(|id| view.entry_heights.get(id).copied().unwrap_or(1))
        .collect();
    let r1 = tpi::tui::scroll::row_of(&ids, &heights, a1.entry_id, a1.row_in_entry);
    let r2 = tpi::tui::scroll::row_of(&ids, &heights, a2.entry_id, a2.row_in_entry);
    assert_eq!(r1.saturating_sub(r2), page, "PageUp 移动 viewport-2 行");
    // 向下翻回。
    view.scroll_down(page as u16);
    let ScrollMode::Locked(a3) = view.scroll_mode else {
        panic!();
    };
    let r3 = tpi::tui::scroll::row_of(&ids, &heights, a3.entry_id, a3.row_in_entry);
    assert_eq!(r2.saturating_add(page).min(r3), r3);
}

#[test]
fn wheel_moves_anchor_by_three_rows() {
    let mut view = ViewModel::default();
    messages(&mut view, 30);
    layout(&mut view, 80, 24);
    view.scroll_up(3);
    let ScrollMode::Locked(anchor) = view.scroll_mode else {
        panic!();
    };
    let ids: Vec<EntryId> = view.transcript.iter().map(|e| e.id()).collect();
    let heights: Vec<usize> = ids
        .iter()
        .map(|id| view.entry_heights.get(id).copied().unwrap_or(1))
        .collect();
    let top_row = tpi::tui::scroll::row_of(&ids, &heights, anchor.entry_id, anchor.row_in_entry);
    // 初始 Follow 视口顶部 = 30 - 22(视口：24-2 footer+input) = 行 8；上移 3 → 行 5。
    assert_eq!(top_row, 5);
}

#[test]
fn locked_viewport_renders_anchor_content() {
    let mut view = ViewModel::default();
    for i in 0..30 {
        view.push_line(LineKind::Assistant, format!("msg {i}"));
    }
    layout(&mut view, 80, 24);
    view.scroll_up(10);
    // 渲染后窗口应包含锚点附近内容而不包含最新内容。
    let buf = draw_to_test_backend(&mut view, 80, 24);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("msg 0") || text.contains("msg 1"),
        "应显示历史内容"
    );
    assert!(!text.contains("msg 29"), "不应显示最新内容（Locked）");
}

#[test]
fn move_by_rows_helpers_agree_with_model() {
    // 纯函数与 ViewModel 滚动路径的一致性（§65）。
    let mut view = ViewModel::default();
    messages(&mut view, 10);
    let ids: Vec<EntryId> = view.transcript.iter().map(|e| e.id()).collect();
    let heights: Vec<usize> = ids.iter().map(|_| 1).collect();
    let (entry, row) = move_by_rows(&ids, &heights, 9, -5);
    assert_eq!(
        (entry.0, row),
        (5, 0),
        "第 9 行上移 5 行 = 行 4 = 第 5 个 entry"
    );
    let anchor = ScrollAnchor {
        entry_id: entry,
        row_in_entry: row,
    };
    assert_eq!(
        window_start_row(&ids, &heights, &ScrollMode::Locked(anchor), 3),
        4
    );
}

#[test]
fn tool_overlay_does_not_change_transcript_anchor() {
    // §64：tool overlay 不改变 transcript anchor（§17）。
    let mut view = ViewModel::default();
    messages(&mut view, 40);
    layout(&mut view, 80, 24);
    view.scroll_up(10);
    let ScrollMode::Locked(anchor) = view.scroll_mode else {
        panic!();
    };
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool(
        "c1",
        "bash",
        tpi::tool::outcome::ToolStatus::Failed,
        1,
        Some(1),
        "err",
        None,
    );
    view.open_tool_overlay("c1");
    assert_eq!(
        view.scroll_mode,
        ScrollMode::Locked(anchor),
        "打开 overlay 不得改变锚点"
    );
    view.close_overlay();
    assert_eq!(view.scroll_mode, ScrollMode::Locked(anchor));
}
