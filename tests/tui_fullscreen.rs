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
    // §用户诉求：侧边栏默认打开，但这些测试测主区/scrollbar/footer 布局，
    // 需在 sidebar 关闭下运行（主区占满全宽）。
    view.sidebar.open = false;
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
    // 24 行终端：footer(1) + input(1) + transcript 占其余（无 header）。
    let mut view = busy_view(30);
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let texts = row_texts(&buf);
    // 视觉瘦身：无常驻 header，transcript 从第 0 行开始。
    assert!(
        !texts.first().map(|s| s.contains("TPI")).unwrap_or(false),
        "无常驻 header（信息并入 footer）: {:?}",
        texts.first()
    );
    // §美化：消息块间插留白空行——首行可能是留白+scrollbar 轨道；
    // 断言 transcript 区（前几行）有消息正文。
    assert!(
        texts.iter().take(3).any(|s| s.contains("第")),
        "transcript 区必须有消息内容: {:?}",
        &texts[..3.min(texts.len())]
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
    assert!(
        all.contains("Ctrl+End") && all.contains("返回最新"),
        "提示必须写明 Ctrl+End（此前误写 End）: {all:?}"
    );
}

/// History browsing (Locked): footer must show the state hint; Follow must not.
#[test]
fn footer_shows_history_browsing_indicator_when_locked() {
    let mut view = busy_view(100);
    view.scroll_up(50);
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    assert!(
        all.contains("历史浏览中"),
        "Locked must show the history browsing hint: {all:?}"
    );

    let mut view2 = busy_view(100);
    let buf2 = draw_to_test_backend_mode(&mut view2, 80, 24, ViewMode::Fullscreen);
    let all2: String = row_texts(&buf2).join("\n");
    assert!(
        !all2.contains("历史浏览中"),
        "Follow must NOT show the history browsing hint: {all2:?}"
    );
}

#[test]
fn footer_shows_transient_hint() {
    let mut view = busy_view(3);
    view.transient_hint = Some("没有用户消息可跳转".into());
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    assert!(
        all.contains("没有用户消息可跳转"),
        "footer must show the transient hint: {all:?}"
    );

    let mut view2 = busy_view(3);
    let buf2 = draw_to_test_backend_mode(&mut view2, 80, 24, ViewMode::Fullscreen);
    let all2: String = row_texts(&buf2).join("\n");
    assert!(
        !all2.contains("没有用户消息可跳转"),
        "no hint by default: {all2:?}"
    );
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
        } else if sym == "█" {
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

/// PM：分隔线按终端宽度铺满——窄屏不折行（此前固定 40 个 ─ 在 30 列会折成两行）。
#[test]
fn system_separator_fills_width_without_wrap() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "内容");
    view.push_line(LineKind::System, "─".repeat(40));
    let buf = draw_to_test_backend_mode(&mut view, 30, 12, ViewMode::Fullscreen);
    let texts = row_texts(&buf);
    // 视觉瘦身：无常驻 header。§美化：底部新增 footer 分隔线（也铺满 ─）；
    // 只统计 transcript 区（去掉底部 input/rule/footer 三行）——system 分隔线
    // 必须单行铺满不折行。
    let transcript_rows = &texts[..texts.len().saturating_sub(3)];
    let rule_rows = transcript_rows
        .iter()
        .map(|s| {
            let s = s.trim_end_matches('│').trim(); // 去掉 scrollbar 列
            !s.is_empty() && s.chars().all(|c| c == '─')
        })
        .filter(|is_rule| *is_rule)
        .count();
    assert_eq!(rule_rows, 1, "分隔线必须单行铺满（不折行）: {texts:?}");
}
/// PM：窄屏（40 列）下工具卡主行仍保持单行（此前预算不足时可能折行）。
#[test]
fn tool_card_stays_single_line_on_narrow_terminal() {
    let mut view = ViewModel::default();
    view.sidebar.open = false; // 测主区卡片布局，sidebar 关闭。
    let long = format!("cargo test -- --nocapture {}", "x".repeat(200));
    view.begin_tool("c", "bash", Some(long.clone()), Some(long));
    view.finish_tool(
        ("c", "bash"),
        tpi::tool::outcome::ToolStatus::Failed,
        123_456,
        Some(101),
        "error[E0308]",
        None,
    );
    let buf = draw_to_test_backend_mode(&mut view, 40, 12, ViewMode::Fullscreen);
    let rows: Vec<&[ratatui::buffer::Cell]> = buf.content().chunks(40).collect();
    let card_row = rows
        .iter()
        .position(|r| r.iter().any(|cell| cell.symbol() == "✗"))
        .expect("卡片行存在");
    let width = 40;
    let following = rows[card_row + 1..]
        .iter()
        .take_while(|r| r[..width - 1].iter().any(|cell| cell.symbol() != " ")) // 排除 scrollbar 列
        .count();
    assert!(
        following <= 1,
        "窄屏卡片主行必须单行（后续最多 1 行失败 tail）: following={following}"
    );
}

/// PM：成功工具卡不再显示 `exit 0`（噪声），失败卡仍显示退出码。
#[test]
fn success_card_hides_exit_zero_failure_keeps_it() {
    let mut view = ViewModel::default();
    view.begin_tool("c", "bash", Some("cargo test".into()), None);
    view.finish_tool(
        ("c", "bash"),
        tpi::tool::outcome::ToolStatus::Succeeded,
        100,
        Some(0),
        "",
        None,
    );
    let buf = draw_to_test_backend_mode(&mut view, 80, 12, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    assert!(!all.contains("exit"), "成功卡不应显示 exit 文本: {all:?}");

    let mut view2 = ViewModel::default();
    view2.begin_tool("c", "bash", Some("cargo test".into()), None);
    view2.finish_tool(
        ("c", "bash"),
        tpi::tool::outcome::ToolStatus::Failed,
        100,
        Some(101),
        "boom",
        None,
    );
    let buf2 = draw_to_test_backend_mode(&mut view2, 80, 12, ViewMode::Fullscreen);
    let all2: String = row_texts(&buf2).join("\n");
    assert!(all2.contains("exit101"), "失败卡必须显示退出码: {all2:?}");
}

/// 多行输入时 footer 显示行数提示（人体工学：>8 行内部滚动也可见）。
#[test]
fn footer_shows_multi_line_input_hint() {
    let mut view = ViewModel::default();
    view.input = "第一行\n第二行\n第三行".into();
    view.input_cursor = view.input.len();
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    assert!(all.contains("输入3行"), "多行输入必须显示行数: {all:?}");

    let mut view2 = ViewModel {
        input: "单行".into(),
        ..ViewModel::default()
    };
    let buf2 = draw_to_test_backend_mode(&mut view2, 80, 24, ViewMode::Fullscreen);
    let all2: String = row_texts(&buf2).join("\n");
    assert!(!all2.contains("输入"), "单行输入不应显示行数: {all2:?}");
}

/// §用户诉求（恢复会话可判断）：/sessions 菜单名字（首条消息）置前为主列，
/// UUID 缩短为辅助列——不再让完整哈希抢视觉主体。
#[test]
fn sessions_menu_shows_name_first_short_id_second() {
    let mut view = ViewModel::default();
    let id = "019feea2-e01d-7c00-be52-bf10a5f44d3c";
    view.menu = Some(tpi::tui::model::MenuView {
        items: vec![(
            id.to_string(),
            "修复测试 · 08-11 04:06 · 543 事件".to_string(),
        )],
        selected: 0,
        kind: tpi::tui::model::MenuKind::Session,
        session_previews: Vec::new(),
    });
    // 宽屏：完整布局——名字主列 + 短 id 辅助列，完整 UUID 不再出现。
    let buf = draw_to_test_backend_mode(&mut view, 160, 24, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    assert!(all.contains("修复测试"), "名字必须置前显示: {all:?}");
    // 注意：row_texts 收集时跳过空格，故用无空格形式匹配渲染内容。
    assert!(
        all.contains("(id019feea2-e01d…)"),
        "短 id 辅助显示: {all:?}"
    );
    assert!(
        !all.contains("019feea2-e01d-7c00-be52-bf10a5f44d3c"),
        "完整 UUID 不得再作为视觉主体: {all:?}"
    );
    // 窄屏（80 列）：辅助列被截断，但名字（主列）必须完整保留。
    let buf_narrow = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let narrow: String = row_texts(&buf_narrow).join("\n");
    assert!(narrow.contains("修复测试"), "窄屏名字仍须保留: {narrow:?}");
    assert!(
        !narrow.contains("019feea2-e01d-7c00-be52-bf10a5f44d3c"),
        "窄屏也不得显示完整 UUID: {narrow:?}"
    );
}

/// 长菜单（/sessions 多会话）可视窗口跟随选中项：选中项始终可见，窗口外项隐藏。
#[test]
fn long_menu_window_follows_selection() {
    let mut view = ViewModel::default();
    let items: Vec<(String, String)> = (0..20)
        .map(|i| (format!("sess-{i:02}"), String::new()))
        .collect();
    view.menu = Some(tpi::tui::model::MenuView {
        items,
        selected: 15,
        kind: tpi::tui::model::MenuKind::Session,
        session_previews: Vec::new(),
    });
    let buf = draw_to_test_backend_mode(&mut view, 80, 24, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    assert!(all.contains("sess-15"), "选中项必须可见: {all:?}");
    assert!(all.contains("…"), "超长菜单必须显示省略号: {all:?}");
    assert!(!all.contains("sess-00"), "窗口外顶部项不应显示: {all:?}");
}

/// §用户诉求：/sessions 的对话预览显示在悬浮窗（Modal）里，而非塞进菜单列表——
/// 悬浮窗显示选中会话的 `你/AI` 对话，底部菜单保持紧凑列表。
#[test]
fn session_preview_renders_in_modal_float() {
    let preview = vec![
        tpi::tui::model::MenuPreviewLine {
            is_user: true,
            text: "用户问题".into(),
        },
        tpi::tui::model::MenuPreviewLine {
            is_user: false,
            text: "AI 回答".into(),
        },
    ];
    let mut view = ViewModel {
        menu: Some(tpi::tui::model::MenuView {
            items: vec![
                ("sess-1".into(), "会话 1 · 时间 · 事件".into()),
                ("sess-2".into(), "会话 2 · 时间 · 事件".into()),
            ],
            selected: 1,
            kind: tpi::tui::model::MenuKind::Session,
            session_previews: vec![Vec::new(), preview.clone()],
        }),
        modal: Some(tpi::tui::model::ModalState::new(
            "/sessions",
            tpi::tui::model::preview_lines_to_body(&preview),
        )),
        ..Default::default()
    };
    let buf = draw_to_test_backend_mode(&mut view, 96, 24, ViewMode::Fullscreen);
    let all: String = row_texts(&buf).join("\n");
    // 悬浮窗（Modal）显示预览：`你 用户问题` / `AI AI 回答`（跳过空格）。
    assert!(all.contains("用户问题"), "用户消息在悬浮窗中: {all:?}");
    assert!(all.contains("AIAI回答"), "AI 消息在悬浮窗中: {all:?}");
    // 底部菜单仍是紧凑列表（选中项可见）。
    assert!(all.contains("会话2"), "菜单列表选中项可见: {all:?}");
}
