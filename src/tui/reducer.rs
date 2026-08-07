//! Reducer（TPI_TUI_V2_TASK §26-27）：`update(state, event) -> Vec<UiEffect>`。
//!
//! 只修改状态，不运行 provider/bash，不写 stdout。跨边界动作
//! （退出/取消 run/恢复 session）以 effect 返回，由 app 层执行。
//! 键盘路由优先级（§11）：Overlay > Menu > Composer > Transcript 导航。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::agent::{DeltaKind, RuntimeEvent};
use crate::tui::effect::UiEffect;
use crate::tui::event::UiEvent;
use crate::tui::model::{LineKind, MenuKind, StatusLine};
use crate::tui::state::UiState;

/// 输入变化后重建菜单：`@` 文件菜单优先（维护中不触碰），否则斜杠命令菜单。
fn refresh_menus(state: &mut UiState) {
    if state.view.has_at_token() {
        state.view.refresh_at_menu();
    } else {
        state.view.refresh_command_menu();
    }
}

/// 处理单个按键事件（空闲与运行中共用；行为与迁移前的 handle_key 一致）。
fn handle_key(state: &mut UiState, key: KeyEvent) -> Vec<UiEffect> {
    let mut effects = Vec::new();
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            state.editor.insert_char('\n');
            state.sync_input();
        }
        KeyCode::Enter => {
            // 命令菜单打开时先补全为选中命令（Claude Code 式菜单交互）。
            if state.view.menu.is_some()
                && let Some((label, kind)) = state.view.selected_menu_item()
            {
                match kind {
                    MenuKind::Session => {
                        // 会话恢复由 app 执行（需要重建 SessionLog/history）。
                        state.pending_session = Some(label);
                        state.view.menu = None;
                        return effects;
                    }
                    _ => state.view.complete_menu_command(),
                }
            }
            let text = {
                // P0-5：菜单补全结果先写回 editor（输入事实源），再提交。
                // 无菜单时 view.input 与 editor 文本一致，set_text 无副作用。
                state.editor.set_text(state.view.input.clone());
                state.sync_input();
                state.editor.submit()
            };
            if !text.is_empty() {
                state.pending_message = Some(text);
            }
            refresh_menus(state);
        }
        KeyCode::Tab => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut()
                    && menu.items.len() > 1
                {
                    menu.selected = (menu.selected + 1) % menu.items.len();
                }
                state.view.complete_menu_command();
                // P0-5：补全结果写回 editor（它是输入事实源）。
                state.editor.set_text(state.view.input.clone());
                state.sync_input();
            }
        }
        KeyCode::Esc => {
            if state.view.overlay.is_some() {
                state.view.close_overlay();
            } else if state.view.menu.is_some() {
                state.view.menu = None;
            } else if state.running {
                // §6.2：Esc 打断当前 run（等价 Ctrl-C，保留 session）。
                effects.push(UiEffect::CancelRun);
            }
        }
        KeyCode::Backspace => {
            state.editor.backspace();
            state.sync_input();
            refresh_menus(state);
        }
        KeyCode::Delete => {
            state.editor.delete();
            state.sync_input();
            refresh_menus(state);
        }
        KeyCode::Left => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                state.editor.move_word_left();
            } else {
                state.editor.move_left();
            }
            state.sync_input();
        }
        KeyCode::Right => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                state.editor.move_word_right();
            } else {
                state.editor.move_right();
            }
            state.sync_input();
        }
        KeyCode::Home => state.editor.home(),
        KeyCode::End => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // 整改 C：Ctrl+End 恢复 follow-tail（scroll lock 中）。
                state.view.follow_tail();
            } else {
                state.editor.end();
            }
        }
        KeyCode::Up => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut()
                    && !menu.items.is_empty()
                {
                    menu.selected = (menu.selected + menu.items.len() - 1) % menu.items.len();
                }
            } else {
                state.editor.history_up();
                state.sync_input();
                refresh_menus(state);
            }
        }
        KeyCode::Down => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut()
                    && !menu.items.is_empty()
                {
                    menu.selected = (menu.selected + 1) % menu.items.len();
                }
            } else {
                state.editor.history_down();
                state.sync_input();
                refresh_menus(state);
            }
        }
        KeyCode::PageUp => {
            if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_sub(10);
            } else {
                state.view.scroll_up(8);
            }
        }
        KeyCode::PageDown => {
            if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_add(10);
            } else {
                state.view.scroll_down(8);
            }
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                return effects; // Ctrl-C 由 ctrl_c handler 处理。
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'u' {
                state.editor.clear();
                state.sync_input();
                refresh_menus(state);
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'a' {
                state.editor.home();
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'e' {
                state.editor.end();
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'w' {
                state.editor.delete_word_back();
                state.sync_input();
                refresh_menus(state);
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'k' {
                state.editor.delete_to_end();
                state.sync_input();
                refresh_menus(state);
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::ALT) && c == 't' {
                state.view.reasoning_visible = !state.view.reasoning_visible;
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::ALT) && c == 'e' {
                // 打开最近一张工具卡片的详情 Overlay（鼠标点击的键盘等价）。
                state.view.open_last_tool_overlay();
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::ALT) && c == 'o' {
                state.view.open_failed_tool_overlay();
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::ALT) && c == '[' {
                state.view.cycle_tool_overlay(-1);
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::ALT) && c == ']' {
                state.view.cycle_tool_overlay(1);
                return effects;
            }
            state.editor.insert_char(c);
            state.sync_input();
            refresh_menus(state);
        }
        _ => {}
    }
    effects
}

/// Agent 运行时事件 → 视图状态（reducer 内做纯状态转换）。
fn handle_agent(state: &mut UiState, event: RuntimeEvent) {
    let view = &mut state.view;
    match event {
        RuntimeEvent::AssistantDelta { kind, text, .. } => match kind {
            DeltaKind::Text => view.push_stream_delta(LineKind::Assistant, &text),
            DeltaKind::Reasoning => view.push_stream_delta(LineKind::Reasoning, &text),
        },
        RuntimeEvent::ToolStarted {
            call_id,
            name,
            target,
            command,
        } => {
            view.begin_tool(call_id.to_string(), name.clone(), Some(target), command);
            if let StatusLine::Running { tool, .. } = &mut view.status {
                *tool = name;
            }
        }
        RuntimeEvent::ToolCompleted {
            call_id,
            name,
            status,
            duration_ms,
            exit_code,
            tail,
        } => {
            view.finish_tool(
                call_id.to_string(),
                name,
                status,
                duration_ms,
                exit_code,
                tail,
            );
        }
        RuntimeEvent::ToolOutputDelta { call_id, text, .. } => {
            view.append_tool_output(call_id.to_string(), text);
        }
        RuntimeEvent::ContextUsage { projected, usable } => {
            view.context_usage = Some((projected, usable));
        }
        RuntimeEvent::BudgetWarning => {
            // P1-3：接近 wall-time 预算（此前只写日志，用户看不到）。
            view.push_line(
                LineKind::System,
                "⚠ 接近 wall-time 预算：run 即将被取消，请尽快收敛或保存进度".to_string(),
            );
        }
        RuntimeEvent::TurnStarted { turn } => {
            view.turn = turn;
            view.status = StatusLine::Running {
                turn,
                tool: "模型生成中".into(),
            };
        }
        RuntimeEvent::PlanUpdated { plan } => {
            view.plan = Some(plan);
        }
    }
}

/// 主入口：状态转换 + 效果（§26：reducer 只修改状态）。
pub fn update(state: &mut UiState, event: UiEvent) -> Vec<UiEffect> {
    match event {
        UiEvent::Tick => {
            state.view.anim_tick += 1;
            Vec::new()
        }
        UiEvent::Paste(text) => {
            state.editor.insert_str(&text);
            state.sync_input();
            refresh_menus(state);
            Vec::new()
        }
        UiEvent::Key(key) => handle_key(state, key),
        UiEvent::MouseScrollUp => {
            if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_sub(3);
            } else {
                state.view.scroll_up(3);
            }
            Vec::new()
        }
        UiEvent::MouseScrollDown => {
            if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_add(3);
            } else {
                state.view.scroll_down(3);
            }
            Vec::new()
        }
        UiEvent::ClickTool(id) => {
            state.view.open_tool_overlay(id);
            Vec::new()
        }
        UiEvent::ClickReasoning(index) => {
            state.view.open_reasoning_overlay(index);
            Vec::new()
        }
        UiEvent::Agent(event) => {
            handle_agent(state, event);
            Vec::new()
        }
    }
}
