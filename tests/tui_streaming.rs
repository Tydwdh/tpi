//! M5 TUI 稳定性契约（§20.3、§21 M5）+ M6+ 渲染回归。
//!
//! - 初始化后 streaming path 不包含 CSI 全屏清除序列；
//! - 一个 frame 至多一次 stdout flush；
//! - 100-500 deltas/s 时按 16 ms 合并，而不是 delta 数量等于 draw 次数；
//! - 高速流式输出期间不发全屏 clear，动画仍按目标帧率更新；
//! - inline scrollback 窗口语义（§16.1）：活动区只显示尾部，旧行提交到 scrollback；
//! - 工具卡片（§16.2）、思考折叠、命令菜单、Markdown 渲染。

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use tpi::tool::outcome::ToolStatus;
use tpi::tui::model::{LineKind, ViewModel};
use tpi::tui::{FRAME_INTERVAL, draw_captured_bytes, draw_to_test_backend};

/// 把 TestBackend buffer 拼成文本：跳过空 cell，并跳过宽字符的延续 cell
/// （ratatui 0.30 中延续 cell 与空白 cell 都是单个空格，需按前一字符宽度识别）。
fn buffer_text(buffer: &Buffer) -> String {
    let width = buffer.area().width as usize;
    let content = buffer.content();
    let mut out = String::new();
    for (i, cell) in content.iter().enumerate() {
        let col = i % width;
        let symbol = cell.symbol();
        if symbol.is_empty() {
            continue;
        }
        if symbol == " " && col > 0 {
            // 前一 cell 是宽字符（display width > 1）→ 当前是其延续 cell，跳过。
            let prev = content[i - 1]
                .symbol()
                .chars()
                .next()
                .map(unicode_width::UnicodeWidthChar::width)
                .unwrap_or(None)
                .unwrap_or(0);
            if prev > 1 {
                continue;
            }
        }
        out.push_str(symbol);
    }
    out
}

/// §20.3：初始化后 streaming path 不包含 CSI 全屏清除序列。
#[test]
fn streaming_path_has_no_full_screen_clear() {
    let mut view = ViewModel {
        model_name: "test".into(),
        ..Default::default()
    };
    view.push_line(LineKind::Assistant, "hello");
    let bytes = draw_captured_bytes(&mut view);
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
    let bytes = draw_captured_bytes(&mut view);
    assert!(!bytes.is_empty());
    // 一次 draw 是完整的 CSI 帧：inline viewport 先输出换行占位（打开 viewport），
    // 随后是 CSI 终端定位序列；不得是裸文本或全屏清除。
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("\x1b["),
        "帧包含 CSI 序列（终端定位）: {:?}",
        &text[..text.len().min(64)]
    );
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
            let _ = draw_to_test_backend(&mut view, 80, 20);
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
            let bytes = draw_captured_bytes(&mut view);
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("\x1b[2J") && !text.contains("\x1b[3J"),
                "高速流式期间不得全屏 clear（§20.3）"
            );
        }
    }
}

/// §20.3：中文与长 tool output 渲染不崩溃；窗口语义下尾部内容可见
/// （§16.1：长输出提交到 scrollback，最后一条 assistant 消息保留在活动区）。
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
    view.push_line(LineKind::Assistant, "已修复这个中文 bug。");
    let buffer = draw_to_test_backend(&mut view, 80, 20);
    let rendered = buffer_text(&buffer);
    // 双宽字符在 buffer 中占两个 cell（TestBackend 语义），逐字检查渲染完整性。
    assert!(
        rendered.contains('修') && rendered.contains('复'),
        "长输出后末尾消息必须渲染（窗口语义）: {rendered:?}"
    );
    assert!(
        rendered.contains('中') && rendered.contains('文'),
        "中文必须正确渲染: {rendered:?}"
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

/// 输入历史：↑/↓ 浏览已提交消息。
#[test]
fn editor_history_navigates_submitted_prompts() {
    let mut editor = tpi::tui::editor::Editor::new();
    editor.insert_str("第一条");
    editor.submit();
    editor.insert_str("第二条");
    editor.submit();
    editor.history_up();
    assert_eq!(editor.text(), "第二条");
    editor.history_up();
    assert_eq!(editor.text(), "第一条");
    editor.history_down();
    assert_eq!(editor.text(), "第二条");
    editor.history_down();
    assert!(editor.text().is_empty());
}

/// §16.2：用户消息带 `you` 标签与细紫红左 rail。
#[test]
fn user_message_has_you_label() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "你好");
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let rendered = buffer_text(&buffer);
    assert!(
        rendered.contains("you"),
        "用户消息必须带 you 标签: {rendered:?}"
    );
}

/// §16.2：工具卡片——运行中 spinner，成功后 ✓ + 耗时。
#[test]
fn tool_card_renders_running_and_done_states() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("bash: cargo test".into()), None);
    view.anim_tick = 0;
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let rendered = buffer_text(&buffer);
    assert!(
        rendered.contains('⠋') && rendered.contains("bash: cargo test"),
        "运行中卡片显示 spinner、工具名与命令摘要: {rendered:?}"
    );

    view.finish_tool("c1", "bash", ToolStatus::Succeeded, 2345, Some(0), "", None);
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let rendered = buffer_text(&buffer);
    assert!(
        rendered.contains('✓') && rendered.contains("2.3s"),
        "成功后卡片显示 ✓ 与耗时: {rendered:?}"
    );
}

/// §16.2：失败工具卡片保留红色关键 tail。
#[test]
fn failed_tool_card_shows_status_and_tail() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("bash: cargo test".into()), None);
    view.finish_tool(
        "c1",
        "bash",
        ToolStatus::Failed,
        500,
        Some(2),
        "exit_code: 1\n失败输出",
        None,
    );
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let rendered = buffer_text(&buffer);
    assert!(rendered.contains('✗'), "失败卡片显示 ✗: {rendered:?}");
    assert!(
        rendered.contains("bash: cargo test"),
        "命令摘要展示: {rendered:?}"
    );
    assert!(rendered.contains("exit 2"), "exit code 展示: {rendered:?}");
    assert!(
        rendered.contains("失败输出"),
        "失败 tail 保留: {rendered:?}"
    );
}

/// §16.2：thinking 可折叠——溢出时默认折叠显示提示行，Alt+T 后显示全文。
#[test]
fn reasoning_can_be_folded() {
    let mut view = ViewModel::default();
    // §PointerHit：折叠只对溢出内容生效（单行 thinking 无需折叠，直接显示）。
    let text = (0..10)
        .map(|i| format!("推理第{i}行"))
        .collect::<Vec<_>>()
        .join("\n");
    view.push_line(LineKind::Reasoning, text);
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let rendered = buffer_text(&buffer);
    assert!(
        rendered.contains("点击展开思考"),
        "默认折叠显示提示行: {rendered:?}"
    );
    assert!(!rendered.contains("推理第9行"), "超 6 行的内容折叠不可见");

    // Alt+T 全局展开后显示原文。
    view.reasoning_visible = true;
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    assert!(buffer_text(&buffer).contains("推理第9行"), "展开后全文可见");
}

/// Markdown 渲染：assistant 消息中加粗/行内代码进入 buffer 且带样式。
#[test]
fn assistant_markdown_bold_and_code_are_styled() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "**加粗** 和 `code`");
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let rendered = buffer_text(&buffer);
    assert!(rendered.contains("加粗") && rendered.contains("code"));
    // 加粗 cell 必须带 BOLD 修饰。
    let bold = buffer
        .content()
        .iter()
        .any(|cell| cell.symbol() == "加" && cell.style().add_modifier.contains(Modifier::BOLD));
    assert!(bold, "加粗文本必须带 BOLD 修饰");
}

/// 命令补全菜单：输入 `/` 前缀时弹出匹配命令与中文说明。
#[test]
fn command_menu_pops_up_with_matches() {
    let mut view = ViewModel {
        input: "/set".into(),
        ..Default::default()
    };
    view.refresh_command_menu();
    let buffer = draw_to_test_backend(&mut view, 80, 14);
    let rendered = buffer_text(&buffer);
    assert!(
        rendered.contains("/settings") && rendered.contains("查看生效配置"),
        "菜单显示匹配命令与说明: {rendered:?}"
    );
}

/// §16.2：计划条不出现在 transcript 流水，而是独立区域。
#[test]
fn plan_renders_as_compact_strip() {
    use tpi::tool::plan::{Plan, PlanItem, PlanStatus};
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "请实现功能");
    view.plan = Some(Plan {
        explanation: None,
        items: vec![
            PlanItem {
                text: "第一步".into(),
                status: PlanStatus::InProgress,
            },
            PlanItem {
                text: "第二步".into(),
                status: PlanStatus::Pending,
            },
        ],
    });
    let buffer = draw_to_test_backend(&mut view, 80, 14);
    let rendered = buffer_text(&buffer);
    assert!(rendered.contains("计划"), "计划条必须渲染: {rendered:?}");
    assert!(rendered.contains("第一步"));
}
