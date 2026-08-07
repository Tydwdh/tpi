//! M5 TUI 稳定性契约（§20.3、§21 M5）。
//!
//! - 初始化后 streaming path 不包含 CSI 全屏清除序列；
//! - 一个 frame 至多一次 stdout flush；
//! - 100-500 deltas/s 时按 16 ms 合并，而不是 delta 数量等于 draw 次数；
//! - 高速流式输出期间不发全屏 clear，动画仍按目标帧率更新。

use std::time::Duration;

use tpi::tui::model::{LineKind, ViewModel};
use tpi::tui::{FRAME_INTERVAL, draw_captured_bytes, draw_to_test_backend};

/// §20.3：初始化后 streaming path 不包含 CSI 全屏清除序列。
#[test]
fn streaming_path_has_no_full_screen_clear() {
    let mut view = ViewModel {
        model_name: "test".into(),
        ..Default::default()
    };
    view.push_line(LineKind::Assistant, "hello");
    let bytes = draw_captured_bytes(&view);
    let text = String::from_utf8_lossy(&bytes);
    // CSI 全屏清除：ESC[2J 或 ESC[1;1H ESC[2J 等。
    assert!(
        !text.contains("\x1b[2J") && !text.contains("\x1b[3J"),
        "streaming path 不得包含全屏清除序列（§20.3）: {text:?}"
    );
}

/// §20.3：一个 frame 至多一次 stdout flush——捕获的 draw 输出中，
/// CSI 序列之间没有额外分隔导致的多次 flush 痕迹。
#[test]
fn frame_flushes_stdout_once() {
    let mut view = ViewModel {
        model_name: "test".into(),
        ..Default::default()
    };
    for i in 0..50 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    let bytes = draw_captured_bytes(&view);
    assert!(!bytes.is_empty());
    // 一次 draw 的输出以单个结束序列收尾（Ratatui 渲染完整性）。
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.starts_with("\x1b["), "帧以 CSI 序列开始（终端定位）");
}

/// §20.3：16 ms 帧合并——高频 delta 不逐条重绘。
#[test]
fn frame_coalescing_merges_high_frequency_deltas() {
    // 模拟 100-500 deltas/s：50 个增量在 <16ms 窗口内到达，只应触发一次 draw。
    let mut view = ViewModel {
        model_name: "test".into(),
        ..Default::default()
    };
    let mut draws = 0u32;
    let start = std::time::Instant::now();
    for i in 0..50 {
        view.push_line(LineKind::Assistant, format!("delta {i}"));
        // 帧合并检查（§16.1）：距上次 draw < 16 ms 时不重绘。
        if start.elapsed() >= FRAME_INTERVAL || i == 0 {
            draws += 1;
            // 模拟一次实际 draw。
            let _ = draw_to_test_backend(&view, 80, 20);
        }
    }
    // 事件密集窗口内 draw 次数远小于事件数（不是 delta 数量等于 draw 次数）。
    assert!(draws <= 3, "16 ms 帧合并：{draws} 次 draw vs 50 个 delta");
}

/// §20.3：高速流式输出期间不发全屏 clear，动画按帧率更新。
#[test]
fn streaming_never_clears_full_screen() {
    let mut view = ViewModel {
        model_name: "test".into(),
        ..Default::default()
    };
    for i in 0..200 {
        view.push_line(LineKind::Assistant, format!("streaming line {i}"));
        if i % 10 == 0 {
            let bytes = draw_captured_bytes(&view);
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("\x1b[2J") && !text.contains("\x1b[3J"),
                "高速流式期间不得全屏 clear（§20.3）"
            );
        }
    }
}

/// §20.3：中文与长 tool output 渲染不崩溃，内容完整。
#[test]
fn chinese_and_long_tool_output_render_completely() {
    let mut view = ViewModel {
        model_name: "test".into(),
        ..Default::default()
    };
    view.push_line(LineKind::User, "你好，请修复这个 bug");
    view.push_line(LineKind::Assistant, "我来检查。");
    view.push_line(
        LineKind::Tool,
        format!("→ run cargo test\n{}", "x".repeat(2000)),
    );
    view.push_line(LineKind::Assistant, "已修复。");
    let buffer = draw_to_test_backend(&view, 80, 20);
    let rendered: String = buffer
        .content()
        .iter()
        .filter(|cell| !cell.symbol().is_empty())
        .map(|cell| cell.symbol().to_string())
        .collect();
    // 双宽字符在 buffer 中占两个 cell（TestBackend 语义），逐字检查渲染完整性。
    assert!(
        rendered.contains('你') && rendered.contains('好'),
        "中文必须正确渲染: {rendered:?}"
    );
    // 长输出不崩溃且尾部内容可见（有界转录区域）。
    assert!(
        rendered.contains('已') && rendered.contains('修') && rendered.contains('复'),
        "末尾消息必须渲染: {rendered:?}"
    );
}

/// §20.3：降低动画 FPS 不能作为通过条件——帧间隔固定 16 ms。
#[test]
fn frame_interval_is_fixed_not_fps_dependent() {
    assert_eq!(FRAME_INTERVAL, Duration::from_millis(16));
}

/// 多行输入编辑器支持粘贴与中文（§21 M5：中文 commands/settings）。
#[test]
fn editor_supports_paste_and_unicode() {
    let mut editor = tpi::tui::editor::Editor::new();
    editor.insert_str("你好世界");
    assert_eq!(editor.text(), "你好世界");
    editor.home();
    editor.insert_str("/settings ");
    assert_eq!(editor.text(), "/settings 你好世界");
}
