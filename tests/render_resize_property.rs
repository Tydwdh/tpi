//! §26/§31 property：随机终端尺寸 + 持续输出下渲染不 panic、buffer 尺寸正确。
//!
//! 覆盖 width 40~220、height 10~70（任务书 §26 的随机 resize 范围），
//! 同一 view 先 Follow 后 Locked 各渲染一次。

use proptest::prelude::*;
use tpi::tui::draw_to_test_backend_mode;
use tpi::tui::model::{LineKind, ViewModel};
use tpi::tui::terminal::ViewMode;

fn busy_view() -> ViewModel {
    let mut view = ViewModel {
        model_name: "prop".into(),
        workspace: "tpi".into(),
        ..Default::default()
    };
    for i in 0..30 {
        view.push_line(LineKind::Assistant, format!("第 {i} 行 中文 混排 😀"));
    }
    view.begin_tool("c", "bash", Some("cargo test -- --nocapture".into()), None);
    view.finish_tool(
        "c",
        "bash",
        tpi::tool::outcome::ToolStatus::Failed,
        1234,
        Some(101),
        "error[E0308]",
        None,
    );
    view
}

proptest! {
    #[test]
    fn render_at_random_sizes_never_panics(
        w in 40usize..=220usize,
        h in 10usize..=70usize,
    ) {
        let mut view = busy_view();
        // Follow 模式。
        let buf = draw_to_test_backend_mode(&mut view, w as u16, h as u16, ViewMode::Fullscreen);
        prop_assert_eq!(buf.area().width as usize, w);
        prop_assert_eq!(buf.area().height as usize, h);
        // Locked（历史浏览）模式。
        view.scroll_up(8);
        let buf2 = draw_to_test_backend_mode(&mut view, w as u16, h as u16, ViewMode::Fullscreen);
        prop_assert_eq!(buf2.area().width as usize, w);
    }
}
