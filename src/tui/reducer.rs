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

/// 搜索模式的按键路由（§14）：字符/Backspace 更新 query；Enter/F3 下一个；
/// Shift+Enter 上一个；Esc 关闭（不强制跳回底部）。
fn handle_search_key(
    state: &mut UiState,
    key: KeyEvent,
    effects: &mut Vec<UiEffect>,
) -> Vec<UiEffect> {
    match key.code {
        KeyCode::Char(c) => {
            if c == 'f' && key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+F 已打开：无操作。
            } else {
                let mut query = state
                    .view
                    .search
                    .as_ref()
                    .map(|s| s.query.clone())
                    .unwrap_or_default();
                query.push(c);
                state.view.update_search_query(&query);
            }
        }
        KeyCode::Backspace => {
            let mut query = state
                .view
                .search
                .as_ref()
                .map(|s| s.query.clone())
                .unwrap_or_default();
            query.pop();
            state.view.update_search_query(&query);
        }
        KeyCode::Enter => {
            let forward = !key.modifiers.contains(KeyModifiers::SHIFT);
            state.view.search_jump(forward);
        }
        KeyCode::F(3) => {
            state.view.search_jump(true);
        }
        KeyCode::Esc => {
            state.view.search = None;
        }
        _ => {}
    }
    std::mem::take(effects)
}

/// 处理单个按键事件（空闲与运行中共用；行为与迁移前的 handle_key 一致）。
/// 键盘路由优先级（§11）：Overlay > Modal > Search > Menu > Composer。
fn handle_key(state: &mut UiState, key: KeyEvent) -> Vec<UiEffect> {
    let mut effects = Vec::new();

    // BUG-004：Ctrl-C 必须在 raw mode 下作为按键处理（Windows 下 crossterm raw
    // mode 清除 ENABLE_PROCESSED_INPUT，Ctrl-C 不会产生 CTRL_C_EVENT，tokio 的
    // ctrl_c() 信号 handler 不会触发；此前 reducer 直接忽略该按键 → 取消失效）。
    // 语义与 Esc 同层：Overlay > Modal > Menu > 运行中取消；空闲时退出。
    // （`ctrl_c()` handler 保留，作为 -p 模式/非 raw 场景的兜底。）
    let is_ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'));
    if is_ctrl_c {
        if state.view.overlay.is_some() {
            state.view.close_overlay();
        } else if state.view.modal.is_some() {
            state.view.close_modal();
        } else if state.view.menu.is_some() {
            state.view.menu = None;
        } else if state.view.search.is_some() {
            // Ctrl-C 在搜索打开时先关闭搜索（避免误退出/误取消，与 Esc 语义一致）。
            state.view.search = None;
        } else if state.running {
            // §6.2：Ctrl-C 打断当前 run（等价 Esc，保留 session）。
            effects.push(UiEffect::CancelRun);
        } else {
            // 空闲 Ctrl-C：退出 TUI（与 ctrl_c handler 的语义一致）。
            effects.push(UiEffect::Quit);
        }
        return effects;
    }

    // 弹层（Modal/Overlay）打开时：只响应导航/关闭/菜单键，普通按键不得写入 composer
    // （否则用户打字全进后台输入框，关弹层后输入框出现乱码）。
    let blocking = state.view.overlay.is_some() || state.view.modal.is_some();
    if blocking {
        let allowed = matches!(
            key.code,
            KeyCode::Esc | KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) || (key.code == KeyCode::Enter && state.view.menu.is_some());
        if !allowed {
            return effects;
        }
    }
    // §14：搜索打开时按键路由进搜索（输入/跳转/关闭）。
    if state.view.overlay.is_none() && state.view.modal.is_none() && state.view.search.is_some() {
        return handle_search_key(state, key, &mut effects);
    }
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            state.editor.insert_char('\n');
            state.sync_input();
        }
        // T6（§23）：Shift+Enter / Ctrl+J 换行（Alt+Enter 兼容保留）。
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
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
                state.push_pending(text);
            }
            // 提交后 editor 已清空，必须同步 view.input，否则发送的文本仍显示在输入框。
            state.sync_input();
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
            // §49：Esc 优先级 = overlay > modal > menu > run 取消。
            if state.view.overlay.is_some() {
                state.view.close_overlay();
            } else if state.view.modal.is_some() {
                state.view.close_modal();
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
        KeyCode::Home => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // §25：Ctrl+Home 跳到历史最顶部。
                state.view.jump_to_top();
            } else {
                state.editor.home();
            }
        }
        KeyCode::End => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // 整改 C：Ctrl+End 恢复 follow-tail（scroll lock 中）。
                state.view.follow_tail();
            } else {
                state.editor.end();
            }
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
            // §13：Alt+Up 跳到上一条 User entry（基于 EntryId 查找）。
            state.view.jump_to_user_turn(false);
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
            state.view.jump_to_user_turn(true);
        }
        KeyCode::Up => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut()
                    && !menu.items.is_empty()
                {
                    menu.selected = (menu.selected + menu.items.len() - 1) % menu.items.len();
                }
            } else if let Some(modal) = &mut state.view.modal {
                // BUG-013：Modal 提示 ↑/↓ 滚动——让提示与实际行为一致。
                modal.scroll = modal.scroll.saturating_sub(1);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_sub(1);
            } else if !state.editor.move_up() {
                // §12：到第一 logical line 后才进入 prompt history。
                state.editor.history_up();
                state.sync_input();
            }
            state.sync_input();
            refresh_menus(state);
        }
        KeyCode::Down => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut()
                    && !menu.items.is_empty()
                {
                    menu.selected = (menu.selected + 1) % menu.items.len();
                }
            } else if let Some(modal) = &mut state.view.modal {
                // BUG-013：Modal ↑/↓ 滚动。
                modal.scroll = modal.scroll.saturating_add(1);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_add(1);
            } else if !state.editor.move_down() {
                // §12：到最后一个 logical line 后才进入 prompt history。
                state.editor.history_down();
                state.sync_input();
            }
            state.sync_input();
            refresh_menus(state);
        }
        KeyCode::PageUp => {
            if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_sub(10);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_sub(10);
            } else {
                state.view.scroll_up(8);
            }
        }
        KeyCode::PageDown => {
            if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_add(10);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_add(10);
            } else {
                state.view.scroll_down(8);
            }
        }
        KeyCode::Char(c) => {
            // Ctrl-C 已在 handle_key 顶部处理（BUG-004）；此处不再忽略。
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
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'j' {
                // §23：Ctrl+J 换行（终端 LF 常映射为 Enter，此处兜底）。
                state.editor.insert_char('\n');
                state.sync_input();
                return effects;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'f' {
                // §14：Ctrl+F 打开转录搜索。
                state.view.open_search();
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
            // While a Modal/Overlay is open, Paste must not write into the composer
            // behind it (keys are already blocked; Paste is a separate event).
            if state.view.overlay.is_some() || state.view.modal.is_some() {
                return Vec::new();
            }
            if state.view.search.is_some() {
                // BUG-014：搜索打开时粘贴应进入搜索框，而不是 composer。
                let mut query = state
                    .view
                    .search
                    .as_ref()
                    .map(|s| s.query.clone())
                    .unwrap_or_default();
                query.push_str(&text);
                state.view.update_search_query(&query);
            } else {
                state.editor.insert_str(&text);
                state.sync_input();
                refresh_menus(state);
            }
            Vec::new()
        }
        UiEvent::Key(key) => handle_key(state, key),
        UiEvent::MouseScrollUp => {
            if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_sub(3);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_sub(3);
            } else {
                state.view.scroll_up(3);
            }
            Vec::new()
        }
        UiEvent::MouseScrollDown => {
            if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_add(3);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_add(3);
            } else {
                state.view.scroll_down(3);
            }
            Vec::new()
        }
        UiEvent::ClickTool(id) => {
            if state.view.modal.is_some() || state.view.overlay.is_some() {
                return Vec::new(); // 弹层打开时鼠标点击不得打开后台 overlay
            }
            state.view.open_tool_overlay(id);
            Vec::new()
        }
        UiEvent::ClickReasoning(id) => {
            if state.view.modal.is_some() || state.view.overlay.is_some() {
                return Vec::new();
            }
            state.view.open_reasoning_overlay(id);
            Vec::new()
        }
        UiEvent::ScrollbarClick(row) => {
            if state.view.modal.is_some() || state.view.overlay.is_some() {
                return Vec::new(); // 弹层打开时不得滚动背后 transcript
            }
            // §24：scrollbar 点击/拖拽 → 按比例跳到绝对位置。
            let area = state.view.transcript_rows.max(1) as f64;
            state.view.scroll_to_ratio(row as f64 / area);
            Vec::new()
        }
        UiEvent::Agent(event) => {
            handle_agent(state, event);
            Vec::new()
        }
    }
}
