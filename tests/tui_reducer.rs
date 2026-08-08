//! T3：UiState + Reducer 单向流测试（TPI_TUI_V2_TASK §26-27、§56）。
//!
//! 断言：状态可重放（相同事件序列 → 相同状态）、reducer 不产生副作用
//! （跨边界动作只以 effect 返回）、键盘路由优先级（§11）。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tpi::agent::{DeltaKind, RuntimeEvent};
use tpi::tool::outcome::ToolStatus;
use tpi::tui::effect::UiEffect;
use tpi::tui::event::UiEvent;
use tpi::tui::model::{LineKind, StatusLine, ViewModel};
use tpi::tui::reducer;
use tpi::tui::scroll::ScrollMode;
use tpi::tui::state::UiState;

fn key(code: KeyCode) -> UiEvent {
    UiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn state() -> UiState {
    UiState::new(ViewModel::default())
}

#[test]
fn typing_and_enter_produces_pending_message() {
    let mut s = state();
    for ch in "你好 world".chars() {
        reducer::update(
            &mut s,
            UiEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
        );
    }
    assert_eq!(s.editor.text(), "你好 world");
    assert_eq!(
        s.view.input, "你好 world",
        "输入投影必须同步（§25 双状态消除）"
    );
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.pending_messages.front().map(String::as_str),
        Some("你好 world")
    );
    assert!(s.editor.text().is_empty(), "提交后清空编辑区");
}

#[test]
fn event_sequence_is_replayable() {
    // 同一事件序列跑两次，状态必须一致（§56：状态可重放、可单测）。
    let events = [
        UiEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        UiEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
        UiEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        UiEvent::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        UiEvent::MouseScrollUp,
        UiEvent::MouseScrollDown,
        UiEvent::Paste("中文😀".into()),
        UiEvent::Tick,
        UiEvent::Agent(RuntimeEvent::AssistantDelta {
            request_id: tpi::ids::RequestId::new_v7(),
            kind: DeltaKind::Text,
            text: "流式内容".into(),
        }),
        UiEvent::Agent(RuntimeEvent::ContextUsage {
            projected: 100,
            usable: 1000,
        }),
    ];
    let mut s1 = state();
    let mut s2 = state();
    for e in events.iter().cloned() {
        reducer::update(&mut s1, e.clone());
        reducer::update(&mut s2, e);
    }
    assert_eq!(s1.editor.text(), s2.editor.text());
    assert_eq!(s1.editor.cursor, s2.editor.cursor);
    assert_eq!(s1.view.input, s2.view.input);
    assert_eq!(s1.view.transcript_scroll, s2.view.transcript_scroll);
    assert_eq!(s1.view.anim_tick, s2.view.anim_tick);
    assert_eq!(s1.view.context_usage, s2.view.context_usage);
    assert_eq!(s1.view.transcript.len(), s2.view.transcript.len());
    assert_eq!(s1.pending_messages, s2.pending_messages);
}

#[test]
fn esc_priority_overlay_over_menu_over_cancel() {
    // overlay 打开：Esc 只关 overlay，不产生取消效果。
    let mut s = state();
    s.running = true;
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool("c1", "bash", ToolStatus::Failed, 1, Some(1), "err");
    reducer::update(&mut s, UiEvent::ClickTool("c1".into()));
    assert!(s.view.overlay.is_some());
    let effects = reducer::update(&mut s, key(KeyCode::Esc));
    assert!(s.view.overlay.is_none());
    assert!(effects.is_empty(), "overlay 优先：不得产生 CancelRun");

    // 菜单打开：Esc 关菜单。
    reducer::update(&mut s, UiEvent::Paste("/set".into()));
    assert!(s.view.menu.is_some());
    let effects = reducer::update(&mut s, key(KeyCode::Esc));
    assert!(s.view.menu.is_none());
    assert!(effects.is_empty());

    // 无 overlay/menu 且 run 中：Esc → CancelRun。
    let effects = reducer::update(&mut s, key(KeyCode::Esc));
    assert_eq!(effects, vec![UiEffect::CancelRun]);

    // 空闲（非 run）：Esc 无效果。
    s.running = false;
    let effects = reducer::update(&mut s, key(KeyCode::Esc));
    assert!(effects.is_empty());
}

#[test]
fn running_state_gates_cancel_and_tick_advances_spinner() {
    let mut s = state();
    s.running = false;
    assert!(reducer::update(&mut s, key(KeyCode::Esc)).is_empty());
    s.running = true;
    assert_eq!(
        reducer::update(&mut s, key(KeyCode::Esc)),
        vec![UiEffect::CancelRun]
    );
    let before = s.view.anim_tick;
    reducer::update(&mut s, UiEvent::Tick);
    assert_eq!(s.view.anim_tick, before + 1);
}

#[test]
fn menu_enter_submits_completed_command() {
    let mut s = state();
    for ch in "/hel".chars() {
        reducer::update(
            &mut s,
            UiEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
        );
    }
    assert!(s.view.menu.is_some());
    // Tab 选中下一项（循环到 /help）。
    reducer::update(&mut s, key(KeyCode::Tab));
    reducer::update(&mut s, key(KeyCode::Tab));
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.pending_messages.front().map(String::as_str),
        Some("/help")
    );
}

#[test]
fn agent_events_drive_view_state() {
    let mut s = state();
    s.view.status = StatusLine::Running {
        turn: 0,
        tool: "连接中".into(),
    };
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::TurnStarted { turn: 2 }),
    );
    assert_eq!(s.view.turn, 2);
    assert!(matches!(s.view.status, StatusLine::Running { turn: 2, .. }));
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::AssistantDelta {
            request_id: tpi::ids::RequestId::new_v7(),
            kind: DeltaKind::Text,
            text: "增量一".into(),
        }),
    );
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::AssistantDelta {
            request_id: tpi::ids::RequestId::new_v7(),
            kind: DeltaKind::Text,
            text: "增量二".into(),
        }),
    );
    let msg = s
        .view
        .live
        .assistant
        .as_ref()
        .expect("live 区必须有流式消息");
    assert_eq!(msg.text, "增量一增量二", "同一条消息的流式增量必须合并");

    let call_id = tpi::ids::ToolCallId::new_v7();
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::ToolStarted {
            call_id,
            name: "read".into(),
            target: "a.rs".into(),
            command: None,
        }),
    );
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::ToolOutputDelta {
            call_id,
            stream: 0,
            text: "输出内容".into(),
        }),
    );
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::ToolCompleted {
            call_id,
            name: "read".into(),
            status: ToolStatus::Succeeded,
            duration_ms: 5,
            exit_code: None,
            tail: "输出内容".into(),
        }),
    );
    // 工具卡片存在且含输出。
    let tool = s
        .view
        .transcript
        .iter()
        .find_map(|e| match e {
            Entry::Tool { card, .. } if card.id == call_id.to_string() => Some(card),
            _ => None,
        })
        .expect("工具卡片必须存在");
    assert_eq!(tool.output.as_deref(), Some("输出内容"));
    // BudgetWarning 是系统行。
    reducer::update(&mut s, UiEvent::Agent(RuntimeEvent::BudgetWarning));
    assert!(s.view.transcript.iter().any(|e| match e {
        Entry::Message { line, .. } => line.kind == LineKind::System,
        _ => false,
    }));
}

use tpi::tui::model::Entry;

#[test]
fn shift_enter_and_ctrl_j_insert_newline_but_plain_enter_submits() {
    let mut s = state();
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
    );
    // Shift+Enter 换行（§23）。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
    );
    assert_eq!(s.editor.text(), "a\nb");
    // Ctrl+J 换行兜底。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
    );
    assert_eq!(s.editor.text(), "a\nb\nc");
    // 普通 Enter 提交（§23：Enter = 提交）。
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.pending_messages.front().map(String::as_str),
        Some("a\nb\nc")
    );
}

#[test]
fn up_down_moves_within_multiline_then_falls_back_to_history() {
    let mut s = state();
    // 先提交一条历史。
    reducer::update(&mut s, UiEvent::Paste("历史命令".into()));
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.pending_messages.front().map(String::as_str),
        Some("历史命令")
    );
    s.pending_messages.clear();
    // 多行输入。
    reducer::update(&mut s, UiEvent::Paste("第一行\n第二行".into()));
    s.editor.home();
    s.sync_input();
    // §12：↑ 先多行移动（第二行 → 第一行），到边界后才进历史。
    reducer::update(&mut s, key(KeyCode::Up));
    assert_eq!(
        s.editor.text(),
        "第一行\n第二行",
        "第二行 ↑ 应移到第一行，不触发历史"
    );
    reducer::update(&mut s, key(KeyCode::Up));
    assert_eq!(
        s.editor.text(),
        "历史命令",
        "第一行再 ↑ 应进入 prompt history"
    );
    // ↓ 从历史向下：历史语义（到底后清空回空输入）。
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(s.editor.text(), "", "历史向下到底 = 空输入");
}

#[test]
fn ctrl_f_search_finds_and_jumps_without_moving_on_close() {
    let mut s = state();
    s.view.push_line(LineKind::User, "修复 stale_revision 问题");
    s.view
        .push_line(LineKind::Assistant, "好的，这是 stale_revision 的修复");
    s.view.push_line(LineKind::System, "无关内容");
    // Ctrl+F 打开搜索。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
    );
    assert!(s.view.search.is_some(), "Ctrl+F 打开搜索");
    // 输入关键词。
    for ch in "stale".chars() {
        reducer::update(
            &mut s,
            UiEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
        );
    }
    let search = s.view.search.as_ref().unwrap();
    assert_eq!(search.hits.len(), 2, "两条消息命中");
    // 搜索命中 → Locked 锚定（§14：命中后进入 Locked）。
    assert!(
        s.view.scroll_mode != ScrollMode::Follow,
        "命中后必须 Locked"
    );
    // Enter 下一个命中。
    let locked_before = s.view.scroll_mode;
    reducer::update(&mut s, key(KeyCode::Enter));
    let ScrollMode::Locked(after) = s.view.scroll_mode else {
        panic!();
    };
    let ScrollMode::Locked(before) = locked_before else {
        panic!();
    };
    assert_ne!(after.entry_id, before.entry_id, "Enter 跳到下一个命中");
    // Esc 关闭搜索：不强制跳回底部（保持 Locked）。
    reducer::update(&mut s, key(KeyCode::Esc));
    assert!(s.view.search.is_none());
    assert!(
        s.view.scroll_mode != ScrollMode::Follow,
        "关闭搜索不得跳回底部（§14）"
    );
}

#[test]
fn alt_up_down_jumps_between_user_turns() {
    let mut s = state();
    s.view.push_line(LineKind::User, "第一个问题");
    s.view.push_line(LineKind::Assistant, "回答一");
    s.view.push_line(LineKind::User, "第二个问题");
    s.view.push_line(LineKind::Assistant, "回答二");
    // Alt+Up：从尾部（layout_top None → 末尾 user）跳到上一个 User。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
    );
    let ScrollMode::Locked(anchor) = s.view.scroll_mode else {
        panic!("跳转后必须 Locked");
    };
    // 第二个 User 是第 3 个 entry（id 3）。
    assert_eq!(anchor.entry_id.0, 3, "Alt+Up 应跳到最近的 User entry");
    // 再 Alt+Up → 第一个 User。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
    );
    let ScrollMode::Locked(anchor) = s.view.scroll_mode else {
        panic!();
    };
    assert_eq!(anchor.entry_id.0, 1);
    // Alt+Down → 回到第二个 User。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
    );
    let ScrollMode::Locked(anchor) = s.view.scroll_mode else {
        panic!();
    };
    assert_eq!(anchor.entry_id.0, 3);
}

/// BUG-004：Ctrl-C 在运行中必须产生 CancelRun（Windows raw mode 下
/// tokio 的 ctrl_c() 信号不会触发，此前按键被 reducer 直接忽略）。
#[test]
fn ctrl_c_running_cancels_run() {
    let mut s = state();
    s.running = true;
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(
        effects.contains(&UiEffect::CancelRun),
        "运行中 Ctrl-C 必须取消 run: {effects:?}"
    );
}

/// BUG-004：空闲 Ctrl-C 必须产生 Quit（此前该按键被忽略，Windows 上无法退出）。
#[test]
fn ctrl_c_idle_quits() {
    let mut s = state();
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(
        effects.contains(&UiEffect::Quit),
        "空闲 Ctrl-C 必须退出: {effects:?}"
    );
}

/// BUG-004：Ctrl-C 与 Esc 同层——Overlay 打开时先关闭 Overlay，不产生 Quit/CancelRun。
#[test]
fn ctrl_c_priority_overlay_first() {
    let mut s = state();
    s.running = true;
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool("c1", "bash", ToolStatus::Failed, 1, Some(1), "err");
    reducer::update(&mut s, UiEvent::ClickTool("c1".into()));
    assert!(s.view.overlay.is_some());
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(s.view.overlay.is_none(), "Ctrl-C 应关闭 Overlay");
    assert!(
        effects.is_empty(),
        "关闭 Overlay 不得附带取消/退出: {effects:?}"
    );
}

/// BUG-005：运行中连续提交两条消息必须按顺序排队，不能覆盖丢失第一条。
#[test]
fn second_submit_while_running_queues_not_overwrites() {
    let mut s = state();
    // 模拟运行中提交第一条。
    for ch in "第一条".chars() {
        reducer::update(
            &mut s,
            UiEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
        );
    }
    reducer::update(&mut s, key(KeyCode::Enter));
    // 再提交第二条（不经过 run 边界）。
    for ch in "第二条".chars() {
        reducer::update(
            &mut s,
            UiEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
        );
    }
    reducer::update(&mut s, key(KeyCode::Enter));

    assert_eq!(s.pending_messages.len(), 2, "两条消息都必须排队");
    assert_eq!(s.pop_pending().as_deref(), Some("第一条"), "FIFO 顺序");
    assert_eq!(s.pop_pending().as_deref(), Some("第二条"));
}

/// BUG-005：has_pending_work 供 app 主循环判断是否可跳过键盘阻塞（BUG-003）。
#[test]
fn has_pending_work_reflects_queue_and_session() {
    let mut s = state();
    assert!(!s.has_pending_work());
    s.push_pending("hello".into());
    assert!(s.has_pending_work());
    assert_eq!(s.pop_pending().as_deref(), Some("hello"));
    assert!(!s.has_pending_work());
    s.pending_session = Some("id".into());
    assert!(s.has_pending_work());
}

/// BUG-013：Modal 打开时 ↑/↓ 滚动 Modal（提示与实际行为一致）。
#[test]
fn arrows_scroll_modal_when_open() {
    let mut s = state();
    s.view.open_modal("/help", "line1\nline2\nline3");
    assert_eq!(s.view.modal.as_ref().unwrap().scroll, 0);
    reducer::update(&mut s, key(KeyCode::Down));
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(s.view.modal.as_ref().unwrap().scroll, 2);
    reducer::update(&mut s, key(KeyCode::Up));
    assert_eq!(s.view.modal.as_ref().unwrap().scroll, 1);
    // 滚动到顶不 panic。
    for _ in 0..5 {
        reducer::update(&mut s, key(KeyCode::Up));
    }
    assert_eq!(s.view.modal.as_ref().unwrap().scroll, 0);
}

/// BUG-014：搜索打开时 Paste 进入搜索框而不是 composer。
#[test]
fn paste_goes_into_search_when_search_open() {
    let mut s = state();
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
    );
    assert!(s.view.search.is_some());
    reducer::update(&mut s, UiEvent::Paste("error[E0308]".into()));
    assert_eq!(
        s.view.search.as_ref().unwrap().query,
        "error[E0308]",
        "粘贴必须进入搜索框"
    );
    assert!(s.editor.text().is_empty(), "composer 不得被污染");
    // 搜索关闭后 Paste 回到 composer。
    reducer::update(&mut s, key(KeyCode::Esc));
    reducer::update(&mut s, UiEvent::Paste("正常输入".into()));
    assert_eq!(s.editor.text(), "正常输入");
}

/// BUG-005：footer 排队计数与队列同步（push 后可见、pop 后递减）。
#[test]
fn pending_queue_len_tracks_queue_for_footer() {
    let mut s = state();
    s.push_pending("A".into());
    s.push_pending("B".into());
    assert_eq!(s.view.pending_queue_len, 2, "footer 必须显示排队数");
    assert_eq!(s.pop_pending().as_deref(), Some("A"));
    assert_eq!(s.view.pending_queue_len, 1);
    assert_eq!(s.pop_pending().as_deref(), Some("B"));
    assert_eq!(s.view.pending_queue_len, 0);
}

/// §24：scrollbar 点击 → 按比例锁定到绝对位置（0 = 顶部，1 = 底部）。
#[test]
fn scrollbar_click_jumps_to_ratio() {
    let mut s = state();
    for i in 0..50 {
        s.view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    // 模拟布局后的视口高度（如 22 行）。
    s.view.transcript_rows = 22;
    // 点底部 → 锁定到内容末尾（Follow 语义等价：到底部）。
    reducer::update(&mut s, UiEvent::ScrollbarClick(21));
    let ScrollMode::Locked(anchor) = s.view.scroll_mode else {
        panic!("点击后必须 Locked");
    };
    // 底部锚点 = 最后一个 entry 的某行。
    assert!(
        anchor.entry_id.0 > 0,
        "点底部应锁定到历史末尾附近: {anchor:?}"
    );
    // 点顶部 → 锁定到第一个 entry。
    reducer::update(&mut s, UiEvent::ScrollbarClick(0));
    let ScrollMode::Locked(anchor) = s.view.scroll_mode else {
        panic!();
    };
    assert_eq!(
        anchor.entry_id.0, 1,
        "点顶部应锁定到第一条 entry（首条 id=1）"
    );
}

/// §25：Ctrl+Home 跳到历史最顶部。
#[test]
fn ctrl_home_jumps_to_top() {
    let mut s = state();
    for i in 0..10 {
        s.view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    s.view.follow_tail();
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)),
    );
    let ScrollMode::Locked(anchor) = s.view.scroll_mode else {
        panic!("Ctrl+Home 后必须 Locked");
    };
    assert_eq!(
        anchor.entry_id.0, 1,
        "Ctrl+Home 应跳到第一条 entry（首条 id=1）"
    );
}

/// 修复：回车发送后输入框不得残留文本（submit 清空 editor 后必须同步 view.input）。
#[test]
fn submit_clears_input_projection() {
    let mut s = state();
    for ch in "你好 world".chars() {
        reducer::update(
            &mut s,
            UiEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
        );
    }
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.pending_messages.front().map(String::as_str),
        Some("你好 world")
    );
    assert!(s.editor.text().is_empty(), "editor 必须清空");
    assert!(
        s.view.input.is_empty(),
        "view.input 必须同步清空（发送后输入框不得残留文本）"
    );
}

/// Ctrl-C 在搜索打开时先关闭搜索，不误退出/误取消；再按一次才退出。
#[test]
fn ctrl_c_closes_search_instead_of_quitting_or_cancelling() {
    let mut s = state();
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
    );
    assert!(s.view.search.is_some());
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(s.view.search.is_none(), "Ctrl-C 必须先关闭搜索");
    assert!(
        effects.is_empty(),
        "关闭搜索不得产生 Quit/CancelRun: {effects:?}"
    );
    // 搜索已关、空闲：再按 Ctrl-C → 退出。
    let effects2 = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(effects2.contains(&UiEffect::Quit));
}
