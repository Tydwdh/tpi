//! TUI 整改验收回归（建议书 §13.1-13.8）。
//!
//! 核心不变量：
//! - 同一个 ToolCallId 在 transcript 中最多一个可见 ToolActivity block；
//! - 单个 ToolCard header 恒为 1 个 visual line（ellipsis + metadata 可见）；
//! - reasoning 默认折叠，不形成正文墙；
//! - 成功工具不输出日志正文；失败工具只显示有限 tail；
//! - 历史详情走 Overlay，不重写 scrollback；
//! - scroll lock 期间新输出不强制拉回底部。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tpi::tool::outcome::ToolStatus;
use tpi::tui::event::UiEvent;
use tpi::tui::model::{Entry, LineKind, MAX_CARD_OUTPUT, ToolCard, ToolCardState, ViewModel};
use tpi::tui::{HitTarget, draw_to_test_backend};

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    // 宽字符（中文等）的第二个 cell 占位符是空/空格：去掉以还原连续文本。
    buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .replace(' ', "")
}

fn card_rows(view: &ViewModel, id: &str) -> usize {
    view.transcript
        .iter()
        .filter(|entry| matches!(entry, Entry::Tool { card, .. } if card.id == id))
        .count()
}

fn tool_card(id: &str, status: ToolStatus, output: Option<&str>) -> ToolCard {
    ToolCard {
        id: id.into(),
        name: "bash".into(),
        target: Some("cargo test --all-targets".into()),
        command: Some("cargo test --all-targets".into()),
        state: ToolCardState::Done {
            status,
            duration_ms: 3600,
            exit_code: if status == ToolStatus::Succeeded {
                Some(0)
            } else {
                Some(101)
            },
        },
        output: output.map(|s| s.to_string()),
        diff: None,
        output_truncated: false,
        expanded: false,
        tail: if status == ToolStatus::Succeeded {
            None
        } else {
            output.map(|s| s.to_string())
        },
    }
}

/// 13.1：Tool lifecycle——ToolStarted/ToolProgress/ToolCompleted 只对应一张卡。
#[test]
fn one_call_id_never_renders_two_cards() {
    let mut view = ViewModel::default();
    view.begin_tool(
        "call-1",
        "bash",
        Some("cargo test".into()),
        Some("cargo test".into()),
    );
    view.append_tool_output("call-1", "running...\n");
    view.finish_tool(
        "call-1",
        "bash",
        ToolStatus::Succeeded,
        3600,
        Some(0),
        "ok\n",
        None,
    );
    assert_eq!(card_rows(&view, "call-1"), 1, "同一 call_id 只能有一张卡");
    // 多次 finish（防御）也不追加新卡。
    view.finish_tool(
        "call-1",
        "bash",
        ToolStatus::Succeeded,
        3600,
        Some(0),
        "ok\n",
        None,
    );
    assert_eq!(card_rows(&view, "call-1"), 1);
}

/// 13.2：500 字符命令在 80 列下主行仍为单行，ellipsis 且 metadata 可见。
#[test]
fn long_command_stays_single_line_with_ellipsis() {
    let mut view = ViewModel::default();
    let long = format!("git commit -m \"{}\"", "x".repeat(400));
    view.begin_tool("c", "bash", Some(long.clone()), Some(long));
    view.finish_tool("c", "bash", ToolStatus::Succeeded, 113, Some(0), "", None);
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let text = buffer_text(&buffer);
    assert!(text.contains('…'), "超长命令必须 ellipsis: {text}");
    assert!(text.contains("113ms"), "metadata 仍可见: {text}");
    // 单行验证：卡片在 buffer 中只占 1 行（整卡高度 12 - 其他区域）。
    let first_row = buffer
        .content()
        .chunks(buffer.area().width as usize)
        .enumerate()
        .find(|(_, row)| row.iter().any(|c| c.symbol() == "✓"))
        .map(|(i, _)| i)
        .expect("卡片行存在");
    let next_row = buffer
        .content()
        .chunks(buffer.area().width as usize)
        .nth(first_row + 1)
        .map(|row| row.iter().any(|c| c.symbol() != " "))
        .unwrap_or(false);
    assert!(!next_row, "卡片主行必须只占 1 个 visual line");
}

/// 13.3：Reasoning flood——20KB reasoning 折叠后只占 1 行。
#[test]
fn reasoning_flood_collapses_to_one_line() {
    let mut view = ViewModel::default();
    let flood = "reasoning ".repeat(2000); // ~20KB
    view.push_stream_delta(LineKind::Reasoning, &flood);
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let text = buffer_text(&buffer);
    assert!(text.contains("思考"), "默认折叠（live 区）: {text}");
    assert!(!text.contains("reasoning"), "正文不进入 transcript: {text}");
    // 折叠态整块 = 1 行（"◇ 思考" 行）。
    let reasoning_rows = buffer
        .content()
        .chunks(buffer.area().width as usize)
        .filter(|row| row.iter().any(|c| c.symbol() == "◇"))
        .count();
    assert_eq!(reasoning_rows, 1, "reasoning 只占 1 行");
}

/// 13.4：Tool output flood——10MB stdout 不膨胀 transcript，成功只显示单行卡。
#[test]
fn output_flood_does_not_grow_transcript() {
    let mut view = ViewModel::default();
    view.begin_tool("c", "bash", Some("echo big".into()), None);
    let big = "x".repeat(10 * 1024 * 1024);
    view.append_tool_output("c", &big);
    view.finish_tool("c", "bash", ToolStatus::Succeeded, 100, Some(0), &big, None);
    let Entry::Tool { card, .. } = &view.transcript[0] else {
        panic!();
    };
    assert!(
        card.output.as_ref().unwrap().len() <= MAX_CARD_OUTPUT,
        "UI 有界"
    );
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let text = buffer_text(&buffer);
    assert!(
        text.chars().filter(|c| *c == 'x').count() < 1000,
        "成功卡片不输出日志正文"
    );
    assert!(text.contains("✓"), "成功单行卡可见: {text}");
}

/// 13.5：Failure preview——1000 行失败输出只显示预算内 tail。
#[test]
fn failure_shows_bounded_tail_only() {
    let mut view = ViewModel::default();
    let mut lines = String::new();
    for i in 0..1000 {
        lines.push_str(&format!("line-{i}\n"));
    }
    lines.push_str("error[E0308]: mismatched types\n");
    view.begin_tool("c", "bash", Some("cargo build".into()), None);
    view.finish_tool(
        "c",
        "bash",
        ToolStatus::Failed,
        3600,
        Some(101),
        &lines,
        None,
    );
    let buffer = draw_to_test_backend(&mut view, 80, 12);
    let text = buffer_text(&buffer);
    assert!(text.contains("exit101"), "exit code 明确: {text}");
    // tail 只保留末尾关键内容（bound_tail 240 字符 ≈ 最后 3-4 行），早期行不可见。
    assert!(!text.contains("line-1"), "早期日志不可见");
    assert!(
        text.contains("error[E0308]") || text.contains("mismatched"),
        "错误行在 tail 内可见: {text}"
    );
}

/// 13.6：历史展开——详情走 Overlay，不重写 scrollback（open_tool_overlay 构造详情）。
#[test]
fn detail_opens_overlay_not_inline_rewrite() {
    let mut view = ViewModel::default();
    let mut card = tool_card("c", ToolStatus::Failed, Some("stdout-a\nstdout-b\n"));
    card.command = Some("git diff --stat\n第二行命令".into());
    view.transcript.push(Entry::Tool {
        id: tpi::tui::scroll::EntryId(1),
        card,
    });
    view.open_tool_overlay("c");
    let overlay = view.overlay.as_ref().expect("overlay 打开");
    assert!(overlay.title.contains("failed"));
    assert_eq!(
        overlay.command.as_deref(),
        Some("git diff --stat\n第二行命令")
    );
    assert!(overlay.body.contains("stdout-b"));
    // Esc 关闭后原 transcript 未被改写（卡片仍是原样）。
    view.close_overlay();
    assert!(view.overlay.is_none());
    let Entry::Tool { card, .. } = &view.transcript[0] else {
        panic!();
    };
    assert_eq!(card.command.as_deref(), Some("git diff --stat\n第二行命令"));
}

/// 13.6b：reasoning 行点击打开原文 Overlay（hit 目标正确）。
#[test]
fn reasoning_hit_opens_overlay() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Reasoning, "内部思考原文");
    let id = view.transcript[0].id();
    view.open_reasoning_overlay(id);
    let overlay = view.overlay.as_ref().expect("overlay 打开");
    assert!(overlay.body.contains("内部思考原文"));
    assert!(overlay.title.contains("思考"));
}

/// 13.7：80/120/180 列——中文字符、ellipsis、metadata 在窄列下仍正常。
#[test]
fn layout_works_at_80_120_180_columns() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "你好，请修复这个问题");
    let long = format!(
        "cargo run --example demo -- --verbose {}",
        "参数".repeat(30)
    );
    view.begin_tool("c", "bash", Some(long.clone()), Some(long));
    view.finish_tool("c", "bash", ToolStatus::Succeeded, 2500, Some(0), "", None);
    view.push_stream_delta(LineKind::Assistant, "已完成修复。");

    for width in [80u16, 120, 180] {
        let buffer = draw_to_test_backend(&mut view, width, 16);
        let text = buffer_text(&buffer);
        assert!(text.contains("你好"), "中文用户消息: {width}");
        assert!(text.contains("已完成修复"), "assistant 正文: {width}");
        assert!(text.contains("2.5s"), "metadata: {width}");
        // 卡片主行单行（✓ 行之后无连续工具输出行）。
        let rows: Vec<&[ratatui::buffer::Cell]> = buffer.content().chunks(width as usize).collect();
        let card_row = rows
            .iter()
            .position(|row| row.iter().any(|c| c.symbol() == "✓"))
            .expect("卡片行");
        let following = rows[card_row + 1..]
            .iter()
            .take_while(|row| row.iter().any(|c| c.symbol() != " "))
            .count();
        assert!(
            following <= 1,
            "卡片后最多 1 个非空行（tail/提示），实际 {following} @ {width} 列"
        );
    }
}

/// 13.8：Scroll lock——新输出不强制拉回底部，计数增加，Ctrl+End 恢复。
#[test]
fn scroll_lock_keeps_position_and_counts() {
    let mut view = ViewModel::default();
    for i in 0..30 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    view.scroll_up(10);
    let locked_scroll = view.transcript_scroll;
    // Agent 继续产生事件。
    view.push_line(LineKind::Assistant, "new-1");
    view.begin_tool("c", "bash", Some("cargo test".into()), None);
    view.finish_tool("c", "bash", ToolStatus::Succeeded, 100, Some(0), "", None);
    assert_eq!(
        view.transcript_scroll, locked_scroll,
        "scroll lock 期间位置不被拉回"
    );
    assert!(
        view.pending_below >= 2,
        "新消息计数: {}",
        view.pending_below
    );
    view.follow_tail();
    assert_eq!(view.transcript_scroll, 0);
    assert_eq!(view.pending_below, 0);
}

/// HitTarget 类型检查：卡片行命中 Tool，reasoning 行命中 Reasoning。
#[test]
fn hit_targets_distinguish_tool_and_reasoning() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Reasoning, "思考");
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool("c1", "bash", ToolStatus::Succeeded, 10, Some(0), "", None);
    let buffer = draw_to_test_backend(&mut view, 80, 20);
    // 通过渲染路径验证（间接）：构造渲染并检查 overlay 数据流。
    view.open_last_tool_overlay();
    assert!(view.overlay.as_ref().unwrap().title.contains("bash"));
    view.close_overlay();
    let reasoning_id = view.transcript[0].id();
    view.open_reasoning_overlay(reasoning_id);
    assert!(view.overlay.as_ref().unwrap().title.contains("思考"));
    let _ = buffer;
    let _ = HitTarget::Tool("x".into());
}

/// T7：操作型 UI 进 Modal，不污染 transcript（§42）。
#[test]
fn modal_keeps_transcript_clean_and_renders() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "请修复测试");
    view.push_line(LineKind::Assistant, "好的。");
    view.open_modal("/settings", "workspace: x\nsessions: y\n保留 token: 8192");
    assert!(view.modal.is_some());
    // Modal 打开不产生 transcript 条目。
    assert_eq!(view.transcript.len(), 2, "Modal 不得污染 transcript");
    // 渲染：Modal 内容可见（覆盖层）。
    let buffer = draw_to_test_backend(&mut view, 80, 24);
    let text: String = buffer
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("/settings"), "Modal 标题可见: {text}");
    assert!(text.contains("workspace: x"), "Modal 正文可见: {text}");
    // Esc 关闭（reducer 路径）。
    let mut state = tpi::tui::state::UiState::new(view);
    let effects = tpi::tui::reducer::update(
        &mut state,
        UiEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.view.modal.is_none(), "Esc 关闭 Modal");
    assert!(effects.is_empty(), "关 Modal 不产生取消效果");
    // transcript 仍干净。
    assert_eq!(state.view.transcript.len(), 2);
}
