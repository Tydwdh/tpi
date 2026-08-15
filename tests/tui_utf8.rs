//! T1：UTF-8 P0 回归测试（TPI_TUI_V2_TASK §24、§54）。
//!
//! 覆盖：中文/日本語/emoji/ZWJ/combining/全角 在各类有界截断路径上
//! 绝不 panic、绝不产出非法 UTF-8、长度不超预算。
//! 回归点：push_stream_delta（message cap）、append_tool_output（card cap
//! 中段丢弃）、finish_tool tail（bound_output 尾部窗口）、command 渲染截断。

use tpi::outcome::ToolStatus;
use tpi::tui::model::{Entry, LineKind, MAX_CARD_OUTPUT, MAX_MESSAGE_CHARS, ViewModel};

/// 混入多字节字符的流：中文、日文、emoji、ZWJ、combining、全角。
fn multibyte_flood(unit: &str, total_bytes: usize) -> String {
    let mut s = String::new();
    while s.len() < total_bytes {
        s.push_str(unit);
    }
    s
}

#[test]
fn cjk_emoji_over_card_cap_never_panics_and_stays_bounded() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd".into()), Some("cmd".into()));
    let unit = "中文日本語😀👨\u{200d}💻e\u{301}ＡＢ";
    let big = multibyte_flood(unit, MAX_CARD_OUTPUT * 3);
    // 分块推送，命中中段丢弃路径（drain 起点在字符边界内）。
    // 按字符边界分块，避免把多字节字符切成两半（测试自身不引入非法输入）。
    let chars: Vec<char> = big.chars().collect();
    for chunk in chars.chunks(2048) {
        let chunk: String = chunk.iter().collect();
        view.append_tool_output("c1", chunk);
    }
    // 单块超大（远超剩余预算）：命中尾部窗口路径。
    view.append_tool_output("c1", multibyte_flood("👨\u{200d}💻", MAX_CARD_OUTPUT * 2));
    let card = view.live.tools.get("c1").expect("live 区必须有卡片");
    let output = card.card.output.as_ref().expect("输出必须累积");
    assert!(output.len() <= MAX_CARD_OUTPUT, "len={}", output.len());
    assert!(output.is_char_boundary(0) && output.is_char_boundary(output.len()));
}

#[test]
fn cjk_emoji_over_message_cap_never_panics_and_is_valid() {
    let mut view = ViewModel::default();
    let unit = "中文日本語😀👨\u{200d}💻e\u{301}";
    // 先填满到接近上限，再推入会越界的增量。
    view.push_stream_delta(
        LineKind::Assistant,
        &multibyte_flood(unit, MAX_MESSAGE_CHARS - 4),
    );
    view.push_stream_delta(LineKind::Assistant, unit);
    let msg = view.live.assistant.as_ref().expect("live 区必须有消息");
    assert!(
        msg.text.len() <= MAX_MESSAGE_CHARS + 32,
        "len={}",
        msg.text.len()
    );
    assert!(msg.text.is_char_boundary(msg.text.len()));
    // 截断标记只出现在字符边界处。
    if let Some(pos) = msg.text.find("truncated") {
        assert!(msg.text[..pos].is_char_boundary(pos));
    }
}

#[test]
fn finish_tool_huge_cjk_tail_bounds_without_panic() {
    let mut view = ViewModel::default();
    view.begin_tool("c2", "bash", Some("cmd".into()), Some("cmd".into()));
    let tail = multibyte_flood("错误👨\u{200d}💻中文e\u{301}", MAX_CARD_OUTPUT * 2);
    view.finish_tool(
        ("c2", "bash"),
        ToolStatus::Failed,
        1000,
        Some(101),
        tail,
        None,
    );
    let Entry::Tool { card, .. } = &view.transcript[0] else {
        panic!("必须是工具卡片");
    };
    let output = card.output.as_ref().expect("失败必须保留输出");
    assert!(output.len() <= MAX_CARD_OUTPUT, "len={}", output.len());
    assert!(output.starts_with('…'));
    assert!(output.is_char_boundary(output.len()));
    let tail_bounded = card.tail.as_ref().expect("失败必须有 tail");
    assert!(tail_bounded.chars().count() <= 241);
}

#[test]
fn mixed_ascii_cjk_command_truncation_keeps_utf8() {
    // command 主行渲染截断（truncate_display 按 char 宽度）：不 panic、不切字节。
    let mut view = ViewModel::default();
    view.begin_tool(
        "c3",
        "bash",
        Some("中文命令".repeat(60)),
        Some("中文命令".repeat(60)),
    );
    let buf = tpi::tui::draw_to_test_backend(&mut view, 80, 24);
    let text: String = buf
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains('…') || text.len() >= 10);
    // 渲染后的内容必须全部是合法字符（cell.symbol 不 panic 即通过）。
}

#[test]
fn reasoning_zwj_and_fullwidth_render_safely() {
    let mut view = ViewModel::default();
    view.push_line(
        LineKind::Reasoning,
        "思考👨\u{200d}💻e\u{301}全角ＡＢＣ".repeat(20),
    );
    let _buf = tpi::tui::draw_to_test_backend(&mut view, 60, 20);
}

#[test]
fn overlay_body_huge_multibyte_stays_valid() {
    let mut view = ViewModel::default();
    view.begin_tool("c4", "read", Some("a.rs".into()), None);
    let big = multibyte_flood("中文😀e\u{301}", MAX_CARD_OUTPUT * 2);
    view.append_tool_output("c4", &big);
    view.finish_tool(("c4", "read"), ToolStatus::Succeeded, 5, None, &big, None);
    view.open_tool_overlay("c4");
    let overlay = view.overlay.as_ref().expect("overlay 必须打开");
    assert!(overlay.body.is_char_boundary(overlay.body.len()));
}
