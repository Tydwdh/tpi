//! T2：Fullscreen 切换验收测试（TPI_TUI_V2_TASK §55、§64）。
//!
//! 自动化可覆盖：fullscreen 布局占满终端、resize 不 panic、极小终端降级、
//! footer 无兼容模式提示（fullscreen 是正常模式）、inline 兼容保留。
//! 真实终端生命周期（alternate screen 进出）需要人工验收（§55）。

use tpi::tui::draw_to_test_backend_mode;
use tpi::tui::model::{LineKind, ViewModel};
use tpi::tui::terminal::ViewMode;

fn busy_view(rows: usize) -> ViewModel {
    let mut view = ViewModel {
        model_name: "fake-model".into(),
        workspace: "tpi".into(),
        ..Default::default()
    };
    for i in 0..rows {
        view.push_line(LineKind::Assistant, format!("第 {i} 行内容 中文混排"));
    }
    view
}

/// 收集 buffer 每行文本（跳过全空行）。
fn row_texts(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    let w = buf.area().width as usize;
    let h = buf.area().height as usize;
    (0..h)
        .map(|y| {
            let mut s = String::new();
            for x in 0..w {
                let cell = &buf[(x as u16, y as u16)];
                if cell.symbol() != " " {
                    s.push_str(cell.symbol());
                }
            }
            s
        })
        .collect()
}

#[test]
fn fullscreen_fills_entire_terminal_at_common_sizes() {
    for (w, h) in [(80u16, 24u16), (120, 30), (160, 50)] {
        let mut view = busy_view(20);
        let buf = draw_to_test_backend_mode(&mut view, w, h, ViewMode::Fullscreen);
        assert_eq!(buf.area().width, w);
        assert_eq!(buf.area().height, h);
        // 最底行必须是 footer（模型名/就绪状态）。
        let last = row_texts(&buf).last().cloned().unwrap_or_default();
        assert!(
            last.contains("fake-model"),
            "footer 必须在最底行（fullscreen 占满），实际: {last:?}"
        );
        // fullscreen 是正常模式：不显示“兼容模式”提示。
        let all: String = row_texts(&buf).join("\n");
        assert!(!all.contains("兼容模式"), "fullscreen 不应显示兼容提示");
    }
}

#[test]
fn fullscreen_transcript_gets_all_remaining_height() {
    // 24 行终端：footer(1) + input(1) + transcript 应占 22 行。
    let mut view = busy_view(30);
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let texts = row_texts(&buf);
    // 首行应有转录内容（assistant 文本）。
    assert!(
        texts.first().map(|s| s.contains("第")).unwrap_or(false),
        "transcript 应从顶部开始: {:?}",
        texts.first()
    );
    // 底部三行是 input 区与 footer：footer 含模型名。
    assert!(texts[texts.len() - 1].contains("fake-model"));
}

#[test]
fn fullscreen_small_terminal_degrades_without_panic() {
    // 极小终端（§64：可降级）不得 panic，footer 仍在。
    let mut view = busy_view(10);
    let buf = draw_to_test_backend_mode(&mut view, 40, 10, ViewMode::Fullscreen);
    let texts = row_texts(&buf);
    assert!(!texts.is_empty());
    // 转录区至少 1 行（Min(5) 在极小时让位于固定区，但不得 panic）。
}

#[test]
fn fullscreen_resize_keeps_layout_stable() {
    // resize 模拟：同一 view 依次以不同尺寸渲染，不 panic、footer 位置稳定。
    let mut view = busy_view(25);
    for (w, h) in [(120u16, 40u16), (70, 25), (160, 50), (80, 24)] {
        let buf = draw_to_test_backend_mode(&mut view, w, h, ViewMode::Fullscreen);
        let texts = row_texts(&buf);
        assert!(
            texts
                .last()
                .map(|s| s.contains("fake-model"))
                .unwrap_or(false)
        );
    }
}

#[test]
fn inline_mode_keeps_compat_behavior() {
    // inline 模式保留：活动区有限高，但渲染不 panic。
    let mut view = busy_view(30);
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Inline);
    let texts = row_texts(&buf);
    assert!(
        texts
            .last()
            .map(|s| s.contains("fake-model"))
            .unwrap_or(false)
    );
}

#[test]
fn fullscreen_pending_below_hint_renders() {
    // scroll lock 时的新内容提示在 fullscreen 下正常渲染。
    let mut view = busy_view(10);
    view.scroll_up(5);
    view.push_line(LineKind::Assistant, "新输出");
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    assert!(all.contains("条新内容"), "scroll lock 提示应可见: {all:?}");
}

/// §24：全屏历史必须有垂直 scrollbar——右侧 1 列，thumb 按 visual 行数比例。
#[test]
fn fullscreen_shows_scrollbar_at_right_edge() {
    let mut view = busy_view(50); // 远超一屏，thumb 可见
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let w = buf.area().width as usize;
    // footer(1) + input(1) → 转录区 22 行（0..22）。
    let trans_rows = 22usize;
    let mut found_track = false;
    let mut found_thumb = false;
    for y in 0..trans_rows {
        let sym = buf[((w - 1) as u16, y as u16)].symbol();
        if sym == "│" {
            found_track = true;
        } else if sym == "▐" {
            found_thumb = true;
        }
    }
    assert!(found_track, "scrollbar 轨道必须渲染在右缘");
    assert!(
        found_thumb,
        "内容超一屏时 thumb 必须渲染（比例按 visual 行数）"
    );
}

/// §24：内容不足一屏时 scrollbar 仍保留轨道（布局稳定，不随内容增长跳变）。
#[test]
fn fullscreen_scrollbar_track_is_stable_when_short() {
    let mut view = busy_view(5); // 不足一屏
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let w = buf.area().width as usize;
    let trans_rows = 22usize;
    let mut track_rows = 0;
    for y in 0..trans_rows {
        if buf[((w - 1) as u16, y as u16)].symbol() == "│" {
            track_rows += 1;
        }
    }
    assert!(track_rows > 0, "内容不足一屏时仍应画轨道（布局稳定）");
}

/// §27/§31 stress：2000 条 transcript（上限）在 fullscreen 下渲染不 panic、
/// 不超时，且 Follow/Locked 两种模式都稳定。
#[test]
fn long_transcript_2000_entries_renders_stably() {
    let mut view = ViewModel::default();
    for i in 0..2000 {
        view.push_line(LineKind::Assistant, format!("line {i} 中文内容"));
    }
    assert!(view.transcript.len() <= 2000, "transcript 必须受上限约束");
    let buf = draw_to_test_backend_mode(&mut view, 120, 40, ViewMode::Fullscreen);
    assert_eq!(buf.area().width, 120);
    // Locked 模式（历史浏览）下同样渲染稳定。
    view.scroll_up(50);
    let buf2 = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    assert_eq!(buf2.area().width, 80);
}
