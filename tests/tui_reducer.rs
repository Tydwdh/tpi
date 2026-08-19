//! T3：UiState + Reducer `单向流测试（TPI_TUI_V2_TASK` §26-27、§56）。
//!
//! 断言：状态可重放（相同事件序列 → 相同状态）、reducer 不产生副作用
//! （跨边界动作只以 effect 返回）、键盘路由优先级（§11）。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tpi::agent::{DeltaKind, RuntimeEvent};
use tpi::outcome::ToolStatus;
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
    assert_eq!(s1.view.anim_tick, s2.view.anim_tick);
    assert_eq!(s1.view.context_usage, s2.view.context_usage);
    assert_eq!(s1.view.transcript.len(), s2.view.transcript.len());
    assert_eq!(s1.pending_messages, s2.pending_messages);
}

/// §用户诉求：手动 /compact 结果（CompactionNotice）写入系统行，用户可见。
#[test]
fn compaction_notice_pushes_system_line() {
    let mut s = state();
    let before = s.view.transcript.len();
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::CompactionNotice {
            message: "手动压缩未生效：没有可压缩的历史".into(),
        }),
    );
    assert_eq!(s.view.transcript.len(), before + 1, "必须写入一行系统消息");
    let last = s.view.transcript.last().unwrap();
    let tpi::tui::model::Entry::Message { line, .. } = last else {
        panic!("CompactionNotice 必须产生 Message 条目");
    };
    assert_eq!(line.kind, LineKind::System);
    assert!(
        line.text.contains("没有可压缩的历史"),
        "系统行必须包含通知文本: {:?}",
        line.text
    );
}

#[test]
fn esc_priority_overlay_over_menu_over_cancel() {
    // overlay 打开：Esc 只关 overlay，不产生取消效果。
    let mut s = state();
    s.running = true;
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool(("c1", "bash"), ToolStatus::Failed, 1, Some(1), "err", None);
    s.view.open_tool_overlay("c1");
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
        step: 0,
        tool: "连接中".into(),
    };
    reducer::update(
        &mut s,
        UiEvent::Agent(RuntimeEvent::StepStarted { step: 2 }),
    );
    assert_eq!(s.view.step, 2);
    assert!(matches!(s.view.status, StatusLine::Running { step: 2, .. }));
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
            diff: None,
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
    // ↓ 从历史向下：到最新槽位时恢复进入历史前的草稿（不丢输入）。
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(
        s.editor.text(),
        "第一行\n第二行",
        "历史向下回最新 = 恢复草稿"
    );
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

/// §用户诉求：Ctrl+C 只做复制——运行中无选区静默忽略（取消统一用 Esc）。
#[test]
fn ctrl_c_running_without_selection_is_ignored() {
    let mut s = state();
    s.running = true;
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(
        effects.is_empty(),
        "运行中无选区 Ctrl-C 应静默忽略（不取消、不复制）: {effects:?}"
    );
    assert!(!effects.contains(&UiEffect::CancelRun), "Ctrl-C 不取消 run");
    assert!(s.view.input.is_empty(), "Ctrl-C 不得写入输入框");
}

/// §用户诉求：退出用 Ctrl+D（与复制分离，避免 Ctrl+C 误触退出）。
#[test]
fn ctrl_d_quits() {
    let mut s = state();
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
    );
    assert!(
        effects.contains(&UiEffect::Quit),
        "Ctrl-D 必须退出: {effects:?}"
    );
    // Ctrl+C 空闲不再退出。
    let effects2 = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(
        !effects2.contains(&UiEffect::Quit),
        "Ctrl-C 不得退出（退出用 Ctrl-D）: {effects2:?}"
    );
    assert!(s.view.input.is_empty(), "Ctrl-D 不得写入输入框");
}

/// §用户诉求：Ctrl-C 静默忽略——不关闭 Overlay（Esc 负责关闭弹层）。
#[test]
fn ctrl_c_does_not_close_overlay() {
    let mut s = state();
    s.running = true;
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool(("c1", "bash"), ToolStatus::Failed, 1, Some(1), "err", None);
    s.view.open_tool_overlay("c1");
    assert!(s.view.overlay.is_some());
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(
        s.view.overlay.is_some(),
        "Ctrl-C 不关闭 Overlay（Esc 负责）"
    );
    assert!(effects.is_empty(), "Ctrl-C 静默忽略: {effects:?}");
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

/// §用户诉求：Ctrl-C 只用于复制——搜索打开时也静默忽略（Esc 负责关闭搜索）。
#[test]
fn ctrl_c_ignored_while_search_open() {
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
    // §用户诉求：Ctrl-C 只复制——不关闭搜索（Esc 负责关闭），静默忽略。
    assert!(s.view.search.is_some(), "Ctrl-C 不关闭搜索（Esc 负责）");
    assert!(effects.is_empty(), "Ctrl-C 静默忽略: {effects:?}");
    // 搜索已开、空闲：Ctrl-C 也不退出。
    let effects2 = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(!effects2.contains(&UiEffect::Quit), "Ctrl-C 不得退出");
}

/// 弹层打开时普通按键不得写入 composer（关弹层后输入框不应出现乱码）。
#[test]
fn modal_blocks_composer_typing() {
    let mut s = state();
    s.view.open_modal("/help", "内容");
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(
        s.editor.text().is_empty(),
        "弹层打开时输入不得进入 composer"
    );
    assert!(s.view.input.is_empty());
    assert!(s.pending_messages.is_empty(), "Enter 不得在弹层打开时提交");

    // 导航键仍然有效：Down 滚动 Modal。
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(s.view.modal.as_ref().unwrap().scroll, 1);
    // Esc 关闭。
    reducer::update(&mut s, key(KeyCode::Esc));
    assert!(s.view.modal.is_none());
    // 关闭后可正常输入。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
    );
    assert_eq!(s.editor.text(), "b");
}

/// Overlay 打开时同样屏蔽 composer 输入；鼠标/点击由 app 层处理。
#[test]
fn overlay_blocks_composer_typing() {
    let mut s = state();
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool(("c1", "bash"), ToolStatus::Failed, 1, Some(1), "err", None);
    s.view.open_tool_overlay("c1");
    assert!(s.view.overlay.is_some());
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
    );
    assert!(
        s.editor.text().is_empty(),
        "Overlay 打开时输入不得进入 composer"
    );
}

#[test]
fn paste_blocked_while_modal_or_overlay_open() {
    // Paste while Modal is open must not write into the composer (keys are already blocked).
    let mut s = state();
    s.view.open_modal("/help", "content");
    reducer::update(&mut s, UiEvent::Paste("junk".into()));
    assert!(
        s.editor.text().is_empty(),
        "Paste must not reach composer while Modal is open"
    );
    assert!(s.view.input.is_empty());

    // Overlay blocks Paste the same way.
    let mut s2 = state();
    s2.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s2.view
        .finish_tool(("c1", "bash"), ToolStatus::Failed, 1, Some(1), "err", None);
    s2.view.open_tool_overlay("c1");
    assert!(s2.view.overlay.is_some());
    reducer::update(&mut s2, UiEvent::Paste("junk".into()));
    assert!(
        s2.editor.text().is_empty(),
        "Paste must not reach composer while Overlay is open"
    );
    reducer::update(&mut s2, key(KeyCode::Esc));
    assert!(s2.view.overlay.is_none());
    reducer::update(&mut s2, UiEvent::Paste("ok".into()));
    assert_eq!(
        s2.editor.text(),
        "ok",
        "Paste returns to composer after closing the overlay"
    );
}

/// While Modal is open, PgUp/PgDn and wheel must scroll the Modal, not the transcript behind.
#[test]
fn page_and_wheel_scroll_modal_when_open() {
    let mut s = state();
    s.view.open_modal("/help", "line1\nline2\nline3\nline4");
    let scroll_before = s.view.scroll_mode;
    reducer::update(&mut s, key(KeyCode::PageDown));
    assert_eq!(
        s.view.modal.as_ref().unwrap().scroll,
        10,
        "PgDn must scroll the Modal"
    );
    reducer::update(&mut s, UiEvent::MouseScrollDown);
    assert_eq!(
        s.view.modal.as_ref().unwrap().scroll,
        15,
        "wheel must scroll the Modal (step 5)"
    );
    reducer::update(&mut s, UiEvent::MouseScrollUp);
    assert_eq!(s.view.modal.as_ref().unwrap().scroll, 10);
    reducer::update(&mut s, key(KeyCode::PageUp));
    assert_eq!(s.view.modal.as_ref().unwrap().scroll, 0);
    assert_eq!(
        s.view.scroll_mode, scroll_before,
        "scrolling while Modal is open must not move the transcript behind"
    );
}

#[test]
fn clicks_blocked_while_modal_open() {
    let mut s = state();
    for i in 0..50 {
        s.view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    s.view.scroll_up(20); // enter Locked so a scrollbar click would otherwise move the viewport
    let locked_before = s.view.scroll_mode;
    s.view.open_modal("/help", "content");
    reducer::update(&mut s, UiEvent::ScrollbarClick(5));
    assert_eq!(
        s.view.scroll_mode, locked_before,
        "scrollbar click must not scroll transcript while Modal is open"
    );
    // ClickTool must not expand a card behind the modal.
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool(("c1", "bash"), ToolStatus::Failed, 1, Some(1), "err", None);
    reducer::update(&mut s, UiEvent::ClickTool("c1".into()));
    assert!(
        !matches!(
            &s.view.transcript[0],
            tpi::tui::model::Entry::Tool { card, .. } if card.expanded
        ),
        "tool click must not expand a card behind Modal"
    );
}

#[test]
fn clicks_blocked_while_overlay_open() {
    let mut s = state();
    for i in 0..50 {
        s.view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool(("c1", "bash"), ToolStatus::Failed, 1, Some(1), "err", None);
    s.view.open_tool_overlay("c1");
    assert!(s.view.overlay.is_some());
    let scroll_before = s.view.scroll_mode;
    reducer::update(&mut s, UiEvent::ScrollbarClick(5));
    assert_eq!(
        s.view.scroll_mode, scroll_before,
        "scrollbar click must not scroll transcript while Overlay is open"
    );
}

#[test]
fn home_end_sync_input_cursor_projection() {
    let mut s = state();
    reducer::update(&mut s, UiEvent::Paste("abc".into()));
    assert_eq!(s.view.input_cursor, s.editor.cursor);
    reducer::update(&mut s, key(KeyCode::Home));
    assert_eq!(
        s.view.input_cursor, 0,
        "Home must sync the cursor projection"
    );
    reducer::update(&mut s, key(KeyCode::End));
    assert_eq!(
        s.view.input_cursor, 3,
        "End must sync the cursor projection"
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
    );
    assert_eq!(
        s.view.input_cursor, 0,
        "Ctrl+A must sync the cursor projection"
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
    );
    assert_eq!(
        s.view.input_cursor, 3,
        "Ctrl+E must sync the cursor projection"
    );
}

#[test]
fn search_mode_keeps_transcript_navigation_keys() {
    let mut s = state();
    for i in 0..60 {
        s.view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    // Ctrl+F opens search.
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
    );
    assert!(s.view.search.is_some(), "Ctrl+F opens search");
    // PgUp must still scroll the transcript (consistent with the mouse wheel).
    reducer::update(&mut s, key(KeyCode::PageUp));
    assert!(
        s.view.scroll_mode != ScrollMode::Follow,
        "PgUp while search is open must scroll the transcript"
    );
    // Ctrl+End returns to Follow while keeping search open.
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
    );
    assert_eq!(
        s.view.scroll_mode,
        ScrollMode::Follow,
        "Ctrl+End restores Follow"
    );
    assert!(s.view.search.is_some(), "Ctrl+End must not close search");
    // PgDown at Follow is a no-op (already at the bottom).
    reducer::update(&mut s, key(KeyCode::PageDown));
    assert_eq!(s.view.scroll_mode, ScrollMode::Follow);
}

#[test]
fn search_ctrl_u_clears_query() {
    let mut s = state();
    s.view.push_line(LineKind::Assistant, "hello world");
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
    );
    assert_eq!(s.view.search.as_ref().unwrap().query, "hi");
    // Ctrl+U must clear the search query (consistent with the composer), not type "u".
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
    );
    assert!(
        s.view.search.as_ref().unwrap().query.is_empty(),
        "Ctrl+U must clear the search query"
    );
}

#[test]
fn slash_enter_defaults_to_help_not_quit() {
    let mut s = state();
    reducer::update(&mut s, UiEvent::Paste("/".into()));
    assert!(s.view.menu.is_some(), "typing / opens the command menu");
    assert_eq!(s.view.menu.as_ref().unwrap().selected, 0);
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.pending_messages.front().map(String::as_str),
        Some("/help"),
        "typing / + Enter must default to /help, never /quit"
    );
}

#[test]
fn esc_idle_clears_input() {
    let mut s = state();
    reducer::update(&mut s, UiEvent::Paste("abc".into()));
    reducer::update(&mut s, key(KeyCode::Esc));
    assert!(s.editor.text().is_empty(), "idle Esc must clear the input");
    assert!(s.view.input.is_empty());
    // Empty input: Esc is a no-op with no effects.
    let effects = reducer::update(&mut s, key(KeyCode::Esc));
    assert!(
        effects.is_empty(),
        "idle Esc on empty input must not produce effects"
    );
}

#[test]
fn alt_up_down_shows_hint_when_no_user_message() {
    let mut s = state();
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
    );
    assert_eq!(
        s.view.transient_hint.as_deref(),
        Some("没有用户消息可跳转"),
        "Alt+Up with no User message must show a hint"
    );
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
    );
    assert_eq!(
        s.view.transient_hint.as_deref(),
        Some("没有用户消息可跳转"),
        "Alt+Down with no User message must show a hint"
    );
    // Next key press clears the transient hint.
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
    );
    assert!(
        s.view.transient_hint.is_none(),
        "next key must clear the hint"
    );
}

/// /sessions 是“菜单 + Modal 指令”组合：Enter 仍应恢复选中 session（例外路径）。
#[test]
fn sessions_menu_enter_still_works_with_modal_open() {
    let mut s = state();
    s.view.open_modal("/sessions", "会话列表");
    s.view.menu = Some(tpi::tui::model::MenuView {
        items: vec![("sess-1".into(), "label".into())],
        selected: 0,
        kind: tpi::tui::model::MenuKind::Session,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(s.pending_session.as_deref(), Some("sess-1"));
    assert!(
        s.view.menu.is_none() && s.view.modal.is_none(),
        "selecting a session must close both the menu and the /sessions modal"
    );
}

/// /sessions 浏览器（Modal + Session 菜单）一次 Esc 全部关闭。
#[test]
fn esc_closes_sessions_browser_in_one_press() {
    let mut s = state();
    s.view.open_modal("/sessions", "list");
    s.view.menu = Some(tpi::tui::model::MenuView {
        items: vec![("sess-1".into(), "label".into())],
        selected: 0,
        kind: tpi::tui::model::MenuKind::Session,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    reducer::update(&mut s, key(KeyCode::Esc));
    assert!(
        s.view.modal.is_none() && s.view.menu.is_none(),
        "one Esc must dismiss the whole /sessions browser"
    );
}

/// /theme 是“菜单 + Modal 指令”组合：Enter 仍应应用选中主题（例外路径），
/// 与 /sessions 一致；选中后一并关闭菜单与 Modal。
#[test]
fn theme_menu_enter_sets_pending_theme() {
    let mut s = state();
    s.view.open_modal("/theme", "当前主题: omp");
    s.view.menu = Some(tpi::tui::model::MenuView {
        items: vec![
            ("omp".into(), "默认 · base16-mocha".into()),
            ("onedarkpro".into(), "One Dark Pro · Solarized".into()),
        ],
        selected: 1,
        kind: tpi::tui::model::MenuKind::Theme,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(s.pending_theme.as_deref(), Some("onedarkpro"));
    assert!(
        s.view.menu.is_none() && s.view.modal.is_none(),
        "selecting a theme must close both the menu and the /theme modal"
    );
}

/// /theme 浏览器（Modal + Theme 菜单）一次 Esc 全部关闭。
#[test]
fn esc_closes_theme_browser_in_one_press() {
    let mut s = state();
    s.view.open_modal("/theme", "选择主题");
    s.view.menu = Some(tpi::tui::model::MenuView {
        items: vec![("omp".into(), "默认".into())],
        selected: 0,
        kind: tpi::tui::model::MenuKind::Theme,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    reducer::update(&mut s, key(KeyCode::Esc));
    assert!(
        s.view.modal.is_none() && s.view.menu.is_none(),
        "one Esc must dismiss the whole /theme browser"
    );
}

/// /theme 菜单在 ↑/↓ 导航后必须保持（同 /sessions `回归：refresh_menus`
/// 不得把 Modal+菜单组合清掉）。
#[test]
fn theme_menu_survives_up_down_navigation() {
    let mut s = state();
    s.view.open_modal("/theme", "选择主题");
    s.view.menu = Some(tpi::tui::model::MenuView {
        items: vec![
            ("omp".into(), "默认".into()),
            ("dark".into(), "深色".into()),
            ("light".into(), "浅色".into()),
        ],
        selected: 0,
        kind: tpi::tui::model::MenuKind::Theme,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(s.view.menu.as_ref().map(|m| m.selected), Some(1));
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(s.view.menu.as_ref().map(|m| m.selected), Some(2));
    reducer::update(&mut s, key(KeyCode::Up));
    assert_eq!(s.view.menu.as_ref().map(|m| m.selected), Some(1));
    assert!(
        s.view.menu.is_some(),
        "theme menu must survive navigation (regression)"
    );
}

/// §用户诉求（回归）：/sessions 菜单在 ↑/↓ 导航后必须保持——reducer 的
/// MoveUp/MoveDown 会调用 `refresh_menus，此前会把` Session 菜单清成 None，
/// 导致“在会话列表上下移动后列表突然消失”。
#[test]
fn sessions_menu_survives_up_down_navigation() {
    let mut s = state();
    s.view.open_modal("/sessions", "会话列表");
    s.view.menu = Some(tpi::tui::model::MenuView {
        items: (0..5)
            .map(|i| (format!("sess-{i}"), format!("会话 {i} · 时间 · 事件")))
            .collect(),
        selected: 0,
        kind: tpi::tui::model::MenuKind::Session,
        session_previews: (0..5)
            .map(|i| {
                vec![tpi::tui::model::MenuPreviewLine {
                    is_user: i % 2 == 0,
                    text: format!("预览 {i}"),
                }]
            })
            .collect(),
        filter: String::new(),
    });
    // ↓ 两次：选中 0 → 1 → 2；菜单必须始终存在，selected 跟随。
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(s.view.menu.as_ref().map(|m| m.selected), Some(1));
    assert!(
        s.view.menu.is_some(),
        "↓ 后 Session 菜单不得被命令菜单刷新清掉"
    );
    // §用户诉求：↓ 后悬浮窗（Modal）预览同步为选中会话（选中 1 → `预览 1`）。
    assert_eq!(
        s.view.modal.as_ref().map(|m| m.body.as_str()),
        Some("AI 预览 1"),
        "↓ 后悬浮窗必须显示选中会话的预览"
    );
    reducer::update(&mut s, key(KeyCode::Down));
    assert_eq!(s.view.menu.as_ref().map(|m| m.selected), Some(2));
    assert!(s.view.menu.is_some());
    assert_eq!(
        s.view.modal.as_ref().map(|m| m.body.as_str()),
        Some("你 预览 2"),
        "再 ↓ 后悬浮窗预览跟随"
    );
    // ↑ 回到 1，菜单仍在。
    reducer::update(&mut s, key(KeyCode::Up));
    assert_eq!(s.view.menu.as_ref().map(|m| m.selected), Some(1));
    assert!(s.view.menu.is_some(), "↑ 后 Session 菜单仍须保持");
    // 菜单种类仍是 Session（没有被替换成 SlashCommand）。
    assert_eq!(
        s.view.menu.as_ref().map(|m| m.kind),
        Some(tpi::tui::model::MenuKind::Session)
    );
}

/// §用户诉求：鼠标拖动选择后 Ctrl+C 触发 `CopySelection` effect（复制到剪贴板）。
#[test]
fn ctrl_c_with_selection_triggers_copy_effect() {
    use tpi::tui::interaction::TextPosition;
    use tpi::tui::scroll::EntryId;
    let mut s = state();
    // 模拟语义选区：entry 1 的 offset 0..5。
    s.view.selection_start(TextPosition {
        entry_id: EntryId(1),
        offset: 0,
    });
    s.view.selection_update(TextPosition {
        entry_id: EntryId(1),
        offset: 5,
    });
    s.view.selection_end();
    assert!(s.view.selection.is_some(), "选区必须存在");

    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(
        effects.contains(&UiEffect::CopySelection),
        "有选区时 Ctrl+C 必须触发复制: {effects:?}"
    );

    // 无选区时 Ctrl+C 静默（不复制）。
    let mut s2 = state();
    let effects2 = reducer::update(
        &mut s2,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(
        !effects2.contains(&UiEffect::CopySelection),
        "无选区时 Ctrl+C 不复制: {effects2:?}"
    );
}

/// §InteractionRefactor：SelectionStart/Update/End 事件把语义选区写入 view
///（reducer 直接处理，不再依赖 Renderer 坐标）。
#[test]
fn selection_events_update_view_selection() {
    use tpi::tui::interaction::TextPosition;
    use tpi::tui::scroll::EntryId;
    let mut s = state();
    assert!(s.view.selection.is_none());
    let p2 = TextPosition {
        entry_id: EntryId(1),
        offset: 2,
    };
    let p5 = TextPosition {
        entry_id: EntryId(1),
        offset: 5,
    };
    let effects = reducer::update(&mut s, UiEvent::SelectionStart(p2));
    assert!(effects.is_empty());
    assert_eq!(
        s.view.selection.map(|sel| sel.anchor),
        Some(p2),
        "SelectionStart 必须写入 anchor"
    );
    let effects = reducer::update(&mut s, UiEvent::SelectionUpdate(p5));
    assert!(effects.is_empty());
    assert_eq!(
        s.view.selection.map(|sel| sel.focus),
        Some(p5),
        "SelectionUpdate 必须更新 focus"
    );
    let effects = reducer::update(&mut s, UiEvent::SelectionEnd);
    assert!(effects.is_empty());
    assert!(s.view.selection.is_some(), "SelectionEnd 保留选区");
}

/// §成熟化：链接点击 → Link Overlay 打开；Enter 确认 → `OpenUrl` effect 并关闭。
#[test]
fn link_click_opens_overlay_and_enter_opens_url() {
    let mut s = state();
    let url = "https://example.com".to_string();
    let effects = reducer::update(&mut s, UiEvent::ClickLink(url.clone()));
    assert!(effects.is_empty());
    let overlay = s.view.overlay.as_ref().expect("点击链接必须打开 Overlay");
    assert_eq!(overlay.body, url, "Overlay 正文是 URL");
    assert_eq!(overlay.kind, tpi::tui::model::OverlayKind::Link);
    // Enter 确认打开：effect 返回 OpenUrl，Overlay 关闭。
    let effects = reducer::update(&mut s, key(KeyCode::Enter));
    assert!(
        effects.contains(&UiEffect::OpenUrl(url.clone())),
        "Link Overlay Enter 必须产生 OpenUrl: {effects:?}"
    );
    assert!(s.view.overlay.is_none(), "确认后 Overlay 关闭");
}

/// §成熟化：Link Overlay 内 `c` → `CopyText` effect 并关闭。
#[test]
fn link_overlay_c_copies_url() {
    let mut s = state();
    let url = "https://example.com".to_string();
    let _ = reducer::update(&mut s, UiEvent::ClickLink(url.clone()));
    let effects = reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
    );
    assert!(
        effects.contains(&UiEffect::CopyText(url)),
        "Link Overlay c 必须产生 CopyText: {effects:?}"
    );
    assert!(s.view.overlay.is_none());
}

/// §成熟化：非链接 Overlay 打开时普通按键不得写入 composer（回归）。
#[test]
fn non_link_overlay_blocks_composer_keys() {
    let mut s = state();
    s.view.overlay = Some(tpi::tui::model::OverlayState::for_reasoning("思考内容"));
    // 普通字符不进入输入框（blocking 分支丢弃）。
    let effects = reducer::update(&mut s, key(KeyCode::Char('x')));
    assert!(effects.is_empty());
    assert!(s.editor.text().is_empty(), "弹层打开时普通按键不得进输入框");
}

/// §成熟化：搜索命中使用惰性小写缓存——条目构造后缓存就绪，命中正确。
#[test]
fn search_uses_entry_lowercase_cache() {
    use tpi::tui::model::Entry;
    let mut s = state();
    s.view.push_line(LineKind::Assistant, "Hello World");
    s.view.push_line(LineKind::Assistant, "nothing here");
    // 缓存惰性填充（message 进入后不可变，只算一次）。
    let e1 = s.view.transcript[0].search_lower();
    assert_eq!(e1, "hello world");
    // 大小写不敏感命中。
    s.view.open_search();
    s.view.update_search_query("WORLD");
    assert_eq!(
        s.view.search.as_ref().unwrap().hits.len(),
        1,
        "必须命中大小写不敏感的缓存扫描"
    );
    assert_eq!(
        s.view.search.as_ref().unwrap().hits[0],
        s.view.transcript[0].id(),
        "命中的是 entry 1"
    );
    // 工具卡片同样可命中（name/target/tail 拼接）。
    s.view
        .begin_tool("c1", "read", Some("src/main.rs".into()), None);
    s.view
        .finish_tool(("c1", "read"), ToolStatus::Succeeded, 1, Some(0), "", None);
    let tool_entry = &mut s.view.transcript[2];
    assert!(matches!(tool_entry, Entry::Tool { .. }));
    assert_eq!(tool_entry.search_lower(), "read src/main.rs");
    s.view.update_search_query("main.rs");
    assert_eq!(
        s.view.search.as_ref().unwrap().hits.len(),
        1,
        "工具卡片 target 命中"
    );
}

/// §成熟化：语义文本缓存——同一 entry 重复复制不重新渲染 markdown。
#[test]
fn semantic_text_cache_reused_across_copies() {
    use tpi::tui::interaction::TextPosition;
    let mut s = state();
    s.view.push_line(LineKind::Assistant, "**加粗** 和 `code`");
    // 选区覆盖整条消息 → selected_text 走缓存路径。
    let id = s.view.transcript[0].id();
    s.view.selection_start(TextPosition {
        entry_id: id,
        offset: 0,
    });
    let len = match &s.view.transcript[0] {
        tpi::tui::model::Entry::Message { line, .. } => line.text.chars().count(),
        _ => 0,
    };
    s.view.selection_update(TextPosition {
        entry_id: id,
        offset: len,
    });
    s.view.selection_end();
    let text1 = s.view.selected_text();
    assert!(
        text1.contains("加粗") && text1.contains("code"),
        "markdown 去样式后的语义文本: {text1:?}"
    );
    let text2 = s.view.selected_text();
    assert_eq!(text1, text2, "重复提取结果一致");
}

/// §用户诉求：已移除 hover 悬浮高亮——鼠标移动不再更新任何状态；
/// 点击卡片仍正常展开。
#[test]
fn click_tool_expands_without_hover_state() {
    let mut s = state();
    // 点击卡片 → 展开（不依赖任何 hover 状态）。
    s.view.begin_tool("c1", "bash", Some("cmd".into()), None);
    s.view
        .finish_tool(("c1", "bash"), ToolStatus::Failed, 1, Some(1), "err", None);
    let effects = reducer::update(&mut s, UiEvent::ClickTool("c1".into()));
    assert!(effects.is_empty());
    let expanded = match &s.view.transcript[0] {
        tpi::tui::model::Entry::Tool { card, .. } => card.expanded,
        _ => false,
    };
    assert!(expanded, "点击卡片必须展开");
}

/// §用户诉求：大粘贴（≥300 字符或 >5 行）不真实渲染——输入框只放占位符，
/// 真实内容存入 `UiState.pasted`，提交时一次性展开成全文发送。
#[test]
fn large_paste_becomes_placeholder_and_expands_on_submit() {
    let mut s = state();
    let big = format!("第一行内容\n{}", "x".repeat(400));
    reducer::update(&mut s, UiEvent::Paste(big.clone()));
    // 输入框渲染的是占位符，不是全文。
    let text = s.editor.text().to_string();
    assert!(
        text.starts_with("[Pasted Content ") && text.ends_with(" chars]"),
        "大粘贴必须是占位符，实际: {text:?}"
    );
    assert_eq!(s.pasted.len(), 1, "真实内容存入旁路");
    // 提交 → pending 是展开后的全文。
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(
        s.pop_pending().as_deref(),
        Some(big.as_str()),
        "提交时占位符必须展开为完整原文"
    );
    assert!(s.editor.text().is_empty(), "提交后编辑区清空");
    assert!(s.pasted.is_empty(), "提交后必须释放旁路粘贴内容");
}

#[test]
fn clearing_composer_releases_large_paste_storage() {
    let mut s = state();
    reducer::update(&mut s, UiEvent::Paste("x".repeat(400)));
    assert_eq!(s.pasted.len(), 1);
    reducer::update(&mut s, key(KeyCode::Esc));
    assert!(s.editor.text().is_empty());
    assert!(s.pasted.is_empty());
}

/// §用户诉求：小粘贴（低于阈值）保持直接插入输入框，不产生占位符。
#[test]
fn small_paste_inserts_verbatim() {
    let mut s = state();
    reducer::update(&mut s, UiEvent::Paste("short text".into()));
    assert_eq!(s.editor.text(), "short text");
    assert!(s.pasted.is_empty(), "小粘贴不进入占位符存储");
}

#[test]
fn windows_paste_newlines_are_normalized() {
    let mut s = state();
    reducer::update(&mut s, UiEvent::Paste("第一行\r\n第二行\r第三行".into()));
    assert_eq!(s.editor.text(), "第一行\n第二行\n第三行");
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(s.pop_pending().as_deref(), Some("第一行\n第二行\n第三行"));
}

/// §用户诉求：大粘贴占位符与普通输入混排时，提交展开不吞普通文本。
#[test]
fn placeholder_expands_among_typed_text() {
    let mut s = state();
    reducer::update(&mut s, UiEvent::Paste("a\nb\nc\nd\ne\nf".into()));
    reducer::update(&mut s, key(KeyCode::Char(' ')));
    reducer::update(&mut s, key(KeyCode::Char('尾')));
    reducer::update(&mut s, key(KeyCode::Enter));
    let pending = s.pop_pending().unwrap();
    assert!(
        pending.ends_with(" 尾"),
        "占位符展开后保留后续输入，实际: {pending:?}"
    );
    assert!(
        pending.starts_with("a\nb\nc\nd\ne\nf"),
        "占位符展开为原文开头"
    );
}

#[test]
fn same_length_large_pastes_get_readable_unique_placeholders() {
    let mut s = state();
    let first = "a".repeat(300);
    let second = "b".repeat(300);
    reducer::update(&mut s, UiEvent::Paste(first.clone()));
    reducer::update(&mut s, UiEvent::Paste(second.clone()));

    assert_eq!(
        s.editor.text(),
        "[Pasted Content 300 chars][Pasted Content 300 chars] #2"
    );
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(s.pop_pending(), Some(format!("{first}{second}")));
}

#[test]
fn unbracketed_multiline_key_stream_is_collapsed_and_completed() {
    let mut s = state();
    let initial = "a\nb\nc\nd\ne\n";
    let full = "a\nb\nc\nd\ne\nf-tail";
    s.editor.insert_str(initial);
    s.sync_input();

    reducer::update(
        &mut s,
        UiEvent::CollapseKeyStreamPaste {
            rendered_suffix: initial.into(),
            text: initial.into(),
        },
    );
    assert_eq!(s.editor.text(), "[Pasted Content 10 chars]");

    reducer::update(
        &mut s,
        UiEvent::FinishKeyStreamPaste {
            initial_text: initial.into(),
            full_text: full.into(),
        },
    );
    assert_eq!(s.editor.text(), "[Pasted Content 16 chars]");
    reducer::update(&mut s, key(KeyCode::Enter));
    assert_eq!(s.pop_pending().as_deref(), Some(full));
}

/// §用户诉求：右侧边栏——Ctrl+B 切换开关（default 关闭，启动时 app 打开）；
/// 关闭时滚动无效果。
#[test]
fn toggle_sidebar_via_key_and_scroll_guarded_by_open() {
    let mut s = state();
    assert!(!s.view.sidebar.open, "default 关闭");
    // Ctrl+B 切换打开。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
    );
    assert!(s.view.sidebar.open, "Ctrl+B 必须打开侧边栏");
    // 再按一次关闭。
    reducer::update(
        &mut s,
        UiEvent::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
    );
    assert!(!s.view.sidebar.open, "Ctrl+B 再按关闭");
    // 关闭时滚动事件无效果。
    s.view.sidebar.scroll = 3;
    reducer::update(&mut s, UiEvent::SidebarScroll(false));
    assert_eq!(s.view.sidebar.scroll, 3, "关闭时滚动无效");
}

/// §用户诉求：侧边栏滚动——滚动条点击按比例跳转、滚轮增量滚动。
#[test]
fn sidebar_scroll_and_ratio_jump() {
    let mut s = state();
    s.view.sidebar.open = true;
    s.view.sidebar.total_rows = 20;
    s.view.sidebar.area_height = 5;
    // 滚轮向下：增量 +5，clamp 到 19。
    reducer::update(&mut s, UiEvent::SidebarScroll(false));
    assert_eq!(s.view.sidebar.scroll, 5);
    // 滚动条点击：点击第 1 行（0-based）→ 按比例跳到 20*1/5 = 4。
    reducer::update(&mut s, UiEvent::SidebarScrollbarClick(1));
    assert_eq!(s.view.sidebar.scroll, 4);
    // 滚轮向上：-5，clamp 到 0。
    reducer::update(&mut s, UiEvent::SidebarScroll(true));
    assert_eq!(s.view.sidebar.scroll, 0);
}

/// §用户诉求：大纲行点击跳转到该用户消息，侧边栏保持打开（连续浏览）。
#[test]
fn sidebar_jump_locks_to_user_entry_and_keeps_open() {
    let mut s = state();
    s.view.push_line(LineKind::Assistant, "助手回复一\n第二行");
    s.view.push_line(LineKind::User, "用户消息一");
    s.view.push_line(LineKind::Assistant, "助手回复二");
    let user_id = s.view.sidebar_outline()[0].0;
    s.view.sidebar.open = true;
    reducer::update(&mut s, UiEvent::SidebarJump(user_id));
    // 锁定到该 entry，侧边栏保持打开（用户可 Ctrl+B 自行关闭）。
    assert!(matches!(
        s.view.scroll_mode,
        ScrollMode::Locked(anchor) if anchor.entry_id == user_id
    ));
    assert!(s.view.sidebar.open, "跳转后侧边栏应保持打开");
}
