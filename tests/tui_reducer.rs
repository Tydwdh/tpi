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
    assert_eq!(s.pending_message.as_deref(), Some("你好 world"));
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
    assert_eq!(s1.pending_message, s2.pending_message);
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
    assert_eq!(s.pending_message.as_deref(), Some("/help"));
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
