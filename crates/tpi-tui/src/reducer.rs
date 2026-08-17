//! Reducer（TPI_TUI_V2_TASK §26-27）：`update(state, event) -> Vec<UiEffect>`。
//!
//! 只修改状态，不运行 provider/bash，不写 stdout。跨边界动作
//! （退出/取消 run/恢复 session/打开链接/复制）以 effect 返回，由 app 层执行。
//! 键盘路由优先级（§11）：Overlay > Menu > Composer > Transcript 导航。
//! 键位来自 `[ui.keymap]`（成熟化）：按键 → KeyAction 的绑定表由 UiState 持有
//! （app 启动时注入），未配置动作保持内建默认；状态可重放不变量不受影响。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::effect::UiEffect;
use crate::event::UiEvent;
use crate::keymap::KeyAction;
use crate::model::{LineKind, MenuKind, QuestionMode, StatusLine};
use crate::state::UiState;
use tpi_agent::agent::{DeltaKind, RuntimeEvent};
use tpi_session::Usage;

/// 当前本地时间 `HH:MM:SS`（重连/续写提示的时间戳，Claude Code 式）。
/// 本地时区不可用时退回 UTC；失败（几乎不可能）退回空串。
fn now_hhmmss() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

/// 输入变化后重建菜单：`@` 文件菜单优先（维护中不触碰），否则斜杠命令菜单。
///
/// §用户诉求（/sessions 菜单）：Session 菜单是 Modal+Menu 组合、由 app 层
/// 显式挂载，与输入无关——↑/↓/Tab 等导航会调用本函数，若走命令菜单刷新
/// 会把菜单清成 None（refresh_command_menu 先 `self.menu = None`），导致
/// “在会话列表上下移动后列表突然消失”。对 Session 菜单保持不动。
/// §用户诉求：/sessions 悬浮窗（Modal）随选中项更新对话预览——
/// ↑/↓ 移动 Session 菜单后同步 modal.body。非 Session 菜单无操作。
fn sync_session_preview(state: &mut UiState) {
    let Some(menu) = state.view.menu.as_ref() else {
        return;
    };
    if menu.kind != MenuKind::Session {
        return;
    }
    let Some(preview) = menu.filtered_preview(menu.selected) else {
        return;
    };
    let body = crate::model::preview_lines_to_body(preview);
    if let Some(modal) = &mut state.view.modal {
        modal.body = body;
        modal.scroll = 0;
    }
}

fn refresh_menus(state: &mut UiState) {
    // Session / Theme 菜单是“Modal + 菜单”组合、由 app 层显式挂载，与输入
    // 无关——↑/↓/Tab 等导航不得经命令菜单刷新把它们清掉。
    if matches!(
        state.view.menu.as_ref().map(|m| m.kind),
        Some(MenuKind::Session) | Some(MenuKind::Theme) | Some(MenuKind::Model)
    ) {
        return;
    }
    if state.view.has_at_token() {
        state.view.refresh_at_menu();
    } else {
        state.view.refresh_command_menu();
    }
}

/// `request_input` 交互模态的按键路由（opencode 形态）。
/// 优先级最高：打开时拦截所有按键（不落 composer）。
///
/// - Selecting：↑↓/k/j 移动、1-9 快选、Enter 确认（单问题提交；多问题
///   下一 tab 或 Review）、Tab/←→/h/l 切 tab、Esc 拒绝
/// - EditingCustom：字符输入、Enter 确认、Esc 取消编辑
/// - Review：Enter 提交全部、Esc 拒绝
fn handle_question_key(state: &mut UiState, key: KeyEvent, effects: &mut Vec<UiEffect>) -> bool {
    let Some(q) = state.view.question.as_mut() else {
        return false;
    };
    if q.mode == QuestionMode::Done {
        // 已提交/拒绝：关闭模态（effect 已发出，app 处理）。
        state.view.question = None;
        return true;
    }
    if q.mode == QuestionMode::EditingCustom {
        match key.code {
            KeyCode::Char(c) => {
                // ISSUE-018：Ctrl/Alt/Super 组合键不得当字面字符插入自定义回答
                //（Ctrl+C/Ctrl+Z/Ctrl+A 此前变成字母）。模态拦截优先于 keymap，
                // 这里必须过滤修饰位。
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                {
                    return true;
                }
                q.custom_input.push(c);
                return true;
            }
            KeyCode::Backspace => {
                q.custom_input.pop();
                return true;
            }
            KeyCode::Enter => {
                let val = std::mem::take(&mut q.custom_input);
                if !val.trim().is_empty() {
                    let i = q.tab;
                    if q.questions[i].multiple {
                        q.answers[i].push(val.trim().to_string());
                        q.mode = QuestionMode::Selecting;
                    } else {
                        q.answers[i] = vec![val.trim().to_string()];
                        // 单选自定义回答：单问题直接提交；多问题进下一 tab/Review。
                        if q.questions.len() == 1 {
                            submit_single(q, effects);
                        } else {
                            advance_tab(q);
                        }
                    }
                } else {
                    q.mode = QuestionMode::Selecting;
                }
                return true;
            }
            KeyCode::Esc => {
                q.custom_input.clear();
                q.mode = QuestionMode::Selecting;
                return true;
            }
            _ => return true, // 编辑中拦截所有其他键。
        }
    }
    if q.mode == QuestionMode::Review {
        match key.code {
            KeyCode::Enter => {
                // §13 修复：未全部回答时不提交（与 submit_single 的空答案拦截
                // 同语义）--否则模型收到“Q: （空）”仍不知答案；提示用户先补答。
                if !q.all_answered() {
                    // §askuser 修复：未全答时 Enter 不再静默——跳到第一个未答
                    // 问题并给出明确提示（此前静默拦截让用户“以为全部提交”，
                    // 只看到模态没反应，感觉被卡住）。
                    let unanswered = q
                        .questions
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| q.answers.get(*i).is_none_or(|a| a.is_empty()))
                        .count();
                    let first = q
                        .questions
                        .iter()
                        .enumerate()
                        .find(|(i, _)| q.answers.get(*i).is_none_or(|a| a.is_empty()))
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    q.mode = QuestionMode::Selecting;
                    q.tab = first;
                    q.selected = 0;
                    state.view.transient_hint = Some(format!(
                        "还有 {unanswered} 个问题未回答，已跳到第 {} 题；全部答完后再到 Review 提交",
                        first + 1
                    ));
                    return true;
                }
                q.mode = QuestionMode::Done;
                let text = q.answers_text();
                effects.push(UiEffect::QuestionSubmitted(text));
                return true;
            }
            KeyCode::Esc => {
                q.mode = QuestionMode::Done;
                q.rejected = true;
                effects.push(UiEffect::QuestionRejected);
                return true;
            }
            // §13 修复：Review 不是死胡同--导航键回到第一个未答问题的
            // 编辑页（全答时回到第一题），用户可补答后再进 Review。
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Char('j')
            | KeyCode::Char('k') => {
                q.mode = QuestionMode::Selecting;
                q.tab = q
                    .questions
                    .iter()
                    .enumerate()
                    .find(|(i, _)| q.answers.get(*i).is_none_or(|a| a.is_empty()))
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                q.selected = 0;
                return true;
            }
            _ => return true,
        }
    }
    // Selecting 模式。
    let multi_q = q.questions.len() > 1;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            let total = q.option_count();
            q.selected = if total == 0 {
                0
            } else {
                (q.selected + total - 1) % total
            };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let total = q.option_count();
            q.selected = if total == 0 {
                0
            } else {
                (q.selected + 1) % total
            };
        }
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let n = (c as usize) - ('0' as usize);
            if n <= q.option_count() {
                q.selected = n - 1;
                select_current_option(q);
                // multiple：toggle 后停留（用户可继续多选）；
                // 单选：单问题提交；多问题进下一 tab/Review。
                if !q.questions[q.tab].multiple {
                    if !multi_q {
                        submit_single(q, effects);
                    } else {
                        advance_tab(q);
                    }
                } else if q.mode == QuestionMode::Review {
                    // §bug 修复：多问题 multiple 完成项被数字快选选中时，
                    // select_current_option 已把 mode 置为 Review——与单选
                    // 一致应进下一 tab（最后一题才进 Review），而非停在
                    // Review 把未答问题一并展示。
                    advance_tab(q);
                }
            }
        }
        KeyCode::Enter => {
            select_current_option(q);
            // multiple 的“完成”项：单问题直接提交；多问题进下一 tab
            //（与单选一致），最后一题的完成项才进 Review。
            if q.mode == QuestionMode::Done && !multi_q {
                effects.push(UiEffect::QuestionSubmitted(q.answers_text()));
            } else if q.mode == QuestionMode::Review {
                // §bug 修复：多问题 multiple 完成项——此前直接停在 Review
                //（未答问题显示“（未回答）”，用户以为要全部提交）；
                // 改为进下一 tab（advance_tab 在最后一题自然进 Review）。
                advance_tab(q);
            } else if !q.questions[q.tab].multiple {
                if !multi_q {
                    submit_single(q, effects);
                } else {
                    advance_tab(q);
                }
            }
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            if multi_q {
                advance_tab(q);
            }
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            if multi_q {
                q.tab = (q.tab + q.questions.len() - 1) % q.questions.len();
                q.selected = 0;
            }
        }
        KeyCode::Esc => {
            q.mode = QuestionMode::Done;
            q.rejected = true;
            effects.push(UiEffect::QuestionRejected);
        }
        // §askuser 修复：用户直接打字即进入自定义回答——此前 Selecting
        // 模式拦截所有字符键（除数字快选/导航字母），单问题有选项时用户
        // 想输入自定义文本没有任何反应（“只有一个用户输入时无法输入”）。
        // 当前问题允许自定义时，字符键直接进入编辑并插入该字符。
        KeyCode::Char(c) if q.questions[q.tab].custom => {
            q.mode = QuestionMode::EditingCustom;
            q.custom_input.push(c);
        }
        _ => {}
    }
    true
}

/// 选中当前高亮项（选项/自定义项/完成项）。
fn select_current_option(q: &mut crate::model::QuestionModalState) {
    let cur = &q.questions[q.tab];
    if q.on_done() {
        // multiple 且无 custom：完成项 → 提交（单问题）或 Review（多问题）。
        q.mode = if q.questions.len() == 1 {
            QuestionMode::Done
        } else {
            QuestionMode::Review
        };
        return;
    }
    if q.on_custom() {
        // 自定义项：进入编辑（multiple 直接编辑追加；单选编辑后提交）。
        q.mode = QuestionMode::EditingCustom;
        return;
    }
    let Some(option) = cur.options.get(q.selected) else {
        return;
    };
    let label = option.label.clone();
    if cur.multiple {
        let i = q.tab;
        if let Some(pos) = q.answers[i].iter().position(|a| a == &label) {
            q.answers[i].remove(pos);
        } else {
            q.answers[i].push(label);
        }
    } else {
        q.answers[q.tab] = vec![label];
    }
}

/// 单问题提交（发出 QuestionSubmitted）。
fn submit_single(q: &mut crate::model::QuestionModalState, effects: &mut Vec<UiEffect>) {
    if q.answers[0].is_empty() {
        return; // 未选不提交。
    }
    q.mode = QuestionMode::Done;
    effects.push(UiEffect::QuestionSubmitted(q.answers_text()));
}

/// 多问题：前进到下一 tab 或 Review。
///
/// §bug 修复：多选完成项路径调用时 mode 已被 `select_current_option` 置为
/// Review——前进到下一题必须回到 Selecting，否则渲染仍停在 Review 页。
/// 单选路径调用时 mode 本就是 Selecting，重置无副作用。
fn advance_tab(q: &mut crate::model::QuestionModalState) {
    if q.tab + 1 < q.questions.len() {
        q.tab += 1;
        q.selected = 0;
        q.mode = QuestionMode::Selecting;
    } else {
        q.mode = QuestionMode::Review;
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
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'u' {
                // Ctrl+U 在搜索框也应清空搜索词（与 composer 一致），
                // 不得把 'u' 当普通字符插入。
                state.view.update_search_query("");
                return std::mem::take(effects);
            }
            if c == 'f' && key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+F 已打开：无操作。
            } else if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            {
                // ISSUE-016：其余 Ctrl/Alt/Super 组合键在搜索打开期间被当普通
                // 字符插入搜索词（Ctrl+C/Ctrl+Z/Ctrl+D/Ctrl+A/Ctrl+W…全变成
                // 字母）。搜索路由在 keymap 之前拦截，这里必须过滤修饰位——
                // 组合键要么另有语义要么应忽略，绝不能进搜索词。
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
        KeyCode::PageUp => {
            // 搜索打开时 PgUp/PgDn 仍可滚动 transcript（与鼠标滚轮一致），
            // 不得被搜索路由吞掉。
            state.view.scroll_up(8);
        }
        KeyCode::PageDown => {
            state.view.scroll_down(8);
        }
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.view.jump_to_top();
        }
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.view.follow_tail();
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
/// 键位语义由 `state.keymap.action(key)` 解析（`[ui.keymap]` 可覆盖）。
fn handle_key(state: &mut UiState, key: KeyEvent) -> Vec<UiEffect> {
    let mut effects = Vec::new();
    // 过渡提示下一次键盘操作清除（同一个键可重新设置）。
    state.view.transient_hint = None;

    // `request_input` 模态优先（opencode 形态：拦截所有按键，不落 composer）。
    if state.view.question.is_some() {
        handle_question_key(state, key, &mut effects);
        return effects;
    }

    // §子代理内部视图（opencode 形态）：打开时拦截导航键——←/→ 在多个
    // subagent 卡间切换、↑↓/PgUp/PgDn 滚动内部文本流、Esc/Backspace 返回
    // 父代理；其余按键不透传（浏览模式，不落 composer）。
    if state.view.subagent.active.is_some() {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => state.view.close_subagent(),
            KeyCode::Left | KeyCode::Char('h') => state.view.cycle_subagent(-1),
            KeyCode::Right | KeyCode::Char('l') => state.view.cycle_subagent(1),
            KeyCode::Up | KeyCode::Char('k') => state.view.scroll_subagent(-1),
            KeyCode::Down | KeyCode::Char('j') => state.view.scroll_subagent(1),
            KeyCode::PageUp => state.view.scroll_subagent(-10),
            KeyCode::PageDown => state.view.scroll_subagent(10),
            _ => {}
        }
        return effects;
    }

    // Ctrl+C 语义（§用户诉求）：只用于复制——Windows Terminal 选中文本后
    // Ctrl+C 由终端优先复制（不传给应用）；未选中时到达应用的 Ctrl+C 静默忽略，
    // 不取消/退出（取消用 Esc、退出用 Ctrl+D）。
    // 注意：raw mode 下 crossterm 把 Ctrl+C 读成 KeyEvent；若不做任何处理，
    // 它会落到 Char('c') 分支变成输入 'c'，因此必须显式忽略（默认 keymap 绑定）。

    // 弹层（Modal/Overlay）打开时：只响应导航/关闭/菜单键，普通按键不得写入 composer
    // （否则用户打字全进后台输入框，关弹层后输入框出现乱码）。
    let blocking = state.view.overlay.is_some() || state.view.modal.is_some();
    if blocking {
        // §成熟化：Link Overlay 响应 Enter（打开 URL）与 c（复制 URL）。
        if let Some(overlay) = &state.view.overlay
            && overlay.kind == crate::model::OverlayKind::Link
        {
            let url = overlay.body.clone();
            match key.code {
                KeyCode::Enter => {
                    state.view.overlay = None;
                    return vec![UiEffect::OpenUrl(url)];
                }
                KeyCode::Char('c') => {
                    state.view.overlay = None;
                    return vec![UiEffect::CopyText(url)];
                }
                _ => {}
            }
        }
        let allowed = matches!(
            key.code,
            KeyCode::Esc | KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) || (key.code == KeyCode::Enter && state.view.menu.is_some());
        if !allowed {
            // §oh-my-pi（type-to-filter）：Modal 型菜单（/sessions /theme /model）
            // 打开时，字符键进菜单过滤（不落 composer）、Backspace 删过滤词。
            if let Some(menu) = state.view.menu.as_mut()
                && menu.is_browser_menu()
            {
                match key.code {
                    KeyCode::Char(c)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) && !c.is_ascii_control() =>
                    {
                        menu.filter_push(c);
                        sync_session_preview(state);
                        state.view.transient_hint = None;
                    }
                    KeyCode::Backspace => {
                        menu.filter_backspace();
                        sync_session_preview(state);
                        state.view.transient_hint = None;
                    }
                    _ => {}
                }
            }
            return effects;
        }
    }
    // §14：搜索打开时按键路由进搜索（输入/跳转/关闭）。
    if state.view.overlay.is_none() && state.view.modal.is_none() && state.view.search.is_some() {
        return handle_search_key(state, key, &mut effects);
    }
    let Some(action) = state.keymap.action(key) else {
        return effects;
    };
    match action {
        // T6（§23）：Shift+Enter / Ctrl+J / Alt+Enter 换行。
        KeyAction::InsertNewline => {
            state.editor.insert_char('\n');
            state.sync_input();
        }
        KeyAction::Submit => {
            // 命令菜单打开时先补全为选中命令（Claude Code 式菜单交互）。
            if state.view.menu.is_some()
                && let Some((label, kind)) = state.view.selected_menu_item()
            {
                match kind {
                    MenuKind::Session => {
                        // 会话恢复由 app 执行（需要重建 SessionLog/history）。
                        // /sessions = Modal + 菜单一个整体：选中后一并关闭，
                        // 不得把“会话列表”留在恢复后的屏幕上。
                        state.pending_session = Some(label);
                        state.view.menu = None;
                        state.view.modal = None;
                        return effects;
                    }
                    MenuKind::Theme => {
                        // 主题应用由 app 执行（应用 renderer 主题 + 写配置）。
                        // /theme = Modal + 菜单一个整体：选中后一并关闭。
                        state.pending_theme = Some(label);
                        state.view.menu = None;
                        state.view.modal = None;
                        return effects;
                    }
                    MenuKind::Model => {
                        // 模型切换由 app 执行（重建 provider + 更新 config.model）。
                        // /model = Modal + 菜单一个整体：选中后一并关闭。
                        state.pending_model = Some(label);
                        state.view.menu = None;
                        state.view.modal = None;
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
            // §用户诉求：提交前把大粘贴占位符展开为全文，一起发送。
            let text = if text.is_empty() {
                text
            } else {
                crate::paste::expand_paste_placeholders(&text, &state.pasted)
            };
            // editor 已提交并清空，旁路内容已展开进消息；及时释放真实粘贴
            // 文本，避免大剪贴板在整个会话中常驻内存。
            state.pasted.clear();
            if !text.is_empty() {
                // §PointerHit：运行中提交立即在 footer 提示（不写 transcript，
                // 避免消费时重复显示；实际 User 消息在消费时入 transcript）。
                if state.running {
                    state.view.transient_hint = Some(format!(
                        "已排队：{}",
                        crate::text::truncate_middle_utf8(&text, 160, "…")
                    ));
                }
                state.push_pending(text);
            }
            // 提交后 editor 已清空，必须同步 view.input，否则发送的文本仍显示在输入框。
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::MenuNext => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut() {
                    let len = menu.filtered_len();
                    if len > 1 {
                        menu.selected = (menu.selected + 1) % len;
                    }
                }
                state.view.complete_menu_command();
                // P0-5：补全结果写回 editor（它是输入事实源）。
                state.editor.set_text(state.view.input.clone());
                state.sync_input();
            }
            sync_session_preview(state);
        }
        KeyAction::Escape => {
            // §49：Esc 优先级 = overlay > modal > menu > run 取消。
            // §oh-my-pi：Modal 型菜单过滤词非空时，Esc 先清空过滤词（回到全列表），
            // 再按一次才关闭菜单——避免误关（与搜索 Esc 语义一致）。
            if state.view.menu.is_some()
                && state
                    .view
                    .menu
                    .as_ref()
                    .is_some_and(|m| m.is_browser_menu())
                && state
                    .view
                    .menu
                    .as_ref()
                    .is_some_and(|m| !m.filter.is_empty())
            {
                if let Some(menu) = state.view.menu.as_mut() {
                    menu.filter.clear();
                    menu.clamp_selected();
                }
                sync_session_preview(state);
                return effects;
            }
            if state.view.overlay.is_some() {
                state.view.close_overlay();
            } else if state.view.modal.is_some() {
                state.view.close_modal();
                // /sessions、/theme 浏览器 = Modal + 菜单一个整体：Esc 一次关闭两者。
                if matches!(
                    state.view.menu.as_ref().map(|m| m.kind),
                    Some(MenuKind::Session) | Some(MenuKind::Theme) | Some(MenuKind::Model)
                ) {
                    state.view.menu = None;
                }
            } else if state.view.menu.is_some() {
                state.view.menu = None;
            } else if state.running {
                // §6.2：Esc 打断当前 run（等价 Ctrl-C，保留 session）。
                effects.push(UiEffect::CancelRun);
            } else if !state.editor.text().is_empty() {
                // 空闲时 Esc 清空当前输入（人体工学：“退出当前输入”的通用假设）。
                state.editor.clear();
                state.pasted.clear();
                state.sync_input();
                refresh_menus(state);
            }
        }
        KeyAction::Backspace => {
            state.editor.backspace();
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::Delete => {
            state.editor.delete();
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::MoveLeft => {
            state.editor.move_left();
            state.sync_input();
        }
        KeyAction::MoveRight => {
            state.editor.move_right();
            state.sync_input();
        }
        KeyAction::MoveWordLeft => {
            state.editor.move_word_left();
            state.sync_input();
        }
        KeyAction::MoveWordRight => {
            state.editor.move_word_right();
            state.sync_input();
        }
        KeyAction::LineStart => {
            state.editor.home();
            // Home 只改光标位置，必须同步投影，否则硬件光标停在旧位置。
            state.sync_input();
        }
        KeyAction::LineEnd => {
            state.editor.end();
            state.sync_input();
        }
        KeyAction::JumpTranscriptTop => {
            // §25：Ctrl+Home 跳到历史最顶部。
            state.view.jump_to_top();
        }
        KeyAction::FollowTail => {
            // 整改 C：Ctrl+End 恢复 follow-tail（scroll lock 中）。
            state.view.follow_tail();
        }
        KeyAction::JumpPrevUserTurn => {
            // §13：Alt+Up 跳到上一条 User entry（基于 EntryId 查找）。
            if !state.view.jump_to_user_turn(false) {
                state.view.transient_hint = Some("没有用户消息可跳转".into());
            }
        }
        KeyAction::JumpNextUserTurn => {
            if !state.view.jump_to_user_turn(true) {
                state.view.transient_hint = Some("没有用户消息可跳转".into());
            }
        }
        KeyAction::MoveUp => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut()
                    && menu.filtered_len() > 0
                {
                    let len = menu.filtered_len();
                    menu.selected = (menu.selected + len - 1) % len;
                }
                sync_session_preview(state);
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
        KeyAction::MoveDown => {
            if state.view.menu.is_some() {
                if let Some(menu) = state.view.menu.as_mut()
                    && menu.filtered_len() > 0
                {
                    let len = menu.filtered_len();
                    menu.selected = (menu.selected + 1) % len;
                }
                sync_session_preview(state);
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
        KeyAction::PageUp => {
            if state.view.menu.is_some() {
                // §oh-my-pi：长菜单 PgUp/PgDn 翻页（一页 = 可视行数）。
                if let Some(menu) = state.view.menu.as_mut()
                    && menu.filtered_len() > 0
                {
                    let len = menu.filtered_len();
                    menu.selected = menu.selected.saturating_sub(8);
                    menu.selected = menu.selected.min(len - 1);
                }
                sync_session_preview(state);
            } else if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_sub(10);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_sub(10);
            } else {
                state.view.scroll_up(8);
            }
        }
        KeyAction::PageDown => {
            if state.view.menu.is_some() {
                // §oh-my-pi：长菜单 PgUp/PgDn 翻页。
                if let Some(menu) = state.view.menu.as_mut()
                    && menu.filtered_len() > 0
                {
                    let len = menu.filtered_len();
                    menu.selected = (menu.selected + 8).min(len - 1);
                }
                sync_session_preview(state);
            } else if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_add(10);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_add(10);
            } else {
                state.view.scroll_down(8);
            }
        }
        KeyAction::Copy => {
            // Ctrl+C 只做复制（§用户诉求）：有选区复制到剪贴板；无选区静默忽略
            // ——不取消 run（Esc 负责）、不退出（Ctrl+D 负责）。
            if state.view.selection.is_some() {
                effects.push(UiEffect::CopySelection);
            }
        }
        KeyAction::QuitApp => {
            // §用户诉求：退出用 Ctrl+D（与复制分离，避免 Ctrl+C 误触退出）。
            effects.push(UiEffect::Quit);
        }
        KeyAction::ClearInput => {
            state.editor.clear();
            state.pasted.clear();
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::DeleteWordBack => {
            state.editor.delete_word_back();
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::DeleteToLineEnd => {
            state.editor.delete_to_end();
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::OpenSearch => {
            // §14：Ctrl+F 打开转录搜索。
            state.view.open_search();
        }
        KeyAction::ToggleReasoning => {
            // Alt+T：全量切换所有 thinking 卡（历史 + live 同一套按条目状态）。
            state.view.toggle_all_reasoning();
        }
        KeyAction::OpenLastTool => {
            // 打开最近一张工具卡片的详情 Overlay（鼠标点击的键盘等价）。
            state.view.open_last_tool_overlay();
        }
        KeyAction::OpenFailedTool => {
            state.view.open_failed_tool_overlay();
        }
        KeyAction::CycleToolPrev => {
            state.view.cycle_tool_overlay(-1);
        }
        KeyAction::CycleToolNext => {
            state.view.cycle_tool_overlay(1);
        }
        KeyAction::Undo => {
            state.editor.undo();
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::Redo => {
            state.editor.redo();
            state.sync_input();
            refresh_menus(state);
        }
        KeyAction::ToggleSidebar => {
            state.view.toggle_sidebar();
            refresh_menus(state);
        }
        KeyAction::TypedChar(c) => {
            state.editor.insert_char(c);
            state.sync_input();
            refresh_menus(state);
        }
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
            diff,
        } => {
            view.finish_tool(
                (call_id.to_string(), name),
                status,
                duration_ms,
                exit_code,
                tail,
                diff,
            );
        }
        RuntimeEvent::ToolOutputDelta { call_id, text, .. } => {
            view.append_tool_output(call_id.to_string(), text);
        }
        RuntimeEvent::ContextUsage { projected, usable } => {
            view.context_usage = Some((projected, usable));
        }
        RuntimeEvent::UsageUpdated {
            input_tokens,
            output_tokens,
            cache_read_tokens,
        } => {
            // §用户诉求：缓存命中实时展示（Claude Code 式）——每次请求结束就把
            // usage **实时累加**到累计字段（不等 run 结束），footer 的 ↑↓⇄ 与
            // 命中率因此始终同口径（修复：此前只存“最近一次”导致累计值旁边
            // 挂的是本次命中率，口径不一致）。
            view.add_usage(&Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
            });
        }
        RuntimeEvent::BudgetWarning => {
            // P1-3：接近 wall-time 预算（此前只写日志，用户看不到）。
            view.push_line(
                LineKind::System,
                "⚠ 接近 wall-time 预算：run 即将被取消，请尽快收敛或保存进度".to_string(),
            );
        }
        RuntimeEvent::StepStarted { step } => {
            view.step = step;
            view.status = StatusLine::Running {
                step,
                tool: "模型生成中".into(),
            };
        }
        RuntimeEvent::PlanUpdated { plan } => {
            view.plan = Some(plan);
        }
        RuntimeEvent::StreamRecovering { attempt } => {
            // §4.3 第二阶段：text-only 断联后自动续写，不打断用户。
            // §用户诉求：Claude Code 式重连提示——显示时间与次数（第 N/MAX 次）。
            // 刷屏防护：恢复是过程性事件，只对第一次（attempt == 1）追加提示行，
            // 后续同轮恢复静默（status/footer spinner 仍在运行）；最终失败另有
            // 总结提示（ProviderInterrupted），连续断联不再每次弹一行。
            // §bug 修复：每次恢复都在 footer 显示进度（transient_hint），否则
            // 用户看不到重试在发生（只提示第一次 + 静默，误以为没有自动重试）。
            view.reconnect_count = view.reconnect_count.saturating_add(1);
            view.transient_hint = Some(format!(
                "⟳ 连接中断，自动续写中（第 {attempt}/{max} 次）…",
                max = tpi_agent::agent::MAX_STREAM_RECOVERIES
            ));
            if attempt == 1 {
                view.push_line(
                    LineKind::System,
                    format!(
                        "[{}] ⟳ 模型连接中断，正在自动续写…（第 {attempt}/{} 次）",
                        now_hhmmss(),
                        tpi_agent::agent::MAX_STREAM_RECOVERIES
                    ),
                );
            }
        }
        RuntimeEvent::TurnRestarting { attempt } => {
            // §4.3 第三阶段：partial tool-call 后整个 turn 重新生成——
            // 丢弃已显示的 partial（不进 transcript），提示用户。
            view.discard_live_turn();
            view.reconnect_count = view.reconnect_count.saturating_add(1);
            // 刷屏防护：同 StreamRecovering，只对第一次追加提示行。
            // §bug 修复：每次重启都在 footer 显示进度（同续写）。
            view.transient_hint = Some(format!(
                "⟳ 连接中断，自动重试中（第 {attempt}/{max} 次）…",
                max = tpi_agent::agent::MAX_TURN_RESTARTS
            ));
            if attempt == 1 {
                view.push_line(
                    LineKind::System,
                    format!(
                        "[{}] ⟳ 工具调用中断，正在重新生成该轮回答…（第 {attempt}/{} 次）",
                        now_hhmmss(),
                        tpi_agent::agent::MAX_TURN_RESTARTS
                    ),
                );
            }
        }
        RuntimeEvent::CompactionNotice { message } => {
            // §用户诉求：手动 /compact 结果反馈（成功/未生效）写入系统行。
            view.push_line(LineKind::System, message);
        }
        RuntimeEvent::SubagentReported {
            child_session,
            summary,
            evidence,
        } => {
            // P8-06：子代理调查完成 → summary card（系统行 + 证据列表）。
            let mut text = format!("🔍 子代理调查完成（session {child_session}）：{summary}");
            if !evidence.is_empty() {
                text.push_str("\n  证据: ");
                text.push_str(&evidence.join(", "));
            }
            view.push_line(LineKind::System, text);
        }
    }
}

/// 主入口：状态转换 + 效果（§26：reducer 只修改状态；§成熟化：键位由
/// UiState.keymap 注入（来自 `[ui.keymap]`），不破坏状态可重放性）。
pub fn update(state: &mut UiState, event: UiEvent) -> Vec<UiEffect> {
    match event {
        UiEvent::Tick => {
            state.view.anim_tick = state.view.anim_tick.wrapping_add(1);
            Vec::new()
        }
        UiEvent::Paste(text) => {
            state.view.transient_hint = None;
            // While a Modal/Overlay is open, Paste must not write into
            // the composer behind it (keys are already blocked; Paste is a separate event).
            if state.view.overlay.is_some() || state.view.modal.is_some() {
                return Vec::new();
            }
            let text = crate::paste::normalize_newlines(text);
            if state.view.search.is_some() {
                // BUG-014：搜索打开时粘贴应进入搜索框，而不是 composer。
                let mut query = state
                    .view
                    .search
                    .as_ref()
                    .map(|s| s.query.clone())
                    .unwrap_or_default();
                let room = (4 * 1024usize).saturating_sub(query.len());
                let keep = crate::text::floor_char_boundary(&text, room.min(text.len()));
                query.push_str(&text[..keep]);
                state.view.update_search_query(&query);
            } else if crate::paste::is_large_paste(&text) {
                // §用户诉求：大粘贴不真实渲染——全文存入旁路，输入框只放
                // 占位符；提交时一次性展开（避免分块上屏/MAX_INPUT_BYTES 截断/
                // 行尾 Enter 批次边界误判提交）。
                let placeholder = state.store_paste(text);
                state.editor.insert_str(&placeholder);
                state.sync_input();
                refresh_menus(state);
            } else {
                let truncated = state.editor.text().len().saturating_add(text.len())
                    > crate::editor::MAX_INPUT_BYTES;
                state.editor.insert_str(&text);
                state.sync_input();
                refresh_menus(state);
                if truncated {
                    state.view.transient_hint = Some(format!(
                        "输入已截断到 {} KiB",
                        crate::editor::MAX_INPUT_BYTES / 1024
                    ));
                }
            }
            Vec::new()
        }
        UiEvent::CollapseKeyStreamPaste {
            rendered_suffix,
            text,
        } => {
            if state.view.overlay.is_some()
                || state.view.modal.is_some()
                || state.view.search.is_some()
            {
                return Vec::new();
            }
            let placeholder = crate::paste::next_paste_placeholder(&state.pasted, &text);
            if state
                .editor
                .replace_suffix_before_cursor(&rendered_suffix, &placeholder)
            {
                state.pasted.insert(placeholder, text);
                state.sync_input();
                refresh_menus(state);
            }
            Vec::new()
        }
        UiEvent::FinishKeyStreamPaste {
            initial_text,
            full_text,
        } => {
            if initial_text == full_text {
                return Vec::new();
            }
            if state.view.search.is_some() {
                // 搜索框不展示占位符；把阈值后旁路收集的尾部补回并沿用 4 KiB 上限。
                if let Some(search) = state.view.search.as_ref()
                    && let Some(prefix) = search.query.strip_suffix(&initial_text)
                {
                    let mut query = prefix.to_owned();
                    let room = (4 * 1024usize).saturating_sub(query.len());
                    let keep =
                        crate::text::floor_char_boundary(&full_text, room.min(full_text.len()));
                    query.push_str(&full_text[..keep]);
                    state.view.update_search_query(&query);
                }
                return Vec::new();
            }
            if state.view.overlay.is_some() || state.view.modal.is_some() {
                return Vec::new();
            }
            let before_cursor = &state.editor.text[..state.editor.cursor];
            let matching = state.pasted.iter().find_map(|(placeholder, content)| {
                (content == &initial_text && before_cursor.ends_with(placeholder))
                    .then(|| placeholder.clone())
            });
            if let Some(old_placeholder) = matching {
                state.pasted.remove(&old_placeholder);
                let new_placeholder =
                    crate::paste::next_paste_placeholder(&state.pasted, &full_text);
                if state
                    .editor
                    .replace_suffix_before_cursor(&old_placeholder, &new_placeholder)
                {
                    state.pasted.insert(new_placeholder, full_text);
                    state.sync_input();
                    refresh_menus(state);
                } else {
                    state.pasted.insert(old_placeholder, initial_text);
                }
            }
            Vec::new()
        }
        UiEvent::Key(key) => handle_key(state, key),
        UiEvent::MouseScrollUp => {
            state.view.transient_hint = None;
            if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_sub(5);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_sub(5);
            } else {
                state.view.scroll_up(5);
            }
            Vec::new()
        }
        UiEvent::MouseScrollDown => {
            state.view.transient_hint = None;
            if let Some(modal) = &mut state.view.modal {
                modal.scroll = modal.scroll.saturating_add(5);
            } else if let Some(overlay) = &mut state.view.overlay {
                overlay.scroll = overlay.scroll.saturating_add(5);
            } else {
                state.view.scroll_down(5);
            }
            Vec::new()
        }
        // MouseMoved 保留为穷尽占位（§用户诉求：已移除 hover 悬浮高亮）。
        UiEvent::MouseMoved { .. } => Vec::new(),
        // §InteractionRefactor：语义选择事件由 reducer 直接写入 view（不再
        // 依赖 Renderer 坐标——TextPosition 指向内容）。SelectionEnd 保留选区。
        UiEvent::SelectionStart(position) => {
            state.view.selection_start(position);
            Vec::new()
        }
        UiEvent::SelectionUpdate(position) => {
            state.view.selection_update(position);
            Vec::new()
        }
        UiEvent::SelectionEnd => {
            state.view.selection_end();
            Vec::new()
        }
        // §用户诉求：新按压清除旧选区（点击其他地方取消选中）。
        UiEvent::SelectionClear => {
            state.view.selection_clear();
            Vec::new()
        }
        UiEvent::ClickTool(id) => {
            if state.view.modal.is_some() || state.view.overlay.is_some() {
                return Vec::new(); // 弹层打开时鼠标点击不得打开后台 overlay
            }
            // §子代理：点击 subagent 卡 → 打开内部视图（实时观察 child），
            // 而非普通展开（展开仍可从内部视图内看到全部活动文本流）。
            if state.view.is_subagent_card(&id) {
                state.view.open_subagent(id);
                return Vec::new();
            }
            state.view.toggle_expand(id);
            Vec::new()
        }
        UiEvent::ClickReasoning(id) => {
            if state.view.modal.is_some() || state.view.overlay.is_some() {
                return Vec::new();
            }
            // §修复：历史与 live reasoning 统一按条目切换（与工具卡 toggle_expand
            // 同构）。live 的 entry_id 在 finalize 后沿用，点击始终能再次收起——
            // 此前 live 点击切全局 reasoning_visible、历史点击切按条目，混合后
            // 一旦 reasoning_visible=true 所有历史卡都“展开后关不上”。
            state.view.toggle_reasoning_expanded(id);
            Vec::new()
        }
        UiEvent::ClickLink(url) => {
            // §成熟化：链接文本点击 → 打开 Link Overlay（确认后再打开/复制）。
            // 仅接受 http/https（其他 scheme 提示不可用，不打开）。
            if state.view.modal.is_some() || state.view.overlay.is_some() {
                return Vec::new();
            }
            state.view.overlay = Some(crate::model::OverlayState::for_link(&url));
            Vec::new()
        }
        UiEvent::SidebarJump(entry) => {
            // §用户诉求：点击大纲行 → 锁定到该用户消息；侧边栏保持打开
            //（连续浏览/跳转多段对话；用户可自行 Ctrl+B 关闭）。
            if state.view.modal.is_some()
                || state.view.overlay.is_some()
                || state.view.question.is_some()
            {
                return Vec::new();
            }
            state.view.lock_to(entry, 0);
            Vec::new()
        }
        UiEvent::SidebarScroll(up) => {
            if state.view.sidebar.open {
                state.view.sidebar.scroll_by(if up { -5 } else { 5 });
            }
            Vec::new()
        }
        UiEvent::SidebarScrollbarClick(row) => {
            if state.view.sidebar.open {
                state.view.sidebar.scroll_to_ratio(row as usize);
            }
            Vec::new()
        }
        UiEvent::ToggleSidebar => {
            state.view.toggle_sidebar();
            refresh_menus(state);
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
