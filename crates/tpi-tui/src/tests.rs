//! tui/mod.rs 的渲染测试（从 `mod tests` 内联块迁出；子模块可经 `super::*`
//! 访问父模块私有项，行为与内联等价）。
use super::*;
use crate::event::UiEvent;
use crate::model::LineKind;
use crate::model::ToolCardState;
use crate::reducer;
use crate::state::UiState;
use crate::tool_card::{render_diff_lines, tool_name_style};
use ratatui::crossterm::event::KeyCode;

/// §bug 修复：每次自动恢复（续写/重生成）都在 footer 显示进度——此前只提示
/// 第一次 + 静默，用户误以为没有自动重试。
#[test]
fn auto_recovery_shows_footer_progress_every_attempt() {
    let mut state = UiState::new(ViewModel::default());
    // 第一次续写：footer 提示。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::StreamRecovering { attempt: 1 }),
    );
    let hint1 = state.view.transient_hint.clone();
    assert!(
        hint1.as_deref().is_some_and(|h| {
            h.contains("自动续写")
                && h.contains(&format!("1/{}", tpi_agent::agent::MAX_STREAM_RECOVERIES))
        }),
        "第一次续写 footer 提示: {hint1:?}"
    );
    // 第二次续写：footer 提示更新（不追加新系统行——刷屏防护保留）。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::StreamRecovering { attempt: 2 }),
    );
    let hint2 = state.view.transient_hint.clone();
    assert!(
        hint2
            .as_deref()
            .is_some_and(|h| h.contains(&format!("2/{}", tpi_agent::agent::MAX_STREAM_RECOVERIES))),
        "第二次续写 footer 提示更新: {hint2:?}"
    );
    // TurnRestarting 同理。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::TurnRestarting { attempt: 3 }),
    );
    let hint3 = state.view.transient_hint.clone();
    assert!(
        hint3.as_deref().is_some_and(|h| {
            h.contains("自动重试")
                && h.contains(&format!("3/{}", tpi_agent::agent::MAX_TURN_RESTARTS))
        }),
        "重生成 footer 提示: {hint3:?}"
    );
}

/// §用户诉求：provider 内部重试对用户可见——footer 提示，首次追加系统行。
#[test]
fn provider_retry_shows_feedback() {
    let mut state = UiState::new(ViewModel::default());
    // 第一次重试：footer 提示 + 追加一条系统行（等待时长向下取整到秒）。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::ProviderRetrying {
            attempt: 1,
            backoff_ms: 1500,
        }),
    );
    let hint = state.view.transient_hint.clone();
    assert!(
        hint.as_deref().is_some_and(|h| {
            h.contains("网络重试中") && h.contains("第 1 次") && h.contains("1s")
        }),
        "首次重试 footer 提示: {hint:?}"
    );
    assert_eq!(state.view.transcript.len(), 1, "首次重试追加一条系统行");
    assert!(entry_text(&state.view.transcript[0]).contains("自动重试"));

    // 第二次重试：footer 更新，不追加新系统行（刷屏防护）。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::ProviderRetrying {
            attempt: 3,
            backoff_ms: 2300,
        }),
    );
    let hint2 = state.view.transient_hint.clone();
    assert!(
        hint2.as_deref().is_some_and(|h| {
            h.contains("第 3 次") && h.contains("2s")
        }),
        "后续重试 footer 更新: {hint2:?}"
    );
    assert_eq!(
        state.view.transcript.len(),
        1,
        "同轮后续重试不得追加提示行"
    );
}

/// §bug 修复：idle 时输入 `/quit` 按 Enter 一次即入队（app 主循环立即消费
/// → 退出）——不应需要额外按键。菜单开着（/ 前缀弹出）也不阻塞提交。
#[test]
fn idle_submit_slash_queues_immediately() {
    let mut state = UiState::new(ViewModel::default());
    state.running = false;
    // 模拟输入 /quit（输入以 / 开头 → 斜杠菜单会弹出；用完整输入 + Enter）。
    for c in "/quit".chars() {
        reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char(c))));
    }
    assert!(state.view.menu.is_some(), "/ 前缀菜单弹出");
    let effects = reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Enter)));
    assert_eq!(
        state.pop_pending().as_deref(),
        Some("/quit"),
        "一次 Enter 即入队"
    );
    assert!(effects.is_empty());
}

/// §bug 修复：run 中提交 `/` 命令——reducer 入队（peek 可见），app 层据此
/// 立即取消 run 执行命令（不再排队等 run 结束）。普通消息不触发取消。
#[test]
fn running_slash_submit_is_peekable() {
    let mut state = UiState::new(ViewModel::default());
    state.running = true;
    // run 中提交 /quit。
    for c in "/quit".chars() {
        reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char(c))));
    }
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Enter)));
    assert_eq!(
        state.peek_pending(),
        Some("/quit"),
        "run 中 / 命令入队且队首可见（app 据此取消 run）"
    );
    assert!(state.view.transient_hint.is_some(), "有排队提示");
    // 清空后再提交普通消息：也入队，但队首不是 /（app 不取消 run）。
    state.pop_pending();
    for c in "继续干活".chars() {
        reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char(c))));
    }
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Enter)));
    assert_eq!(
        state.peek_pending(),
        Some("继续干活"),
        "普通消息入队但非 / 开头"
    );
}

/// §bug 修复：队列中任意位置的 / 命令被提升到队首（app 消费时优先执行），
/// 不被前面排队的普通消息阻塞。
#[test]
fn pending_slash_promoted_over_queued_messages() {
    let mut state = UiState::new(ViewModel::default());
    state.push_pending("继续干活".into());
    state.push_pending("/quit".into());
    state.push_pending("第三条".into());
    assert!(state.has_pending_slash(), "队列中存在 / 命令");
    state.promote_pending_slash();
    assert_eq!(state.pop_pending().as_deref(), Some("/quit"), "/ 命令优先");
    assert_eq!(
        state.pop_pending().as_deref(),
        Some("继续干活"),
        "原顺序保持"
    );
    assert_eq!(state.pop_pending().as_deref(), Some("第三条"));
    assert!(!state.has_pending_slash(), "无 / 命令时 false");
}

/// 非抢占型 /命令（model/session/theme/help 等信息/切换型）不打断 run：
/// 用户在 run 中只是想打开菜单看一眼，或让切换在下一轮再生效——不得
/// 因此取消正在进行的 run。只有抢占型命令（quit/new/cancel/compact/retry）
/// 才触发 has_pending_slash 并被提到队首。
#[test]
fn non_preemptive_slash_does_not_trigger_run_cancel() {
    let mut state = UiState::new(ViewModel::default());
    state.push_pending("/model".into());
    state.push_pending("刚才那个任务的后续".into());
    // 信息/切换型命令不得判定为抢占（run 中不应被取消）。
    assert!(
        !state.has_pending_slash(),
        "/model 不允许打断 run（切换在下轮再生效）"
    );
    // promote 也不应把 /model 提前（保持 FIFO 消费即可）。
    state.promote_pending_slash();
    assert_eq!(
        state.pop_pending().as_deref(),
        Some("/model"),
        "无抢占命令时保持原 FIFO 顺序"
    );
}

/// §修复：UsageUpdated 事件实时累加到累计字段（不等 run 结束）——
/// footer 的 ↑↓⇄ 与缓存命中率因此同口径（累计 cache_read / 累计 input），
/// 不再出现“累计值旁挂本次命中率”的矛盾（⇄27.1M(33%)）。
#[test]
fn usage_updated_accumulates_incrementally() {
    let mut state = UiState::new(ViewModel::default());
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::UsageUpdated {
            input_tokens: 1000,
            output_tokens: 200,
            cache_read_tokens: 600,
        }),
    );
    assert_eq!(state.view.input_tokens, 1000);
    assert_eq!(state.view.output_tokens, 200);
    assert_eq!(state.view.cache_read_tokens, 600);
    // 第二次（下一轮请求）继续累加，而不是覆盖。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::UsageUpdated {
            input_tokens: 500,
            output_tokens: 100,
            cache_read_tokens: 300,
        }),
    );
    assert_eq!(state.view.input_tokens, 1500);
    assert_eq!(state.view.output_tokens, 300);
    assert_eq!(state.view.cache_read_tokens, 900);
}

/// §用户诉求：断开重连提示带时间与次数（Claude Code 式）——系统行含
/// `[HH:MM:SS]` 时间戳、`第 N/MAX 次`；footer 的 reconnect_count 累计。
/// §刷屏防护：恢复是过程性事件，同轮后续恢复（attempt > 1）只累计计数、
/// 不再追加提示行（最终失败另有总结提示）——一次 run 内不再弹 N 行恢复提示。
#[test]
fn reconnect_prompt_shows_time_and_attempt_count() {
    let mut state = UiState::new(ViewModel::default());
    // attempt > 1（同轮第 2 次恢复）：不追加行，仅累计 reconnect_count。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::StreamRecovering { attempt: 2 }),
    );
    assert_eq!(state.view.reconnect_count, 1);
    assert!(
        state.view.transcript.is_empty(),
        "同轮后续恢复不得追加提示行（防止断联抖动刷屏）"
    );

    // 第一次恢复（attempt == 1）：追加带时间戳与次数的提示行。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::StreamRecovering { attempt: 1 }),
    );
    assert_eq!(state.view.reconnect_count, 2);
    let text = entry_text(&state.view.transcript[0]);
    // 时间戳 [HH:MM:SS]（Claude Code 式）。
    assert!(
        text.starts_with('[') && text.contains(':') && text.contains("] ⟳"),
        "系统行必须带 [HH:MM:SS] 时间戳: {text}"
    );
    assert!(
        text.contains(&format!(
            "第 1/{} 次",
            tpi_agent::agent::MAX_STREAM_RECOVERIES
        )),
        "必须显示第 N/MAX 次（MAX=MAX_STREAM_RECOVERIES）: {text}"
    );

    // TurnRestarting 第一次（attempt == 1）：同样追加。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::TurnRestarting { attempt: 1 }),
    );
    assert_eq!(state.view.reconnect_count, 3);
    let text2 = entry_text(&state.view.transcript[1]);
    assert!(
        text2.contains(&format!("第 1/{} 次", tpi_agent::agent::MAX_TURN_RESTARTS)),
        "{text2}"
    );
    assert!(text2.contains("重新生成"), "{text2}");
    // 同轮后续 TurnRestarting（attempt > 1）：静默。
    reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::TurnRestarting { attempt: 3 }),
    );
    assert_eq!(state.view.reconnect_count, 4);
    assert_eq!(state.view.transcript.len(), 2, "同轮后续重启不得追加提示行");
}

/// 取 Entry 的文本（Message 行）；测试辅助。
fn entry_text(entry: &crate::model::Entry) -> String {
    match entry {
        crate::model::Entry::Message { line, .. } => line.text.clone(),
        crate::model::Entry::Tool { .. } => String::new(),
    }
}

#[test]
fn plan_window_follows_tail_and_commits_overflow() {
    let mut view = ViewModel::default();
    for i in 0..10 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    // §美化：每条 Assistant 消息后插 1 空行 → 10 条 = 20 行。
    // 跟随模式：窗口是最后 4 行，前 16 行提交到 scrollback。
    assert_eq!(plan.window.len(), 4);
    assert_eq!(plan.overflow.len(), 16);
    assert_eq!(plan.committed_after, 16);
    let window_text: String = plan
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(window_text.contains("line 9"), "窗口应包含最新行");
    assert!(!window_text.contains("line 0"), "窗口不应包含已提交行");

    // 第二次调用：无新 overflow。
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        4,
        plan.committed_after,
        false,
        &mut cache,
    );
    assert!(plan2.overflow.is_empty());
    assert_eq!(plan2.committed_after, 16);
}

#[test]
fn plan_window_freezes_commits_when_scrolled() {
    let mut view = ViewModel::default();
    for i in 0..10 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    let mut cache = HashMap::new();
    // 先布局一次（Follow 建立 layout_top = 视口顶部行）。
    let _ = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    // §美化：10 条消息 = 20 行；Follow 顶部行 = 16。
    // TUI v2：翻页 = 锚点（视口顶部）上移 2 行 → 窗口 [14, 18)。
    view.scroll_up(2);
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    // Locked：不提交到 scrollback。
    assert_eq!(plan.window.len(), 4);
    assert!(plan.overflow.is_empty());
    assert_eq!(plan.committed_after, 0);
    let window_text: String = plan
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    // 行 14 = m7，行 16 = m8（m_i 在行 2i，空行在 2i+1）。
    assert!(window_text.contains("line 7") && window_text.contains("line 8"));
    assert!(!window_text.contains("line 9"));
    // Locked 保持：新内容不移动视口（§58 场景 A）。
    view.push_line(LineKind::Assistant, "line 10 new".to_string());
    let plan3 = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    let window_text: String = plan3
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        !window_text.contains("line 10 new"),
        "Locked 时新输出不得移动视口"
    );
    assert!(view.pending_below >= 1, "Locked 时新内容必须计数");
}

/// §视觉瘦身：工具卡片只有主行可点击（内容行留给文本选择，避免与拖选冲突）。
#[test]
fn tool_card_only_header_clickable() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Failed,
        10,
        Some(1),
        "第一行\n第二行\n第三行\n第四行",
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    // §美化：整卡可点击——主行与内容行都带 Tool hit（轻点任意行展开；
    // §美化：整卡可点击——主行与内容行都带 Tool hit；卡片后的留白
    // 空行（间隔）不可点。row_hits 与 window 等长。
    assert!(plan.window.len() >= 2, "卡片含主行+正文: {:?}", plan.window);
    assert!(
        matches!(&plan.row_hits[0], Some((HitTarget::Tool(id), end)) if id == "c1" && *end > 0),
        "主行必须可点击: {:?}",
        plan.row_hits[0]
    );
    for (i, (line, hit)) in plan.window.iter().zip(plan.row_hits.iter()).enumerate() {
        if line.spans.is_empty() {
            // 留白空行（卡片间间隔）。
            continue;
        }
        assert!(
            matches!(hit, Some((HitTarget::Tool(id), end)) if id == "c1" && *end > 0),
            "整卡每行（第 {i} 行）都可点击展开: {hit:?}"
        );
    }
}

/// 侧边栏 Todo 按原计划顺序摆放（完成项不沉底、当前项不提前）——
/// 按提交顺序排列更能直观看到进度。
#[test]
fn sidebar_plan_keeps_original_item_order() {
    use tpi_core::plan::{Plan, PlanItem, PlanStatus};
    let plan = Plan {
        explanation: None,
        items: vec![
            PlanItem {
                text: "old1".into(),
                status: PlanStatus::Completed,
            },
            PlanItem {
                text: "old2".into(),
                status: PlanStatus::Completed,
            },
            PlanItem {
                text: "new1".into(),
                status: PlanStatus::InProgress,
            },
            PlanItem {
                text: "new2".into(),
                status: PlanStatus::Pending,
            },
            PlanItem {
                text: "new3".into(),
                status: PlanStatus::Pending,
            },
        ],
    };
    let shown = sidebar_plan_items(&plan);
    let texts: Vec<&str> = shown.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["old1", "old2", "new1", "new2", "new3"],
        "按原计划顺序摆放，完成项不沉底"
    );
    let plan2 = Plan {
        explanation: None,
        items: vec![
            PlanItem {
                text: "old1".into(),
                status: PlanStatus::Completed,
            },
            PlanItem {
                text: "new1".into(),
                status: PlanStatus::InProgress,
            },
        ],
    };
    let texts2: Vec<&str> = sidebar_plan_items(&plan2)
        .iter()
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(texts2, vec!["old1", "new1"], "原顺序：完成后项仍留在原位置");
    // 全部完成：侧边栏仍显示全部已保留的完成历史。
    let plan3 = Plan {
        explanation: None,
        items: vec![
            PlanItem {
                text: "a".into(),
                status: PlanStatus::Completed,
            },
            PlanItem {
                text: "b".into(),
                status: PlanStatus::Completed,
            },
            PlanItem {
                text: "c".into(),
                status: PlanStatus::Completed,
            },
        ],
    };
    assert_eq!(sidebar_plan_items(&plan3).len(), 3);
}

#[test]
fn plan_window_resize_resets_committed_without_overflow() {
    let mut view = ViewModel::default();
    for i in 0..10 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    let mut view = ViewModel::default();
    for i in 0..10 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    let mut cache = HashMap::new();
    // §美化：10 条消息 = 20 行；先提交 16 行（含每条消息后的留白空行）。
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    assert_eq!(plan.committed_after, 16);
    // resize：不产生 overflow，提交位置重置为窗口起点。
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        40,
        4,
        plan.committed_after,
        true,
        &mut cache,
    );
    assert!(plan2.overflow.is_empty());
    // 40 宽度下每行不折行（"line N" 很短），窗口仍是最后 4 行。
    assert_eq!(plan2.committed_after, 16);
}

/// §58 回归：Follow → PgUp（进 Locked）→ PgDn 到底 → 新内容 → 再 PgDn 必须能滚到底。
/// 防止「滚动无反应」（scroll_down 在 Follow 直接 return + Locked 到底后不自动跟随）。
#[test]
fn scroll_up_then_down_with_new_content_is_not_stuck() {
    let mut view = ViewModel::default();
    for i in 0..8 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    let mut cache = HashMap::new();
    // 布局：Follow，视口 4 行 → 顶部行 = 4（显示 line4-7）。
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    assert_eq!(plan.window.len(), 4);
    let window_text: String = plan
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        window_text.contains("line 7"),
        "Follow 显示尾部: {window_text}"
    );

    // PgUp 两次（每次 8 行，实际 clamp 到顶）：应进入 Locked 并锚定顶部。
    view.scroll_up(8);
    view.scroll_up(8);
    assert!(
        matches!(view.scroll_mode, ScrollMode::Locked(_)),
        "PgUp 必须进入 Locked"
    );
    let plan_top = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    let window_text: String = plan_top
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        window_text.contains("line 0"),
        "PgUp 后应显示顶部: {window_text}"
    );

    // 新内容到达（模拟 run 中）→ 仍 Locked（视口不动）。
    view.push_line(LineKind::Assistant, "line 8 new".to_string());
    let plan_new = plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    let window_text: String = plan_new
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        !window_text.contains("line 8 new"),
        "Locked 时新内容不移动视口"
    );

    // PgDn 回到底部（多次）：最终应能看到新内容（可滚回最新）。
    for _ in 0..10 {
        view.scroll_down(8);
    }
    let plan_bottom =
        plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    let window_text: String = plan_bottom
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        window_text.contains("line 8 new"),
        "PgDn 到底后应能看到新内容（不得卡住）: {window_text}"
    );
    // 滚到底自动回 Follow：新内容继续自动跟随（§10 体验修复）。
    assert_eq!(
        view.scroll_mode,
        ScrollMode::Follow,
        "滚到底后必须自动回到 Follow（新内容自动跟随，不再卡住）"
    );
    view.push_line(LineKind::Assistant, "line 9 newest".to_string());
    let plan_newest =
        plan_window_simple(&mut view, theme::Theme::omp(), 80, 4, 0, false, &mut cache);
    let window_text: String = plan_newest
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        window_text.contains("line 9 newest"),
        "回 Follow 后新内容自动跟随: {window_text}"
    );
}

#[test]
fn markdown_bold_code_and_code_block_render_styled() {
    let theme = theme::Theme::omp();
    let lines = render_markdown("**加粗** 和 `code`", theme, None);
    assert_eq!(lines.len(), 1);
    let spans = &lines[0].spans;
    assert!(
        spans
            .iter()
            .any(|s| s.content == "加粗" && s.style.add_modifier.contains(Modifier::BOLD)),
        "加粗必须带 BOLD 修饰: {spans:?}"
    );
    assert!(
        spans
            .iter()
            .any(|s| s.content == "code" && s.style.fg == Some(theme.primary)),
        "行内代码必须使用 primary 色: {spans:?}"
    );

    let lines = render_markdown("```rust\nfn main() {}\n```", theme, None);
    assert_eq!(lines.len(), 1, "代码块渲染为一行");
    // §成熟化：rust 代码块经 syntect 高亮，行被 tokenize 为多个 span；
    // 断言整行文本（而非 spans[0]，后者是首 token）。
    let line_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(line_text, "fn main() {}");
    // 语法高亮生效：至少一个 token 使用语义色（keyword 等），非纯 muted。
    let has_semantic = lines[0]
        .spans
        .iter()
        .any(|s| s.style.fg.is_some() && s.style.fg != Some(theme.muted));
    assert!(
        has_semantic,
        "rust 代码块必须有语法高亮: {:?}",
        lines[0].spans
    );
}

#[test]
fn markdown_list_and_link_render() {
    let theme = theme::Theme::omp();
    let lines = render_markdown(
        "- 第一项\n- 第二项\n\n[链接](https://example.com)",
        theme,
        None,
    );
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("• 第一项") && text.contains("• 第二项"));
    assert!(
        text.contains("链接 (https://example.com)"),
        "链接应附 URL: {text:?}"
    );
}

/// §16.2 增强：markdown 标题分级渲染（h1-h3 分色 + BOLD，h4+ 归一）。
#[test]
fn markdown_headings_render_with_level_styles() {
    let theme = theme::Theme::omp();
    let lines = render_markdown(
        "# 大标题\n\n## 副标题\n\n### 小节\n\n#### 四级\n",
        theme,
        None,
    );
    assert_eq!(lines.len(), 4, "四个标题各一行: {lines:?}");
    // h1 → primary + BOLD。
    let h1 = &lines[0];
    assert!(h1.spans.iter().all(|s| s.content == "大标题"));
    let h1_style = &h1.spans[0].style;
    assert_eq!(h1_style.fg, Some(theme.primary));
    assert!(
        h1_style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    // h2 → accent + BOLD。
    let h2 = &lines[1];
    assert_eq!(h2.spans[0].style.fg, Some(theme.accent));
    assert!(
        h2.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    // h4 → text + BOLD（归一）。
    let h4 = &lines[3];
    assert_eq!(h4.spans[0].style.fg, Some(theme.text));
    assert!(
        h4.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

/// §16.2 增强：markdown 表格渲染——带边框、表头加粗、列宽对齐。
#[test]
fn markdown_table_renders_with_borders_and_header() {
    let theme = theme::Theme::omp();
    let md = "| 名称 | 数量 |\n| --- | ---: |\n| 苹果 | 3 |\n| 香蕉 | 10 |";
    let lines = render_markdown(md, theme, None);
    // 期望：顶边框 + 表头 + 分隔 + 2 数据行 + 底边框 = 6 行。
    assert!(lines.len() >= 6, "表格应渲染多行: {lines:?}");
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("名称") && text.contains("数量"),
        "表头可见: {text}"
    );
    assert!(
        text.contains("苹果") && text.contains("香蕉"),
        "数据行可见: {text}"
    );
    assert!(text.contains('│'), "带竖边框: {text}");
    // 表头行加粗。
    let header_line = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content == "名称"))
        .expect("找到表头行");
    assert!(
        header_line.spans.iter().any(|s| s
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)),
        "表头必须加粗"
    );
}

/// §codex 移植：超宽表格在窄 width 下 cell 内换行，且**每个子行分隔符都在**。
/// 修复旧实现「列分隔符被通用 wrapper 切碎」的缺陷。
#[test]
fn markdown_table_wraps_cells_within_width() {
    let theme = theme::Theme::omp();
    // 超宽 cell：第 2 列内容很长，窄 width 下必须 cell 内换行。
    let md = "| # | 现象 |\n| --- | --- |\n| 1 | 这是很长的一段描述，内容远超单列宽度，必须换行 |";
    let lines = render_markdown(md, theme, Some(24));
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    // 每行宽度 ≤ 24（未被通用 wrapper 切碎）。
    for line in &lines {
        let w: usize = line
            .spans
            .iter()
            .map(|s| crate::text::display_width(s.content.as_ref()))
            .sum();
        assert!(w <= 24, "表格行不得超宽: '{text}' w={w}");
    }
    // 换行后仍有多个含 │ 的行（每行分隔符都在）。
    let rows_with_border = lines
        .iter()
        .filter(|l| l.spans.iter().any(|s| s.content == "│"))
        .count();
    assert!(rows_with_border >= 3, "换行后每行都应有分隔符: {text}");
    // 内容完整保留（不丢字）：按行检查关键片段。
    let row_texts: Vec<String> = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect();
    assert!(
        row_texts.iter().any(|r| r.contains("这是很长的一段")),
        "cell 首行片段保留: {text}"
    );
    assert!(
        row_texts.iter().any(|r| r.contains("换行")),
        "cell 末行片段保留（不丢字）: {text}"
    );
}

/// §codex 移植：极窄终端下表格退化为 records（label: value，逐行），
/// 不产生 0 宽/不可读的输出。
#[test]
fn markdown_table_degrades_to_records_on_narrow_width() {
    let theme = theme::Theme::omp();
    let md = "| 名称 | 值 |\n| --- | --- |\n| alpha | 42 |";
    let lines = render_markdown(md, theme, Some(8));
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    // 窄屏：records 模式（label: value），仍保留内容。
    assert!(
        text.contains("名称") && text.contains("42"),
        "records 模式仍应保留内容: {text}"
    );
    for line in &lines {
        let width: usize = line
            .spans
            .iter()
            .map(|span| crate::text::display_width(span.content.as_ref()))
            .sum();
        assert!(width <= 8, "records 行不得超宽: {line:?}");
    }
}

/// §用户诉求（行间横线）：box 表格表头下**和表体每行之间**都有 ├┼┤ 分隔线
/// ——数据行之间必须能看出行边界（此前只画表头下一条，多行表体直接相邻）。
#[test]
fn markdown_table_uses_compact_box_with_body_row_separators() {
    let theme = theme::Theme::omp();
    let md = "| name | value |\n| --- | --- |\n| a | 1 |\n| b | 2 |";
    let lines = render_markdown(md, theme, Some(40));
    let rendered: Vec<String> = lines.iter().map(Line::to_string).collect();
    assert!(rendered.first().is_some_and(|line| line.starts_with('┌')));
    assert!(rendered.last().is_some_and(|line| line.starts_with('└')));
    // 2 行表体：表头下 1 条 + 行间 1 条 = 2 条 ├┼┤。
    assert_eq!(
        rendered.iter().filter(|line| line.starts_with('├')).count(),
        2,
        "表头下与表体行间都应有分隔线: {rendered:?}"
    );
}

/// §用户诉求：unified diff 渲染为用户友好形式——文件头隐藏、hunk 头变
/// 分隔行、内容行带真实行号；`+` 绿、`-` 红（只改前景色）。
#[test]
fn diff_lines_render_with_add_remove_colors() {
    let theme = theme::Theme::omp();
    let diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n }";
    let lines = render_diff_lines(diff, theme);
    // 文件头（---/+++）隐藏；hunk 头 → 1 行分隔；内容 4 行 → 共 5 行。
    assert_eq!(lines.len(), 5, "文件头隐藏、@@ 变分隔行: {lines:?}");
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        !text.contains("--- a/") && !text.contains("+++ b/"),
        "文件头必须隐藏: {text:?}"
    );
    assert!(
        text.contains("…"),
        "@@ 渲染为分隔行（统一省略号）: {text:?}"
    );
    // - 行：行号 + 红字（span 级 error fg），无背景。
    let minus = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1")))
        .expect("找到 - 行");
    assert!(
        minus.spans.iter().any(|s| s.style.fg == Some(theme.error)),
        "- 行红字: {minus:?}"
    );
    assert!(
        minus.spans.iter().all(|s| s.style.bg.is_none()),
        "diff 行不带背景（只改前景色）"
    );
    // + 行：行号 + 绿字（span 级 success fg），无背景。
    let plus = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 2")))
        .expect("找到 + 行");
    assert!(
        plus.spans.iter().any(|s| s.style.fg == Some(theme.success)),
        "+ 行绿字: {plus:?}"
    );
    assert!(
        plus.spans.iter().all(|s| s.style.bg.is_none()),
        "diff 行不带背景（只改前景色）"
    );
    // 行号：- 用旧行号 1，+ 用新行号 2。
    let minus_no = minus
        .spans
        .iter()
        .find(|s| s.content.trim().chars().all(|c| c.is_ascii_digit()))
        .expect("- 行有行号 span");
    assert_eq!(minus_no.content.trim(), "1", "- 行显示旧行号");
    let plus_no = plus
        .spans
        .iter()
        .find(|s| s.content.trim().chars().all(|c| c.is_ascii_digit()))
        .expect("+ 行有行号 span");
    assert_eq!(plus_no.content.trim(), "2", "+ 行显示新行号");
}

/// §用户诉求：edit/write 卡片**未展开**时 diff 也必须显示（默认可见）。
#[test]
fn tool_card_shows_diff_without_expanding() {
    let theme = theme::Theme::omp();
    let card = ToolCard {
            id: "c1".into(),
            name: "edit".into(),
            target: Some("src/lib.rs".into()),
            command: None,
            state: ToolCardState::Done {
                status: tpi_core::outcome::ToolStatus::Succeeded,
                duration_ms: 10,
                exit_code: Some(0),
            },
            output: Some("status: succeeded\ntool: edit\npath: src/lib.rs\n".into()),
            diff: Some(
                "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-    let x = 1;\n+    let x = 2;\n".into(),
            ),
            output_truncated: false,
            expanded: false, // 未展开——diff 仍应显示
            line_number_start: None,
                    collapsed_lines: 10,
                    started_at_ms: None,
            tail: None,
        };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("let x = 1") && text.contains("let x = 2"),
        "未展开时 diff 必须显示: {text:?}"
    );
    // diff 行只改前景色（红/绿字）；红绿背景不出现（面板底统一由卡片承担，
    // Line 级 style 无 bg）。
    let minus = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1")))
        .expect("找到 - 行");
    assert!(
        minus.spans.iter().any(|s| s.style.fg == Some(theme.error)),
        "删除行红字: {minus:?}"
    );
    assert_eq!(minus.style.bg, None, "diff 行不带红底（Line 级）");
    let plus = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 2")))
        .expect("找到 + 行");
    assert!(
        plus.spans.iter().any(|s| s.style.fg == Some(theme.success)),
        "新增行绿字: {plus:?}"
    );
    assert_eq!(plus.style.bg, None, "diff 行不带绿底（Line 级）");
}

/// §用户诉求：diff 自动展开但限长——未展开时只显示前 N 行 + 折叠提示，
/// 展开时显示全部。
#[test]
fn tool_card_diff_limits_length_when_collapsed() {
    let theme = theme::Theme::omp();
    // 30 行 diff（带 hunk 头，模拟 unified_diff 真实格式）。
    let mut diff = "@@ -1,30 +1,30 @@\n".to_string();
    for i in 0..30 {
        diff.push_str(&format!("+line {i}\n"));
    }
    let mk = |expanded: bool| ToolCard {
        id: "c1".into(),
        name: "edit".into(),
        target: None,
        command: None,
        state: ToolCardState::Done {
            status: tpi_core::outcome::ToolStatus::Succeeded,
            duration_ms: 10,
            exit_code: Some(0),
        },
        output: None,
        diff: Some(diff.clone()),
        output_truncated: false,
        expanded,
        line_number_start: None,
        collapsed_lines: 10,
        started_at_ms: None,
        tail: None,
    };
    // 未展开：只显示 12 行 + 提示。
    let collapsed = tool_card_lines(&mk(false), 0, theme, 100);
    let diff_line_count = collapsed
        .iter()
        .filter(|l| l.spans.iter().any(|s| s.content.contains("line ")))
        .count();
    assert!(
        diff_line_count <= 12,
        "折叠态 diff 最多 12 行: {diff_line_count}"
    );
    let text: String = collapsed
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("点击展开"), "折叠态显示展开提示: {text}");
    // §修复：折叠提示行每个 span 烙 panel 底（wrap 只保留 span 级 bg，
    // Line 级会丢——提示文字不得落到终端底色）。
    let hint_line = collapsed
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("点击展开")))
        .expect("找到提示行");
    assert!(
        hint_line
            .spans
            .iter()
            .filter(|s| !s.content.is_empty())
            .all(|s| s.style.bg == Some(theme.panel)),
        "提示行每段都带 panel 底: {:?}",
        hint_line
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style.bg))
            .collect::<Vec<_>>()
    );
    // 展开：显示全部 30 行。
    let expanded = tool_card_lines(&mk(true), 0, theme, 100);
    let expanded_count = expanded
        .iter()
        .filter(|l| l.spans.iter().any(|s| s.content.contains("line ")))
        .count();
    assert_eq!(expanded_count, 30, "展开态显示全部 diff");
}

#[test]
fn tool_card_running_shows_spinner_and_done_shows_status() {
    let theme = theme::Theme::omp();
    let card = ToolCard {
        id: "c1".into(),
        name: "bash".into(),
        target: Some("bash: cargo test".into()),
        command: None,
        state: ToolCardState::Running,
        output: None,
        diff: None,
        output_truncated: false,
        expanded: false,
        line_number_start: None,
        collapsed_lines: 10,
        started_at_ms: None,
        tail: None,
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    assert!(
        lines[0].spans[0].content.starts_with('⠋'),
        "运行中显示 spinner"
    );
    assert!(lines[0].spans.iter().any(|s| s.content == "bash"));
    // 运行中已展示命令摘要（独立缩进行）。
    assert!(
        lines
            .iter()
            .any(|l| l.to_string().contains("bash: cargo test")),
        "运行中显示命令摘要: {lines:?}"
    );

    let card = ToolCard {
        id: "c1".into(),
        name: "bash".into(),
        target: Some("bash: cargo test".into()),
        command: None,
        state: ToolCardState::Done {
            status: tpi_core::outcome::ToolStatus::Failed,
            duration_ms: 1234,
            exit_code: Some(2),
        },
        output: None,
        diff: None,
        output_truncated: false,
        expanded: false,
        line_number_start: None,
        collapsed_lines: 10,
        started_at_ms: None,
        tail: Some("exit_code: 1".into()),
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains('✗'), "失败卡片显示 ✗: {text:?}");
    assert!(text.contains("1.2s"), "耗时格式化: {text:?}");
    assert!(text.contains("exit 2"), "exit code 展示: {text:?}");
    assert!(text.contains("exit_code: 1"), "失败 tail 保留: {text:?}");
}

#[test]
fn input_cursor_cell_tracks_wrapped_lines() {
    // 短输入：第 0 行，列 = 2（prompt）+ 宽度（"你好"=4 cell，字节 6）。
    assert_eq!(input_cursor_cell("你好", 6, 20), (0, 6));
    // 20 个 'a' 在宽度 12（预算 10）下折成两行；光标在末尾 → 第 1 行末尾。
    let s = "a".repeat(20);
    assert_eq!(input_cursor_cell(&s, 20, 12), (1, 10));
    // 光标在第 10 个字符后 → 第 0 行末尾（折行边界；列含 prompt = 2+10）。
    assert_eq!(input_cursor_cell(&s, 10, 12), (0, 12));
    // 中间位置 → 第 1 行第 5 列。
    assert_eq!(input_cursor_cell(&s, 15, 12), (1, 5));
}

#[test]
fn duration_and_token_formatting() {
    assert_eq!(fmt_duration(42), "42ms");
    assert_eq!(fmt_duration(1234), "1.2s");
    assert_eq!(fmt_duration(125_000), "2m05s");
    assert_eq!(fmt_tokens(999), "999");
    assert_eq!(fmt_tokens(12_345), "12.3k");
    assert_eq!(fmt_tokens(2_000_000), "2.0M");
}

/// BUG-008：长输入（折行数 > 输入区高度）时光标行必须留在输入区内。
#[test]
fn input_scroll_keeps_cursor_inside_visible_area() {
    // 10 行输入、4 行区域：光标在最后一行（row 9）→ 滚动 6 行，
    // 光标 y 落在区域第 4 行（area.y + 3），而不是区域外。
    let offset = input_scroll_offset(9, 10, 4);
    assert_eq!(offset, 6);
    assert!(9 - offset < 4, "光标必须落在 4 行区域内");
    // 光标在顶部时不滚动。
    assert_eq!(input_scroll_offset(0, 10, 4), 0);
    assert_eq!(input_scroll_offset(2, 10, 4), 0);
    // 输入行数小于区域高度：不滚动。
    assert_eq!(input_scroll_offset(2, 3, 8), 0);
    // 极端：区域高度 0 不 panic（clamp 到 1）。
    assert_eq!(input_scroll_offset(5, 10, 0), 5);
}

/// PM：Modal/Overlay 尺寸在窄屏/小终端下不溢出（此前 max(40)/max(10) 会超屏）。
#[test]
fn modal_size_clamps_to_terminal() {
    // 常规：优先 88-4=84，但受 width-2 限制。
    assert_eq!(modal_width(80), 76);
    assert_eq!(modal_width(120), 84);
    // 窄屏：不超过 width-2。
    assert_eq!(modal_width(30), 26);
    assert_eq!(modal_width(20), 16);
    assert_eq!(modal_width(10), 8);
    assert!(modal_width(40) <= 38);
    // 高度：不超过 trans_area.height-2。
    assert_eq!(modal_height(24), 22);
    assert_eq!(modal_height(10), 8);
    assert_eq!(modal_height(40), 26);
}
#[test]
fn activity_height_scales_with_terminal_rows() {
    // 小终端：2/5 屏不足下限 → 12 行。
    assert_eq!(activity_height(24), 12);
    // 常规终端：2/5 屏。
    assert_eq!(activity_height(50), 20);
    assert_eq!(activity_height(80), 32);
    // 大终端：不再被 32 上限卡住，自动拓展（上限 rows-12）。
    assert_eq!(activity_height(100), 40);
    assert_eq!(activity_height(120), 48);
    // 极端小终端：保底 12（不 panic）。
    assert_eq!(activity_height(12), 12);
    assert_eq!(activity_height(15), 12);
}

#[test]
fn spinner_frames_advance_with_anim_tick() {
    let theme = theme::Theme::omp();
    let card = ToolCard {
        id: "c".into(),
        name: "bash".into(),
        target: None,
        command: None,
        state: ToolCardState::Running,
        output: None,
        diff: None,
        output_truncated: false,
        expanded: false,
        line_number_start: None,
        collapsed_lines: 10,
        started_at_ms: None,
        tail: None,
    };
    let f0 = tool_card_lines(&card, 0, theme, 100).remove(0);
    let f1 = tool_card_lines(&card, 1, theme, 100).remove(0);
    assert_ne!(
        f0.spans[0].content, f1.spans[0].content,
        "动画帧应随 tick 变化"
    );
}

#[test]
fn running_card_shows_live_output_tail() {
    let theme = theme::Theme::omp();
    let card = ToolCard {
        id: "c".into(),
        name: "bash".into(),
        target: Some("bash: cargo test".into()),
        command: None,
        state: ToolCardState::Running,
        output: Some(
            "progress 1\nprogress 2\nprogress 3\nprogress 4\nprogress 5\nprogress 6\nprogress 7\n"
                .into(),
        ),
        diff: None,
        output_truncated: false,
        expanded: false,
        line_number_start: None,
        collapsed_lines: 10,
        started_at_ms: None,
        tail: None,
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    // §统一折叠（opencode 式）：运行中显示输出开头（前 10 行），不足 10 行全显。
    assert!(text.contains("progress 1"), "运行中显示输出开头: {text}");
    assert!(
        text.contains("progress 7"),
        "全部 7 行可见（<10 行不折叠）: {text}"
    );
    assert!(!text.contains("点击展开"), "7 行 < 10 行不显示折叠提示");
}

#[test]
fn expanded_card_shows_full_output() {
    let theme = theme::Theme::omp();
    let card = ToolCard {
        id: "c".into(),
        name: "bash".into(),
        target: None,
        command: None,
        state: ToolCardState::Done {
            status: tpi_core::outcome::ToolStatus::Succeeded,
            duration_ms: 10,
            exit_code: Some(0),
        },
        output: Some("第一行\n第二行\n第三行\n".into()),
        diff: None,
        output_truncated: false,
        expanded: true,
        line_number_start: None,
        collapsed_lines: 10,
        started_at_ms: None,
        tail: None,
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("第一行") && text.contains("第三行"),
        "展开显示全部输出: {text}"
    );
    assert!(text.contains("✓"), "成功状态图标");
}

/// §用户诉求：read 卡片正文显示真实文件行号（从 line_number_start 递增）。
#[test]
fn read_card_shows_real_line_numbers() {
    let theme = theme::Theme::omp();
    let card = ToolCard {
        id: "c".into(),
        name: "read".into(),
        target: Some("src/lib.rs".into()),
        command: None,
        state: ToolCardState::Done {
            status: tpi_core::outcome::ToolStatus::Succeeded,
            duration_ms: 5,
            exit_code: Some(0),
        },
        output: Some("fn a() {}\nfn b() {}\n".into()),
        diff: None,
        output_truncated: false,
        expanded: true,
        line_number_start: Some(201),
        collapsed_lines: 10,
        started_at_ms: None,
        tail: None,
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("201 │") && text.contains("202 │"),
        "必须显示递增的真实行号: {text:?}"
    );
    assert!(
        text.contains("fn a() {}") && text.contains("fn b() {}"),
        "正文保留: {text:?}"
    );
}

/// 修复：空输入时光标必须停在 prompt 右侧（此前 wrap 为空 → (0,0)，光标跑到 ❯ 左边）。
#[test]
fn input_cursor_empty_input_sits_right_of_prompt() {
    assert_eq!(input_cursor_cell("", 0, 80), (0, 2));
    assert_eq!(input_cursor_cell("", 0, 10), (0, 2));
    assert_eq!(input_cursor_cell("", 0, 1), (0, 2));
}

/// §22 回归：多行粘贴（含换行）时光标定位正确——不漂移、不跑出输入区。
#[test]
fn multiline_input_cursor_stays_inside_lines() {
    // 粘贴 3 行文本；光标在末尾。
    let pasted = "第一行\n第二行\n第三行";
    let (row, col) = input_cursor_cell(pasted, pasted.len(), 40);
    assert_eq!(
        row, 2,
        "光标应落在最后一行（第 3 行, index 2）: ({row},{col})"
    );
    assert_eq!(
        col, 6,
        "第 3 行内容宽度 6（无 prompt，仅首行有）: ({row},{col})"
    );

    // 粘贴 3 行 + 窄宽度（每行折行）：光标仍应在可视行内。
    let narrow = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc"; // 每行 10 字符
    let (row, col) = input_cursor_cell(narrow, narrow.len(), 8); // 预算 6
    assert!(row >= 2, "窄屏多行后光标行必须在末尾附近: ({row},{col})");

    // 光标在中间行：第 2 行末尾。
    let idx = "第一行\n第二行".len();
    let (row, _) = input_cursor_cell(pasted, idx, 40);
    assert_eq!(row, 1, "光标在第 2 行: ({row})");
}

/// §22：input_area_rows 对多行输入返回多行（≤8），空输入 1 行。
#[test]
fn multiline_input_area_grows() {
    let mut view = ViewModel::default();
    assert_eq!(input_area_rows(&view, 80), 1, "空输入 1 行");
    view.input = "第一行\n第二行\n第三行".into();
    assert_eq!(input_area_rows(&view, 80), 3, "3 行输入 → 3 行区域");
    // 超 8 行 → clamp 8（内部滚动）。
    view.input = (0..10)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(input_area_rows(&view, 80), 8, "超 8 行 clamp 到 8");
}

#[test]
fn input_area_uses_full_width_after_prompt_line() {
    let view = ViewModel {
        input: "abcdefghijklmn".into(),
        ..Default::default()
    };
    // width=8：首行 6 格，续行 8 格，恰好两行。旧实现错误地让续行也
    // 只用 6 格，因此计算成三行并造成输入区跳动。
    assert_eq!(input_area_rows(&view, 8), 2);
}

/// 修复：Overlay 必须先 Clear 覆盖区，否则底层 transcript 文字透出（背景干扰）。
#[test]
fn overlay_clears_background_before_rendering() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::Assistant, "背景文字X".repeat(4));
    }
    view.begin_tool("c", "bash", Some("cmd".into()), None);
    view.finish_tool(
        ("c", "bash"),
        tpi_core::outcome::ToolStatus::Failed,
        1,
        Some(1),
        "err",
        None,
    );
    view.open_tool_overlay(String::from("c"));
    let buf = draw_to_test_backend(&mut view, 80, 24);
    // §美化：footer 上方加了分隔线 → trans_area 高度 = 24-1(input)-1(rule)-1(footer)=21，
    // overlay 居中 (2,1,76,19)，底边框在行 19。内部未覆盖区域取行 18
    //（边框上方内部行）→ 必须被清空为空格。
    assert_eq!(
        buf[(40, 18)].symbol(),
        " ",
        "Overlay 内部未覆盖区域必须清空，不得透出背景文字"
    );
}

/// 用户报告：长系统提示本应折成两行，却打印了两行重复内容。
/// 用窄视口渲染一条超长 System 行，断言屏幕行内容互不相同且无重复。
///
/// 注意：TestBackend 中 CJK 双宽字符占用两个 cell（第二个为空白占位），
/// 逐 cell 拼接会混入空格——本测试用**纯 ASCII** 长文本触发同一 bug
///（单 span 长行 → rail 误取整个 span），避免表示层干扰。
#[test]
fn long_system_line_wraps_without_duplication() {
    let mut view = ViewModel::default();
    // 每个 token 唯一，便于检测重复。
    let long_text: String = (0..40)
        .map(|i| format!("word{i} "))
        .collect::<String>()
        .repeat(2);
    view.push_line(LineKind::System, long_text.clone());
    let width = 30u16;
    // 高度要容纳全部折行行（follow 窗口只显示底部 N 行，窄视口会滚出开头）。
    let buf = draw_to_test_backend(&mut view, width, 30);
    // 提取每行渲染出的文本（去掉尾部空白）。
    let mut rendered_lines: Vec<String> = Vec::new();
    for y in 0..30u16 {
        let mut row = String::new();
        for x in 0..width {
            row.push_str(buf[(x, y)].symbol());
        }
        let trimmed = row.trim_end().to_string();
        if !trimmed.is_empty() {
            rendered_lines.push(trimmed);
        }
    }
    assert!(
        rendered_lines.len() > 1,
        "超长行必须折成多行: {:?}",
        rendered_lines
    );
    // 不允许出现完全相同的两行（重复折行 bug 的特征：每行都从头重复同一段）。
    let mut seen = std::collections::HashSet::new();
    for line in &rendered_lines {
        assert!(
            seen.insert(line.clone()),
            "折行结果出现重复行（重复渲染 bug）: {line:?}\n全部行: {rendered_lines:?}"
        );
    }
    // 语义校验：整段文本被完整呈现（剥离 "系统 " 前缀后拼接）。
    let joined: String = rendered_lines.join("");
    for token in ["word0", "word5", "word20", "word39"] {
        assert!(
            joined.contains(token),
            "折行不得丢失内容片段 {token}: {rendered_lines:?}"
        );
    }
    // 每行必须前进（续行从上次断点继续，而不是回到开头）。
    // 第一行应包含行首 token，最后一行应包含行尾 token（内容完整覆盖）。
    assert!(
        rendered_lines.first().unwrap().contains("word0"),
        "首行必须从内容开头开始: {:?}",
        rendered_lines.first()
    );
    assert!(
        rendered_lines.iter().any(|l| l.contains("word39")),
        "末段内容必须出现（不得被吞）: {:?}",
        rendered_lines
    );
}

/// Menu also floats over the transcript: unselected rows must not bleed background text.
#[test]
fn menu_clears_background_before_rendering() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::Assistant, "背景文字X".repeat(20));
    }
    view.input = "/".to_string();
    view.refresh_command_menu();
    let buf = draw_to_test_backend(&mut view, 80, 24);
    // Locate a menu row by its item label (the menu is drawn over the transcript).
    let mut menu_row = None;
    let mut row_text = String::new();
    for y in 0..24u16 {
        row_text.clear();
        for x in 0..80u16 {
            row_text.push_str(buf[(x, y)].symbol());
        }
        if row_text.contains("/help") {
            menu_row = Some(y);
            break;
        }
    }
    let row = menu_row.expect("menu row with /help must be rendered");
    // Trailing cells of a menu row (beyond the item text) must be cleared to blank.
    assert_eq!(
        buf[(47, row)].symbol(),
        " ",
        "menu row must clear background text (col 47 leaked: {:?})",
        buf[(47, row)].symbol()
    );
}

/// §用户诉求：右侧边栏——打开时主区让出 SIDEBAR_WIDTH 列、边栏内容渲染、
/// 大纲行可点、滚动条存在。
#[test]
fn sidebar_renders_todo_outline_and_shrinks_main() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "第一条用户消息");
    view.push_line(LineKind::Assistant, "回复一");
    view.push_line(LineKind::User, "第二条用户消息更长一些用于测试截断");
    // 计划项（todo 段）。
    view.plan = Some(tpi_core::plan::Plan {
        explanation: None,
        items: vec![
            tpi_core::plan::PlanItem {
                text: "实现侧边栏".into(),
                status: tpi_core::plan::PlanStatus::InProgress,
            },
            tpi_core::plan::PlanItem {
                text: "写测试".into(),
                status: tpi_core::plan::PlanStatus::Pending,
            },
        ],
    });
    // 打开侧边栏（模拟 Ctrl+B 后的状态）。
    view.sidebar.open = true;
    let width: u16 = 80;
    let buf = draw_to_test_backend(&mut view, width, 24);
    let sidebar_w = crate::model::SIDEBAR_WIDTH;
    // 边栏右对齐：占据最右 SIDEBAR_WIDTH 列。
    let sidebar_start_x = width.saturating_sub(sidebar_w);
    // 边栏出现 todo 标题。
    let mut sidebar_text = String::new();
    for y in 0..24u16 {
        for x in sidebar_start_x..width {
            sidebar_text.push_str(buf[(x, y)].symbol());
        }
        sidebar_text.push('\n');
    }
    assert!(
        sidebar_text.contains("Todo"),
        "边栏必须显示 Todo: {sidebar_text}"
    );
    // CJK 占 2 cells，逐 cell 拼接时字符间会有空格；去空格后再断言。
    let compact: String = sidebar_text.chars().filter(|c| *c != ' ').collect();
    assert!(
        compact.contains("实现侧边栏"),
        "边栏必须显示计划项: {sidebar_text}"
    );
    assert!(
        compact.contains("用户消息"),
        "边栏必须显示用户消息标题: {sidebar_text}"
    );
    // 主区收窄：边栏首列是竖线分隔（draw_sidebar 每行第一个 span）。
    assert_eq!(
        buf[(sidebar_start_x, 1)].symbol(),
        "│",
        "主区与边栏之间应有竖线分隔"
    );
}

/// §用户诉求：侧边栏关闭时不占用任何宽度——主区占满全宽，无边栏内容。
#[test]
fn sidebar_closed_does_not_shrink_main() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "一条消息");
    let buf = draw_to_test_backend(&mut view, 80, 24);
    let sidebar_w = crate::model::SIDEBAR_WIDTH;
    let sidebar_start_x = 80u16.saturating_sub(sidebar_w);
    // 边栏区域不应出现边栏竖线（未打开）。
    let cell = buf[(sidebar_start_x, 1)].symbol();
    assert_ne!(cell, "│", "边栏未打开不得有竖线分隔: {cell:?}");
}

/// §用户诉求：侧边栏打开时浮层不得覆盖边栏区域——/sessions 的 Modal + 菜单
/// 横向布局基于主区（main_area）而非整个终端：Modal 右角（┐）必须位于主区内
/// （修复前按整宽居中，右边框伸进边栏被最后绘制的侧边栏盖掉，┐ 消失）。
#[test]
fn session_modal_and_menu_stay_within_main_area_when_sidebar_open() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::Assistant, "背景文字X".repeat(20));
    }
    view.sidebar.open = true;
    // /sessions：底部会话菜单 + 靠上 Modal 预览（同一场景，菜单也随主区收窄）。
    view.menu = Some(crate::model::MenuView {
        items: vec![(
            "0123456789abcdef0123456789abcdef".into(),
            "会话A · 12-31 10:00 · 42 事件".into(),
        )],
        selected: 0,
        kind: crate::model::MenuKind::Session,
        session_previews: vec![vec![crate::model::MenuPreviewLine {
            is_user: true,
            text: "你好".into(),
        }]],
        filter: String::new(),
    });
    view.open_modal("/sessions", "你 你好\nAI 你好");
    let buf = draw_to_test_backend(&mut view, 80, 24);
    let sidebar_start_x = 80u16 - crate::model::SIDEBAR_WIDTH;
    // Modal 上边框行含 ┌ 与 ┐：┐ 的 x 坐标必须 < sidebar_start_x。
    let mut right_corner_x = None;
    for y in 0..24u16 {
        let mut row = String::new();
        for x in 0..80u16 {
            row.push_str(buf[(x, y)].symbol());
        }
        if row.contains('┌') {
            right_corner_x = row.find('┐').map(|b| row[..b].chars().count() as u16);
            break;
        }
    }
    let right = right_corner_x.expect("Modal 右角 ┐ 必须渲染（不得被侧边栏盖掉）");
    assert!(
        right < sidebar_start_x,
        "Modal 右角必须位于主区内: {right} >= {sidebar_start_x}"
    );
}

/// §用户诉求：todo 长文本折行显示完整（不截断）——CJK 双宽按 2 cells
/// 折行；第一行带 marker，续行缩进对齐。此前单行截断丢内容，用户反馈
/// “todo 显示不完全”。
#[test]
fn sidebar_todo_wraps_long_text_by_cell_width() {
    let mut view = ViewModel {
        plan: Some(tpi_core::plan::Plan {
            explanation: None,
            items: vec![tpi_core::plan::PlanItem {
                text: "第一项：重构侧边栏布局与渲染管线使其支持长文本折行显示".into(),
                status: tpi_core::plan::PlanStatus::InProgress,
            }],
        }),
        sidebar: crate::model::SidebarState {
            open: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let buf = draw_to_test_backend(&mut view, 80, 24);
    let sidebar_start_x = 80u16 - crate::model::SIDEBAR_WIDTH;
    // 收集侧边栏全部行（逐 cell 拼接；CJK 中间插空格，过滤后断言）。
    let mut rows: Vec<String> = Vec::new();
    for y in 0..24u16 {
        let mut row_text = String::new();
        for x in sidebar_start_x..80u16 {
            row_text.push_str(buf[(x, y)].symbol());
        }
        if !row_text.trim().is_empty() {
            rows.push(row_text);
        }
    }
    // 找到 todo 首行（含 [>]），其后的行（用户消息标题之前）都是续行。
    let first = rows
        .iter()
        .position(|r| r.contains("[>]"))
        .expect("todo 首行");
    let todo_rows = rows[first..]
        .iter()
        .take_while(|r| !r.contains("用户消息"))
        .collect::<Vec<_>>();
    let compact: Vec<String> = todo_rows
        .iter()
        .map(|r| r.chars().filter(|c| *c != ' ').collect::<String>())
        .collect();
    // 第一行保留 marker + 头部（折行而非截断）。
    assert!(
        compact[0].contains("[>]第一项：重构侧边栏"),
        "首行应保留头部: {todo_rows:?}"
    );
    // 折行：至少 2 行，且续行包含尾部文本（完整显示，无 “…” 截断）。
    assert!(
        todo_rows.len() >= 2,
        "长 todo 必须折行而不是单行截断: {todo_rows:?}"
    );
    let tail_text: String = compact[1..].join("");
    assert!(
        tail_text.contains("长文本折行显示"),
        "续行必须包含文本尾部（完整显示）: {todo_rows:?}"
    );
    assert!(
        !compact.iter().any(|r| r.contains('…')),
        "折行不应出现省略号截断: {todo_rows:?}"
    );
}

/// §用户诉求：全部项完成/取消后 Todo 自动清空，不再显示完成态列表
/// （此前只检查 items 非空，全标完成后完成项仍挂在侧边栏）。
#[test]
fn sidebar_clears_todo_when_plan_fully_terminal() {
    let mut view = ViewModel {
        plan: Some(tpi_core::plan::Plan {
            explanation: None,
            items: vec![
                tpi_core::plan::PlanItem {
                    text: "完成的任务甲".into(),
                    status: tpi_core::plan::PlanStatus::Completed,
                },
                tpi_core::plan::PlanItem {
                    text: "取消的任务乙".into(),
                    status: tpi_core::plan::PlanStatus::Cancelled,
                },
            ],
        }),
        sidebar: crate::model::SidebarState {
            open: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let buf = draw_to_test_backend(&mut view, 80, 24);
    let sidebar_start_x = 80u16 - crate::model::SIDEBAR_WIDTH;
    let mut sidebar_text = String::new();
    for y in 0..24u16 {
        for x in sidebar_start_x..80u16 {
            sidebar_text.push_str(buf[(x, y)].symbol());
        }
        sidebar_text.push('\n');
    }
    let compact: String = sidebar_text.chars().filter(|c| *c != ' ').collect();
    assert!(
        compact.contains("(无活动计划)"),
        "全部终态后 Todo 应显示无活动计划: {sidebar_text}"
    );
    assert!(
        !compact.contains("完成任务甲") && !compact.contains("取消的任务乙"),
        "全部终态后不再显示完成项列表: {sidebar_text}"
    );
}

/// §用户诉求：部分完成时保留历史（终态项沉底），但开放项仍显示。
#[test]
fn sidebar_keeps_terminal_history_when_open_items_exist() {
    let mut view = ViewModel {
        plan: Some(tpi_core::plan::Plan {
            explanation: None,
            items: vec![
                tpi_core::plan::PlanItem {
                    text: "已完成的任务".into(),
                    status: tpi_core::plan::PlanStatus::Completed,
                },
                tpi_core::plan::PlanItem {
                    text: "进行中的任务".into(),
                    status: tpi_core::plan::PlanStatus::InProgress,
                },
            ],
        }),
        sidebar: crate::model::SidebarState {
            open: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let buf = draw_to_test_backend(&mut view, 80, 24);
    let sidebar_start_x = 80u16 - crate::model::SIDEBAR_WIDTH;
    let mut sidebar_text = String::new();
    for y in 0..24u16 {
        for x in sidebar_start_x..80u16 {
            sidebar_text.push_str(buf[(x, y)].symbol());
        }
        sidebar_text.push('\n');
    }
    let compact: String = sidebar_text.chars().filter(|c| *c != ' ').collect();
    assert!(
        compact.contains("进行中的任务"),
        "开放项必须显示: {sidebar_text}"
    );
    assert!(
        compact.contains("已完成的任务"),
        "有开放项时历史保留（沉底）: {sidebar_text}"
    );
    assert!(
        !compact.contains("(无活动计划)"),
        "有开放项时不得显示无活动计划: {sidebar_text}"
    );
}

/// §修复：draw_sidebar 写回可视高度——滚动条点击按比例跳转依赖
/// area_height（此前从未写回，默认 1，点击必跳底部）。
#[test]
fn sidebar_writes_back_area_height_for_ratio_jump() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::User, "一条用于撑高侧边栏的用户消息");
    }
    view.sidebar.open = true;
    let _buf = draw_to_test_backend(&mut view, 80, 24);
    // 整栏高 = 终端高（贯穿 transcript 到 footer）；高度必 > 1。
    assert!(
        view.sidebar.area_height > 1,
        "area_height 必须被写回: {}",
        view.sidebar.area_height
    );
    // 比例跳转按可视高度计算：点击中部应落到中间附近，而非底部。
    let before = view.sidebar.scroll;
    view.sidebar.scroll_to_ratio(view.sidebar.area_height / 2);
    assert!(
        view.sidebar.scroll < view.sidebar.total_rows.saturating_sub(1) || before > 0,
        "点击侧边栏中部不应直接跳到底部"
    );
}

/// Reasoning overlay must show its own border title, not "Tool details".
#[test]
fn reasoning_overlay_uses_thinking_border_title() {
    let mut view = ViewModel::default();
    for _ in 0..40 {
        view.push_line(LineKind::Assistant, "背景文字X".repeat(4));
    }
    view.overlay = Some(crate::model::OverlayState::for_reasoning("let me think"));
    let buf = draw_to_test_backend(&mut view, 80, 24);
    // Find the overlay top border row (Block title is drawn there).
    let mut border_row = None;
    for y in 0..buf.area().height {
        let mut row = String::new();
        for x in 0..buf.area().width {
            row.push_str(buf[(x, y)].symbol());
        }
        if row.contains('┌') {
            border_row = Some(row);
            break;
        }
    }
    let row = border_row.expect("overlay top border must be rendered");
    assert!(
        row.contains('思') && row.contains('考') && row.contains("reasoning") && row.contains('）'),
        "reasoning overlay border must show the thinking title, got: {row:?}"
    );
    assert!(
        !row.contains("Tool details"),
        "reasoning overlay must not show the Tool details border title, got: {row:?}"
    );
}
/// 修复：模型输出（Running + 空输入）期间隐藏光标，避免一直闪烁；空闲时显示。
#[test]
fn cursor_hidden_during_run_when_input_empty() {
    // Product rule: idle always shows; running shows only when input is non-empty;
    // hidden while a Modal/Overlay is open or while search (Ctrl+F) is open.
    assert!(should_show_input_cursor(
        &StatusLine::Idle,
        true,
        false,
        false
    ));
    assert!(should_show_input_cursor(
        &StatusLine::Idle,
        false,
        false,
        false
    ));
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            step: 1,
            tool: "x".into()
        },
        true,
        false,
        false
    ));
    assert!(should_show_input_cursor(
        &StatusLine::Running {
            step: 1,
            tool: "x".into()
        },
        false,
        false,
        false
    ));
    assert!(
        !should_show_input_cursor(&StatusLine::Idle, true, true, false),
        "cursor must hide while Modal/Overlay is open"
    );
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            step: 1,
            tool: "x".into()
        },
        false,
        true,
        false
    ));
    assert!(
        !should_show_input_cursor(&StatusLine::Idle, true, false, true),
        "cursor must hide while search is open (typing goes to the search box)"
    );
    assert!(!should_show_input_cursor(
        &StatusLine::Running {
            step: 1,
            tool: "x".into()
        },
        false,
        false,
        true
    ));

    // Byte level: running + empty input must emit the hide sequence.
    let mut view = ViewModel {
        status: StatusLine::Running {
            step: 1,
            tool: "generating".into(),
        },
        ..ViewModel::default()
    };
    let s = String::from_utf8_lossy(&draw_captured_bytes(&mut view)).into_owned();
    assert!(s.contains("\x1b[?25l"), "running must hide cursor: {s:?}");

    // Idle frame must show cursor.
    let mut view2 = ViewModel {
        status: StatusLine::Idle,
        ..ViewModel::default()
    };
    let s2 = String::from_utf8_lossy(&draw_captured_bytes(&mut view2)).into_owned();
    assert!(s2.contains("\x1b[?25h"), "idle must show cursor: {s2:?}");

    // Search open (Ctrl+F): typing goes into the search box, composer cursor must hide.
    let mut view3 = ViewModel {
        status: StatusLine::Idle,
        ..ViewModel::default()
    };
    view3.open_search();
    let s3 = String::from_utf8_lossy(&draw_captured_bytes(&mut view3)).into_owned();
    assert!(
        s3.contains("\x1b[?25l"),
        "search open must hide cursor: {s3:?}"
    );
}

/// §性能：wrap 缓存——历史 entry 跨帧复用；transcript 结构变化后
/// （push_line 等 bump revision）缓存由 draw 层清空重建；active_hit/search
/// 高亮在窗口层应用，不依赖缓存内容。
#[test]
fn wrap_cache_reuses_and_invalidates() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "第一行 hello");
    let rev0 = view.transcript_revision;
    let mut cache = HashMap::new();
    let mut wrap_cache: HashMap<(EntryId, u16), Arc<Vec<WrappedRow>>> = HashMap::new();
    let plan1 = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    assert_eq!(wrap_cache.len(), 1, "历史 entry 必须写入缓存");
    assert_eq!(plan1.window.len(), 2, "消息行 + 留白空行");
    let key = (view.transcript[0].id(), 80);
    let cached_rows = Arc::clone(&wrap_cache[&key]);
    // 丢掉 markdown 缓存后再次调用：完整的历史 wrap-cache 应走稳态快
    // 路径，不得为了输入/光标重绘而重建全部 Markdown 逻辑行。
    cache.clear();
    let _plan2 = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    assert_eq!(wrap_cache.len(), 1, "未变化时命中缓存不增长");
    assert!(cache.is_empty(), "稳态重绘不得重新构建历史 Markdown");
    assert!(
        Arc::ptr_eq(&cached_rows, &wrap_cache[&key]),
        "稳态重绘必须共享 wrapped rows，而不是深拷贝整段历史"
    );
    // 新增 entry → revision bump；draw 层会清空缓存（此处模拟）。
    view.push_line(LineKind::Assistant, "第二行 world");
    assert_ne!(
        view.transcript_revision, rev0,
        "push_line 必须 bump revision"
    );
    wrap_cache.clear();
    let _plan3 = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    assert_eq!(wrap_cache.len(), 2, "两个历史 entry 都缓存");
}

/// §14 高亮：搜索命中条目整段带下划线，未命中条目不带。
#[test]
fn search_highlight_underlines_matched_entries() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "占位行");
    view.push_line(LineKind::Assistant, "第一条 hello 内容");
    view.push_line(LineKind::Assistant, "第二条 world 内容");
    view.open_search();
    view.update_search_query("hello");
    let buf = draw_to_test_backend(&mut view, 80, 12);
    let mut matched_underlined = false;
    let mut unmatched_not_underlined = true;
    for y in 0..12u16 {
        for x in 0..80u16 {
            let cell = &buf[(x, y)];
            match cell.symbol() {
                "h" => matched_underlined = cell.modifier.contains(Modifier::UNDERLINED),
                "w" => {
                    unmatched_not_underlined = !cell.modifier.contains(Modifier::UNDERLINED);
                }
                _ => {}
            }
        }
    }
    assert!(matched_underlined, "命中条目必须带下划线高亮");
    assert!(unmatched_not_underlined, "未命中条目不得带下划线");
}

/// ISSUE-030：搜索词清空后必须恢复 follow（此前视口停在旧命中位置，
/// 用户关闭搜索后以为内容丢了）。
#[test]
fn clearing_search_query_restores_follow() {
    use crate::scroll::ScrollMode;
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "第一条 hello 内容");
    view.push_line(LineKind::Assistant, "第二条 world 内容");
    view.open_search();
    // 命中 → 锁定（离开 follow）。
    view.update_search_query("hello");
    assert!(matches!(view.scroll_mode, ScrollMode::Locked(_)));
    // 删空搜索词 → 恢复 follow（ISSUE-030）。
    view.update_search_query("");
    assert!(
        matches!(view.scroll_mode, ScrollMode::Follow),
        "空搜索词必须恢复 follow: {:?}",
        view.scroll_mode
    );
    assert_eq!(view.pending_below, 0);
}

/// §视觉瘦身：不再有常驻 header（信息并入 footer）；消息双角色 rail（you/AI）。
#[test]
fn role_rails_render_without_header() {
    let mut view = ViewModel {
        model_name: "test-model".into(),
        workspace: "tpi".into(),
        ..Default::default()
    };
    view.push_line(LineKind::User, "hello");
    view.push_line(LineKind::Assistant, "hi there");
    let buffer = draw_to_test_backend(&mut view, 60, 10);

    // 视觉瘦身：header 已移除，transcript 上移到第 0 行（不再有品牌行）。
    let top_symbol = buffer
        .cell((0, 0))
        .map(|c| c.symbol().to_string())
        .unwrap_or_default();
    assert_ne!(
        top_symbol.as_str(),
        "T",
        "无常驻 header：transcript 从第 0 行开始（首 cell 是 rail │ 而非品牌）"
    );

    // 消息 rail：整屏文本应含 you 与 AI 标签。
    let mut all = String::new();
    for y in 0..10u16 {
        for x in 0..60u16 {
            all.push(
                buffer
                    .cell((x, y))
                    .map(|c| c.symbol().chars().next().unwrap_or(' '))
                    .unwrap_or(' '),
            );
        }
    }
    assert!(all.contains("you"), "用户消息必须有 you rail: {all:?}");
    assert!(all.contains("AI"), "assistant 消息必须有 AI rail: {all:?}");
}

/// §16.2 增强：工具名按类别着色（bash=info 色，编辑类=success 色）。
#[test]
fn tool_name_style_is_category_colored() {
    let theme = theme::Theme::omp();
    assert_eq!(tool_name_style("bash", theme).fg, Some(theme.info));
    assert_eq!(tool_name_style("edit", theme).fg, Some(theme.success));
    assert_eq!(tool_name_style("read", theme).fg, Some(theme.accent));
    assert_eq!(tool_name_style("web_search", theme).fg, Some(theme.warning));
    assert_eq!(tool_name_style("unknown", theme).fg, Some(theme.text));
}

/// §InteractionRefactor：应用内选择高亮——语义选区（entry + 偏移）投影回
/// 视觉行。选中第 2、3 个 entry 的第一字符起 → 对应 window 行 1-2 高亮。
#[test]
fn selection_highlights_selected_window_rows() {
    use crate::interaction::TextPosition;
    use crate::scroll::EntryId;
    let mut view = ViewModel::default();
    for i in 0..6 {
        view.push_line(LineKind::Assistant, format!("line {i}"));
    }
    // 语义选区：entry 2 偏移 0 → entry 4 偏移 0（覆盖 entry 2、3 两行正文）。
    view.selection_start(TextPosition {
        entry_id: EntryId(2),
        offset: 0,
    });
    view.selection_update(TextPosition {
        entry_id: EntryId(4),
        offset: 0,
    });
    view.selection_end();
    let mut cache = HashMap::new();
    // §美化：6 条消息 + 6 留白空行 = 12 行；视口 12 容纳全部。
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 12, 0, false, &mut cache);
    assert_eq!(
        plan.window.len(),
        12,
        "6 条消息 + 6 留白各一行: {:?}",
        plan.window
    );
    let has_reversed = |idx: usize| {
        plan.window[idx]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
    };
    // 正文行布局：entry_id 从 1 起，entry_i 正文在行 2*(i-1)、留白在 2*(i-1)+1。
    // 选中 entry2/3 正文 → window 行 2、4 高亮；留白行 3、5 与未选行不高亮。
    assert!(
        has_reversed(2) && has_reversed(4),
        "选中 entry2/3 正文必须高亮"
    );
    assert!(!has_reversed(3), "留白空行不得高亮");
    assert!(!has_reversed(5), "留白空行不得高亮");
    assert!(
        !has_reversed(0) && !has_reversed(1) && !has_reversed(11),
        "未选中行不高亮"
    );
}

/// §用户诉求：表格后文字可复制——含表格的 Assistant 消息，表格之后
/// 的文字行必须有语义映射（否则拖选/复制触发点缺失）。
#[test]
fn text_after_table_is_selectable() {
    let mut view = ViewModel::default();
    view.push_line(
        LineKind::Assistant,
        "| 列A | 列B |\n|---|---|\n| 1 | 2 |\n\n表格下面的文字段落，应该可以复制。",
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 30, 0, false, &mut cache);
    // 找到语义文本含“表格下面的文字”的行——它必须存在且有 semantic。
    let found = plan.semantic_rows.iter().any(|row| {
        row.as_ref()
            .is_some_and(|r| r.text.contains("表格下面的文字"))
    });
    assert!(found, "表格后面的文字必须可选中（有语义映射）");
    // 表格自身行（如 ┌──── 边框行）也应可选。
    let mut table_found = false;
    for row in plan.semantic_rows.iter().flatten() {
        if row.text.contains("列A") || row.text.contains('│') {
            table_found = true;
        }
    }
    assert!(table_found, "表格行本身也应可选中");
}

/// §用户诉求：表格后文字可复制——selected_text 的提取（canonical_semantic_text
/// 用 semantic_width 渲染）必须与 renderer 的 hit 坐标系（同一宽度）对齐。
/// 复现：宽表格在窄 width 下列宽收缩、cell 竖排换行，width=None 与
/// width=content_width 渲染出不同行数 → 旧实现 offset 错位，表格后文字复制不出。
#[test]
fn table_after_text_is_copyable() {
    // 宽表格（真实触发宽度收缩）：width=None 渲染 7 行，width=40 渲染 22 行。
    let wide_md = "| 项目 | 状态 | 负责人 | 备注 |\n|---|---|---|---|\n| 修复侧边栏布局 | 进行中 | 张三 | 需要在窄屏下也能显示完整 |\n| 优化菜单渲染 | 完成 | 李四 | 边框与背景统一 |\n\n表格后的总结文字。";
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, wide_md.to_string());
    // 模拟 renderer 写回语义宽度（RenderFrame 在真实路径里做，此处手动设）。
    view.semantic_width = Some(40);
    // 选中整个 entry（覆盖全文），验证复制文本包含表格后的文字。
    use crate::interaction::{TextPosition, TextSelection};
    let entry_id = view.transcript[0].id();
    view.selection = Some(TextSelection {
        anchor: TextPosition {
            entry_id,
            offset: 0,
        },
        focus: TextPosition {
            entry_id,
            offset: 10_000, // 远超文本长，offset 会被 clamp 到实际长度
        },
    });
    let text = view.selected_text();
    assert!(
        text.contains("表格后的总结文字"),
        "selected_text 必须包含表格后的文字: {text:?}"
    );
    assert!(
        text.contains("修复侧边栏布") || text.contains("修复侧边栏布局"),
        "表格内容也应保留（窄宽下 cell 竖排拆行）: {text:?}"
    );
    // 没有 semantic_width（未渲染过）时也应有内容（width=None 自然宽）。
    let mut view2 = ViewModel::default();
    view2.push_line(LineKind::Assistant, wide_md.to_string());
    let entry_id2 = view2.transcript[0].id();
    view2.selection = Some(TextSelection {
        anchor: TextPosition {
            entry_id: entry_id2,
            offset: 0,
        },
        focus: TextPosition {
            entry_id: entry_id2,
            offset: 10_000,
        },
    });
    assert!(view2.selected_text().contains("表格后的总结文字"));
}

/// §InteractionRefactor：plan_window 的语义映射与视觉行必须对齐——
/// semantic_rows 每行都能定位到对应 entry 的文本，且语义文本能命中真实内容。
#[test]
fn semantic_rows_align_with_window_and_map_text() {
    use crate::scroll::EntryId;
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "hello world");
    view.push_line(LineKind::Assistant, "second line");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    // 语义行与窗口行等长（每 entry 至少 1 行）。
    assert_eq!(
        plan.semantic_rows.len(),
        plan.window.len(),
        "semantic_rows 必须与 window 等长"
    );
    assert!(!plan.semantic_rows.is_empty());
    // §美化：消息块间有留白空行（text 为空、不可选）；非空行断言正文语义。
    for row in plan.semantic_rows.iter() {
        let row = row.as_ref().expect("窗口行必须有语义映射");
        assert!(
            row.entry_id == EntryId(1) || row.entry_id == EntryId(2),
            "归属错误 entry: {:?}",
            row.entry_id
        );
        if row.text.is_empty() {
            // 留白空行：合法分隔（不可选），跳过 rail/非空断言。
            continue;
        }
        assert!(
            !row.text.starts_with('│'),
            "语义文本不得含 rail 前缀: {:?}",
            row.text
        );
        assert!(!row.text.is_empty(), "正文行语义文本不得为空");
    }
    // 语义文本必须能被定位：entry 1 的语义文本就是 "hello world"（markdown 渲染无装饰）。
    let e1_text = plan
        .semantic_rows
        .iter()
        .find_map(|r| {
            let row = r.as_ref()?;
            (row.entry_id == EntryId(1)).then(|| row.text.clone())
        })
        .expect("entry 1 必须有语义行");
    assert!(
        e1_text.contains("hello"),
        "entry1 语义文本错误: {e1_text:?}"
    );
}

/// §InteractionRefactor：hit_text 从屏幕坐标命中语义位置——CJK 按 cell 宽度。
#[test]
fn hit_text_maps_screen_column_to_char_offset() {
    use crate::interaction::{TextPosition, cell_to_char};
    // 纯函数直接验证（渲染层 hit_text 复用同一映射）。
    let text = "abc你好xyz";
    assert_eq!(cell_to_char(text, 3), 3, "你 的第 1 个 cell 是 char 3");
    assert_eq!(cell_to_char(text, 4), 3, "你 的第 2 个 cell 仍是 char 3");
    assert_eq!(cell_to_char(text, 5), 4, "好 的第 1 个 cell 是 char 4");
    // TextPosition 构造与排序。
    let a = TextPosition {
        entry_id: EntryId(1),
        offset: 3,
    };
    let b = TextPosition {
        entry_id: EntryId(1),
        offset: 5,
    };
    assert!(a < b);
}

/// §PointerHit ④：wrap_with_semantic 一次 layout 产出语义映射。
/// - 视觉行数与语义行数严格一致（不二次折行）；
/// - 首行 decor = 逻辑行前缀宽度，续行 decor = 0（P0-2 修复）。
#[test]
fn wrap_with_semantic_produces_exact_mapping() {
    use crate::scroll::EntryId;
    let entry_id = EntryId(1);
    // 模拟 User 消息：rail "│ " + "you  " = 7 cell 装饰，正文 "hello"。
    let line = Line::from(vec![
        Span::styled("│ ", Style::default()),
        Span::styled("you  ", Style::default()),
        Span::styled("hello", Style::default()),
    ]);
    let semantic = SemanticLine {
        text: "hello".to_string(),
        decor_cells: 7,
        links: Vec::new(),
        rail: Some(Span::styled("│ ", Style::default())),
    };
    let rows = wrap_with_semantic(vec![line], vec![None], &[semantic], 80, entry_id);
    assert_eq!(rows.len(), 1, "短文本单行");
    let row = &rows[0];
    let row_sem = row.semantic.as_ref().expect("必须有语义映射");
    assert_eq!(row_sem.entry_id, entry_id);
    assert_eq!(row_sem.char_start, 0, "首行语义从 entry 偏移 0 开始");
    assert_eq!(row_sem.text, "hello");
    assert_eq!(
        row_sem.decor, 7,
        "短文本首行 decor 必须是 rail 宽度（P0-2 修复）"
    );

    // 长文本换行：语义行数 == 视觉行数，续行 decor=rail 宽、带竖线前缀。
    let long = "x".repeat(90); // 90 cell，宽 40 → 3 行。
    let line2 = Line::from(vec![
        Span::styled("│ ", Style::default()),
        Span::styled("you  ", Style::default()),
        Span::styled(long.clone(), Style::default()),
    ]);
    let semantic2 = SemanticLine {
        text: long.clone(),
        decor_cells: 7,
        links: Vec::new(),
        rail: Some(Span::styled("│ ", Style::default())),
    };
    let rows = wrap_with_semantic(vec![line2], vec![None], &[semantic2], 40, entry_id);
    // 宽 40：首行内容容量 = 40-7 = 33 cell → "x"×33；续行带 rail（2）→
    // 内容容量 38、19。
    assert_eq!(rows.len(), 3, "90 cell 内容宽 40 → 3 视觉行");
    // 首行 decor=7；续行 decor=rail 宽（2）——竖线前缀延续。
    assert_eq!(
        rows[0].semantic.as_ref().unwrap().decor,
        7,
        "首行 decor=rail"
    );
    assert_eq!(
        rows[1].semantic.as_ref().unwrap().decor,
        2,
        "续行 decor=rail 宽（竖线不截断）"
    );
    assert_eq!(
        rows[2].semantic.as_ref().unwrap().decor,
        2,
        "末行 decor=rail 宽"
    );
    // 续行视觉行首 span 必须是竖线（§用户诉求：换行竖线连续）。
    for (i, row) in rows.iter().enumerate().skip(1) {
        let first = row
            .line
            .spans
            .first()
            .map(|s| s.content.as_ref())
            .unwrap_or("");
        assert_eq!(first, "│ ", "续行 {i} 首 span 必须是竖线前缀");
    }
    // 语义文本拼接 = 原文。
    let joined: String = rows
        .iter()
        .filter_map(|r| r.semantic.as_ref().map(|s| s.text.as_str()))
        .collect();
    assert_eq!(joined, long, "语义拼接必须等于原文（不丢字）");
}

/// 端到端：任意字符可选中（非整行）。
/// 用 plan_window 的 semantic_rows 模拟 hit_text 的列→offset 映射，再走
/// ViewModel::selected_text 的 char 级提取。选中中间 3 个字符 → 精确返回。
#[test]
fn arbitrary_char_selection_is_char_precise() {
    use crate::interaction::{TextPosition, cell_to_char, chars_to_cells};
    use crate::model::LineKind;
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "hello world");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    let first = plan.semantic_rows[0]
        .as_ref()
        .expect("第一行必须有语义映射");
    let text = first.text.as_str();
    let char_start = first.char_start;
    let decor = first.decor;
    assert_eq!(text, "hello world");
    // 模拟 hit_text：视觉 cell 列 → 语义 offset。
    // 屏幕列 = rect.x + decor + 目标字符前的 cell 宽度。
    let to_offset = |screen_col: usize| -> usize {
        let semantic_col = screen_col.saturating_sub(decor);
        cell_to_char(text, semantic_col) + char_start
    };
    // 屏幕列指向 "world" 的开头（decor + "hello " = 7 + 6 = 13 cell 列）。
    let w_col = decor + chars_to_cells("hello ", 6);
    let w_offset = to_offset(w_col);
    assert_eq!(w_offset, 6, "w 的 offset 应为 6");
    // 选中 "wor"（offset 6..9）。
    view.selection_start(TextPosition {
        entry_id: first.entry_id,
        offset: w_offset,
    });
    view.selection_update(TextPosition {
        entry_id: first.entry_id,
        offset: w_offset + 3,
    });
    view.selection_end();
    assert_eq!(view.selected_text(), "wor", "必须精确选中 3 个字符，非整行");
    // CJK：选中 2 个汉字。§美化：entry2 前有留白空行，按 entry 定位
    //（不用固定 index——留白行 text 为空）。
    view.push_line(LineKind::Assistant, "你好世界");
    let mut cache2 = HashMap::new();
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        10,
        0,
        false,
        &mut cache2,
    );
    let second = plan2
        .semantic_rows
        .iter()
        .find_map(|r| {
            let row = r.as_ref()?;
            (row.entry_id == EntryId(2)).then_some(row)
        })
        .expect("entry 2 必须有语义映射");
    assert_eq!(second.text, "你好世界");
    view.selection_start(TextPosition {
        entry_id: second.entry_id,
        offset: 1,
    });
    view.selection_update(TextPosition {
        entry_id: second.entry_id,
        offset: 3,
    });
    view.selection_end();
    assert_eq!(view.selected_text(), "好世", "CJK 按 char 精确选中");
}

/// §美化：User 消息 = 左竖线(┃) + panel 背景块（行内首个 span 带 bg），
/// Assistant 保持无背景 rail —— 形成"用户有底、助手裸文本"层次。
#[test]
fn user_message_gets_panel_background_assistant_stays_plain() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "帮我修复");
    view.push_line(LineKind::Assistant, "好的");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 12, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 通过语义行定位正文行索引（wrap 后 span 逐字符拆分，不能直接按文本找）。
    let find_row = |target: &str| -> usize {
        plan.semantic_rows
            .iter()
            .position(|r| r.as_ref().is_some_and(|row| row.text == target))
            .expect("目标消息必须渲染")
    };
    let user_row = find_row("帮我修复");
    assert!(
        plan.window[user_row]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(theme.panel)),
        "用户消息行首 span 必须带 panel 背景: {:?}",
        plan.window[user_row].spans
    );
    let assistant_row = find_row("好的");
    assert!(
        plan.window[assistant_row]
            .spans
            .iter()
            .all(|s| s.style.bg.is_none()),
        "assistant 消息保持裸文本（无背景）: {:?}",
        plan.window[assistant_row].spans
    );
}

/// §美化：消息块间插留白空行（text 为空、不可选）。
#[test]
fn message_blocks_are_separated_by_gap_rows() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Assistant, "line a");
    view.push_line(LineKind::Assistant, "line b");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    // 每条消息 1 正文 + 1 留白 = 4 行（无折行）。
    assert_eq!(plan.window.len(), 4);
    // 第 1、3 行是留白空行（语义为空，不可选）。
    assert_eq!(plan.semantic_rows[1].as_ref().unwrap().text, "");
    assert_eq!(plan.semantic_rows[3].as_ref().unwrap().text, "");
    // 正文行语义非空。
    assert_eq!(plan.semantic_rows[0].as_ref().unwrap().text, "line a");
    assert_eq!(plan.semantic_rows[2].as_ref().unwrap().text, "line b");
}

/// §美化：thinking 加 ◆ 图标前缀（与正文/工具区分）。
#[test]
fn thinking_lines_carry_icon_prefix() {
    let mut view = ViewModel {
        collapsed_lines: 10, // 单行 thinking 不折叠（默认 0 会折叠）。
        ..Default::default()
    };
    view.push_line(LineKind::Reasoning, "先分析再动手");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 12, 0, false, &mut cache);
    // 首行是 thinking（◆ 前缀 + 正文）；语义文本不含前缀。
    let first = &plan.window[0];
    let text: String = first.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("◆ 思考"),
        "thinking 行带 ◆ 图标前缀: {text:?}"
    );
    assert!(text.contains("先分析再动手"), "正文保留: {text:?}");
    // 语义文本是纯正文（无前缀，不可复制前缀）。
    let semantic = plan.semantic_rows[0].as_ref().unwrap();
    assert_eq!(semantic.text, "先分析再动手");
}

/// §用户诉求：代码块只做颜色变化（语法高亮前景色），不加背景。
#[test]
fn highlighted_code_block_has_no_background() {
    let theme = theme::Theme::omp();
    let lines = highlight::highlight_code_block("fn main() { let x = 1; }", Some("rust"), theme)
        .expect("rust 可解析");
    assert_eq!(lines.len(), 1);
    for span in &lines[0].spans {
        assert!(
            span.style.bg.is_none(),
            "语法高亮 span 不得带背景: {:?}",
            span
        );
    }
}

#[test]
fn extended_syntax_pack_supports_common_agent_languages() {
    for language in ["tsx", "toml", "go", "kotlin", "terraform", "vue", "svelte"] {
        assert!(
            highlight::supports_language(language),
            "扩展语法包应支持 {language}"
        );
    }
}

#[test]
fn read_tool_card_highlights_source_without_polluting_semantics() {
    let theme = theme::Theme::onedarkpro();
    let card = ToolCard {
        id: "read-1".into(),
        name: "read".into(),
        target: Some("read src/main.rs".into()),
        command: None,
        state: ToolCardState::Done {
            status: tpi_core::outcome::ToolStatus::Succeeded,
            duration_ms: 1,
            exit_code: None,
        },
        output: Some("fn main() { let answer = 42; }".into()),
        diff: None,
        output_truncated: false,
        expanded: true,
        tail: None,
        line_number_start: Some(1),
        collapsed_lines: 10,
        started_at_ms: None,
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let source = lines
        .iter()
        .find(|line| line.spans.iter().any(|span| span.content.contains("42")))
        .expect("read 正文可见");
    // §用户诉求：syntect 完整主题——数字 token 有独立前景色（不再依赖
    // 语义色映射，不断言具体色值，只断言有着色）。
    assert!(
        source
            .spans
            .iter()
            .filter(|span| !span.content.is_empty())
            .any(|span| span.style.fg.is_some()),
        "工具卡源码应有语法前景色: {source:?}"
    );
    assert!(
        source
            .spans
            .iter()
            .filter(|span| !span.content.is_empty())
            .all(|span| span.style.bg == Some(theme.panel)),
        "工具卡高亮应沿用 panel 背景"
    );
    let semantic = card_semantic_rows(&card);
    assert!(
        semantic
            .iter()
            .any(|row| row == "fn main() { let answer = 42; }")
    );
    assert!(!semantic.iter().any(|row| row.starts_with("1 │")));
}

/// §用户诉求：语法高亮用 syntect 完整主题——数字与关键字等不同 token
/// 各有独立颜色（不再是语义色映射下的少量色值）。
#[test]
fn syntect_theme_colors_tokens_distinctly_under_any_tui_theme() {
    for theme in [
        theme::Theme::omp(),
        theme::Theme::dark(),
        theme::Theme::light(),
        theme::Theme::opencode(),
        theme::Theme::onedarkpro(),
    ] {
        let lines =
            highlight::highlight_code_block("fn main() { let x: u32 = 42; }", Some("rust"), theme)
                .expect("rust 可解析");
        let line = &lines[0];
        let fg = |token: &str| -> Option<ratatui::style::Color> {
            line.spans
                .iter()
                .find(|s| s.content == token)
                .and_then(|s| s.style.fg)
        };
        let num = fg("42").expect("数字 token 必须存在");
        let keyword = fg("fn").expect("fn token 必须存在");
        let ty = fg("u32").expect("类型 token 必须存在");
        let distinct = [num, keyword, ty]
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        // 不同类别 token 至少有 2 种不同颜色且都非空（主题间存在 scope
        // 合并，故不要求全部互异）。
        assert!(
            distinct >= 2,
            "token 类别应有区分颜色（{num:?} {keyword:?} {ty:?}）"
        );
    }
}

/// §用户诉求：行内代码与代码块一致——只做颜色变化（primary 前景色）、
/// 不加背景；行尾 padding 也不得继承任何背景。
#[test]
fn inline_code_has_no_background_and_no_padding_leak() {
    let mut view = ViewModel::default();
    // assistant 消息行首 rail 无背景；行内含 inline code。
    view.push_line(
        LineKind::Assistant,
        r"请查看 `snake\src\main.rs` 是否已存在",
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 60, 10, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 定位含 "main.rs" 的行。
    let row_idx = plan
        .semantic_rows
        .iter()
        .position(|r| r.as_ref().is_some_and(|s| s.text.contains("main.rs")))
        .expect("消息必须渲染");
    let line = &plan.window[row_idx];
    // inline code span 本身无背景、保留 primary 前景色（wrap 逐字符拆分 span）。
    let code_span = line
        .spans
        .iter()
        .find(|s| s.content == "m")
        .expect("inline code 内容必须渲染");
    assert_eq!(
        code_span.style.bg, None,
        "行内 code 不得带背景: {:?}",
        line.spans
    );
    assert_eq!(
        code_span.style.fg,
        Some(theme.primary),
        "行内 code 保留 primary 前景色: {:?}",
        code_span
    );
    // 行尾 padding（内容全空格的 span）不得带任何背景。
    for span in &line.spans {
        if span.content.chars().all(|c| c == ' ') && !span.content.is_empty() {
            assert_eq!(span.style.bg, None, "行尾 padding 不得带背景: {:?}", span);
        }
    }
}

/// §用户诉求：diff 行只改前景色——面板底统一由卡片承担，行尾 padding
/// 用 panel 填满，绝不出现红/绿背景（也不再需要 Line 级红绿 fallback）。
#[test]
fn diff_line_padding_keeps_panel_background() {
    let mut view = ViewModel {
        collapsed_lines: 10, // 展开 diff 正文（默认 0 折叠）。
        ..Default::default()
    };
    view.begin_tool("c1", "edit", Some("src/main.rs".into()), None);
    view.finish_tool(
        ("c1", "edit"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "",
        Some("-    let x = 1;\n+    let x = 2;".into()),
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 50, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 定位删除行（含 "let x = 1"）。
    let row_idx = plan
        .semantic_rows
        .iter()
        .position(|r| r.as_ref().is_some_and(|s| s.text.contains("let x = 1")))
        .expect("diff 删除行必须渲染");
    let line = &plan.window[row_idx];
    // diff 行前景 = error（红字），背景统一 panel（无红/绿底）。
    assert!(
        line.spans.iter().any(|s| s.style.fg == Some(theme.error)),
        "diff 删除行必须红字: {:?}",
        line.spans
    );
    assert!(
        line.spans
            .iter()
            .all(|s| s.style.bg != Some(theme.error) && s.style.bg != Some(theme.success)),
        "diff 行不得带红/绿背景: {:?}",
        line.spans
    );
    // 行尾 padding 用 panel 底填满（不是红/绿）。
    let has_panel_pad = line.spans.iter().any(|s| {
        !s.content.is_empty()
            && s.content.chars().all(|c| c == ' ')
            && s.style.bg == Some(theme.panel)
    });
    assert!(
        has_panel_pad,
        "diff 行尾必须用 panel 底填充到满宽: {:?}",
        line.spans
    );
}

/// §修复回归：卡片正文文字区背景与卡片面板一致（不落到终端底色）。
/// 主行 name/内容行正文都烙 panel。
#[test]
fn tool_card_body_bg_matches_panel() {
    let mut view = ViewModel {
        collapsed_lines: 10, // 显示内容行正文（默认 0 折叠）。
        ..Default::default()
    };
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "第一行输出\n第二行输出",
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 主行 name span 带 panel（wrap 逐字符拆分，任一 "bash" 字符即可）。
    let header_row = &plan.window[0];
    assert!(
        header_row
            .spans
            .iter()
            .any(|s| s.content == "b" && s.style.bg == Some(theme.panel)),
        "主行 name 必须烙 panel 底: {:?}",
        header_row.spans
    );
    // 主行所有非空 span 不得残留无背景（含分隔空格）。
    for span in &header_row.spans {
        if !span.content.is_empty() {
            assert!(
                span.style.bg.is_some(),
                "主行每段文字都带面板底: {:?}",
                span
            );
        }
    }
    // 内容行正文（"第一行输出"）带 panel。
    let body_row = plan
        .window
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content == "第"))
        .expect("内容行必须渲染");
    assert!(
        body_row
            .spans
            .iter()
            .any(|s| s.content == "一" && s.style.bg == Some(theme.panel)),
        "内容行正文必须烙 panel 底: {:?}",
        body_row.spans
    );
}

/// §修复回归：User 消息正文背景与面板一致；inline code 只做颜色变化
/// （primary 前景、无背景，不被 panel 覆盖也不带自身背景）。
#[test]
fn user_message_body_bg_matches_panel() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, r"修复 `snake\src\main.rs`");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 10, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    let row = plan
        .window
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content == "修"))
        .expect("用户消息必须渲染");
    // 正文普通字符烙 panel。
    assert!(
        row.spans
            .iter()
            .any(|s| s.content == "修" && s.style.bg == Some(theme.panel)),
        "用户消息正文必须烙 panel 底: {:?}",
        row.spans
    );
    // inline code：无自身背景——在 User 面板里随正文烙 panel 底，仅保留
    // primary 前景色（不再有独立的 surface_subtle 块）。
    let code_span = row
        .spans
        .iter()
        .find(|s| s.content == "s")
        .expect("inline code 内容必须渲染");
    assert_eq!(
        code_span.style.bg,
        Some(theme.panel),
        "inline code 应随面板烙 panel 底（无独立背景）: {:?}",
        row.spans
    );
    assert_eq!(
        code_span.style.fg,
        Some(theme.primary),
        "inline code 保留 primary 前景色: {:?}",
        code_span
    );
}

/// §修复：thinking 卡片化——panel 底 + 左竖线 + 整卡可点展开。
#[test]
fn thinking_renders_as_panel_card() {
    // 配置 collapsed_lines=6：8 行 thinking → 折叠态（8 > 6，且默认 0 只显示
    // 主行；这里显式 6 复现“折叠线”场景）。
    let mut view = ViewModel {
        collapsed_lines: 6,
        ..Default::default()
    };
    // 8 行 thinking → 折叠态。
    let mut text = String::new();
    for i in 0..8 {
        text.push_str(&format!("思考第{i}行\n"));
    }
    view.push_line(LineKind::Reasoning, text);
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 折叠态单行：panel 底 + 整行可点（Reasoning hit）。
    assert_eq!(plan.window.len(), 2, "折叠单行 + 留白空行");
    let card_line = &plan.window[0];
    assert!(
        card_line
            .spans
            .iter()
            .any(|s| s.style.bg == Some(theme.panel)),
        "thinking 卡片带 panel 底: {:?}",
        card_line.spans
    );
    let card_text: String = card_line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        card_text.contains("点击展开"),
        "折叠态显示点击展开提示: {card_text:?}"
    );
    assert!(
        matches!(
            &plan.row_hits[0],
            Some((HitTarget::Reasoning(id), end)) if *end > 0
        ),
        "thinking 折叠卡整行可点: {:?}",
        plan.row_hits[0]
    );
    // 留白空行在卡片后（第 2 行）。
    assert_eq!(plan.semantic_rows[1].as_ref().unwrap().text, "");
}

/// §修复：展开态 thinking 逐行 panel 底；折叠提示行带 panel 底。
#[test]
fn thinking_expanded_rows_keep_panel_and_toggle_hint() {
    let mut view = ViewModel {
        collapsed_lines: 6, // 8 行 > 6 → 溢出（展开态有折叠提示行）。
        ..Default::default()
    };
    let mut text = String::new();
    // 8 行（末尾不换行——split('\n') 才恰好 8 段，避免空尾段）。
    for i in 0..7 {
        text.push_str(&format!("思考第{i}行\n"));
    }
    text.push_str("思考第7行");
    view.push_line(LineKind::Reasoning, text);
    let entry_id = view.transcript[0].id();
    view.toggle_reasoning_expanded(entry_id);
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let theme = theme::Theme::omp();
    // 展开态：8 行正文 + 1 折叠提示 + 1 留白 = 10 行。
    assert_eq!(plan.window.len(), 10);
    // 每行（含折叠提示）都带 panel 底。
    for (i, line) in plan.window.iter().enumerate() {
        if line.spans.is_empty() {
            continue; // 留白
        }
        assert!(
            line.spans.iter().any(|s| s.style.bg == Some(theme.panel)),
            "展开行 {i} 必须带 panel 底: {:?}",
            line.spans
        );
    }
    // 折叠提示行（"点击折叠"）带 panel 底。
    let hint_row = plan
        .window
        .iter()
        .find(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            text.contains("点击折叠")
        })
        .expect("展开态必须有折叠提示");
    assert!(
        hint_row
            .spans
            .iter()
            .any(|s| s.style.bg == Some(theme.panel)),
        "折叠提示行带 panel 底"
    );
}

/// §修复：live reasoning 展开态每行都可点击（点正文任意处收起）——
/// 否则展开后无任何 Reasoning hit，点击无法触发收起（“打开后关不上”）。
#[test]
fn live_reasoning_expanded_rows_are_clickable_to_collapse() {
    let mut view = ViewModel::default();
    // 流式 reasoning（live 区，非 transcript）。
    let text = "第一行思考\n第二行思考\n第三行思考";
    view.push_stream_delta(LineKind::Reasoning, text);
    let live_entry = view
        .live
        .reasoning
        .as_ref()
        .expect("live reasoning")
        .entry_id;
    view.toggle_reasoning_expanded(live_entry); // 按条目展开（与工具卡同构）
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let mut content_hits = 0usize;
    for (line, hit) in plan.window.iter().zip(plan.row_hits.iter()) {
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if rendered.contains("行思考") {
            assert!(
                matches!(hit, Some((HitTarget::Reasoning(id), end)) if *id == live_entry && *end > 0),
                "live 展开正文行必须可点（id={live_entry:?}）: {rendered:?} hit={hit:?}"
            );
            content_hits += 1;
        }
    }
    assert_eq!(content_hits, 3, "3 行思考正文都应是 Reasoning hit");
    // 折叠提示行也可点（与历史展开态一致）。
    assert!(
        plan.window
            .iter()
            .zip(plan.row_hits.iter())
            .any(|(line, hit)| {
                let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                rendered.contains("点击折叠") && hit.is_some()
            }),
        "展开态必须有可点击的折叠提示行"
    );
}

/// §修复：卡片内选中一小段 → 只高亮该行（不得整卡全选），
/// 且复制内容与高亮一致（offset 对齐）。
#[test]
fn selecting_part_of_tool_card_highlights_only_that_row() {
    use crate::interaction::TextPosition;
    use crate::scroll::EntryId;
    let mut view = ViewModel {
        collapsed_lines: 10, // 显示内容行（默认 0 折叠）。
        ..Default::default()
    };
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "第一行输出\n第二行输出\n第三行输出",
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    // 定位内容行（含 "一" 的行）的语义 char_start。
    let body_row = plan
        .semantic_rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.as_ref().is_some_and(|s| s.text.starts_with("第一行输出")))
        .map(|(i, r)| (i, r.as_ref().unwrap().char_start))
        .expect("内容行必须有语义");
    let (row_idx, char_start) = body_row;
    // 选中该行前 2 个字符（"第一"）。
    view.selection_start(TextPosition {
        entry_id: EntryId(1),
        offset: char_start,
    });
    view.selection_update(TextPosition {
        entry_id: EntryId(1),
        offset: char_start + 2,
    });
    view.selection_end();
    let mut cache2 = HashMap::new();
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        20,
        0,
        false,
        &mut cache2,
    );
    // 统计带 REVERSED 高亮的行。
    let has_reversed = |line: &Line<'static>| {
        line.spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED))
    };
    let highlighted: Vec<usize> = plan2
        .window
        .iter()
        .enumerate()
        .filter(|(_, l)| has_reversed(l))
        .map(|(i, _)| i)
        .collect();
    assert!(
        highlighted.len() == 1 && highlighted[0] == row_idx,
        "选中内容行中段只应高亮该行，实际 {:?}（目标行 {row_idx}）",
        highlighted
    );
    // 复制内容 = "第一"（offset 对齐：渲染 char_start == selected_text 同 offset）。
    assert_eq!(view.selected_text(), "第一", "复制内容必须与高亮一致");
}

/// P0-1：失败卡片折叠态只显示 tail 尾部 ≤4 行——selected_text 必须与**可见行**
/// 对齐（此前用全量 body，选中可见行 offset 与复制内容错位）。
/// 验证：在可见 tail 行内选中一小段，复制内容与语义文本同窗口 offset 精确。
#[test]
fn failed_tool_card_tail_window_selection_aligns_with_visible_rows() {
    use crate::interaction::TextPosition;
    use crate::scroll::EntryId;
    // 10 行输出，失败：折叠态渲染只显示末尾 4 行（FAILED_LINES）。
    let output = (0..10)
        .map(|i| format!("第{i}行输出"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut view = ViewModel {
        collapsed_lines: 0, // 折叠态：失败卡只显示 tail 尾部 ≤4 行。
        ..Default::default()
    };
    view.begin_tool("c1", "bash", Some("cmd".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Failed,
        10,
        Some(1),
        output.clone(),
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    // 可见行只有末尾 4 行（第6..第9行输出）+ 主行；前面 6 行不可见。
    let visible_rows: Vec<String> = plan
        .semantic_rows
        .iter()
        .filter_map(|r| r.as_ref().map(|s| s.text.clone()))
        .filter(|t| !t.is_empty())
        .collect();
    assert!(
        visible_rows.iter().any(|t| t.contains("第9行输出")),
        "失败 tail 必须包含最后一行: {visible_rows:?}"
    );
    assert!(
        visible_rows.iter().all(|t| !t.contains("第0行输出")),
        "失败 tail 不得包含首行: {visible_rows:?}"
    );
    // 找到可见的 "第9行输出" 语义行，选中其中 "输出" 两个字。
    let (_, tail_row) = plan
        .semantic_rows
        .iter()
        .enumerate()
        .find_map(|(i, r)| {
            r.as_ref()
                .and_then(|s| s.text.contains("第9行输出").then_some((i, s.char_start)))
        })
        .expect("第9行输出必须在可见窗口");
    let line = "第9行输出";
    let prefix = tail_row; // char_start
    // offset 是**字符**偏移（非字节）："第9行输出" 中 "输出" 在 char 3。
    let char_off = line.chars().take_while(|c| *c != '输').count();
    view.selection_start(TextPosition {
        entry_id: EntryId(1),
        offset: prefix + char_off,
    });
    view.selection_update(TextPosition {
        entry_id: EntryId(1),
        offset: prefix + char_off + 2,
    });
    view.selection_end();
    assert_eq!(
        view.selected_text(),
        "输出",
        "复制必须与可见 tail 行精确对齐"
    );
}

/// §用户诉求：默认 collapsed_lines=0 → 工具卡片折叠态只显示主行，
/// 不显示正文，也不显示「点击展开」提示（干净的主行摘要）。
#[test]
fn tool_card_default_zero_collapses_to_main_row_only() {
    let theme = theme::Theme::omp();
    let card = ToolCard {
        id: "c".into(),
        name: "bash".into(),
        target: None,
        command: None,
        state: ToolCardState::Done {
            status: tpi_core::outcome::ToolStatus::Succeeded,
            duration_ms: 10,
            exit_code: Some(0),
        },
        output: Some("line1\nline2\nline3\n".into()),
        diff: None,
        output_truncated: false,
        expanded: false,
        line_number_start: None,
        collapsed_lines: 0,
        started_at_ms: None,
        tail: None,
    };
    let lines = tool_card_lines(&card, 0, theme, 100);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        !text.contains("line1") && !text.contains("line2"),
        "collapsed_lines=0 折叠态不显示正文: {text:?}"
    );
    assert!(
        !text.contains("点击展开") && !text.contains("点击折叠"),
        "collapsed_lines=0 折叠态只显示主行（无提示）: {text:?}"
    );
}

/// §用户诉求（紧凑）：collapsed_lines==0 且未展开的工具卡片之间取消间隔
/// （只显示主行）；展开态卡片保留间隔（多行块需要分隔）。
#[test]
fn zero_collapsed_tool_cards_have_no_gap_between() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "out1",
        None,
    );
    view.begin_tool("c2", "read", Some("file.rs".into()), None);
    view.finish_tool(
        ("c2", "read"),
        tpi_core::outcome::ToolStatus::Succeeded,
        5,
        Some(0),
        "out2",
        None,
    );
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    assert!(
        plan.window.iter().all(|l| !l.spans.is_empty()),
        "两张 0 折叠未展开卡之间无空行（紧凑）: {:?}",
        plan.window
            .iter()
            .map(|l| l
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        plan.window.len(),
        2,
        "只主行、无留白空行: {:?}",
        plan.window
    );

    // 展开第一张 → 多行块，其后保留间隔（空行）以分隔。
    view.toggle_expand("c1");
    let mut cache2 = HashMap::new();
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        20,
        0,
        false,
        &mut cache2,
    );
    assert!(
        plan2.window.iter().any(|l| l.spans.is_empty()),
        "展开卡后保留间隔空行: {:?}",
        plan2
            .window
            .iter()
            .map(|l| l
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>())
            .collect::<Vec<_>>()
    );
}

/// §用户诉求：thinking 用 markdown 渲染——展开后代码块有语法高亮
/// （前景色；§用户诉求：代码块不加背景）。
#[test]
fn thinking_expanded_renders_markdown_code_highlight() {
    let mut view = ViewModel {
        collapsed_lines: 10,
        ..Default::default()
    };
    view.push_line(LineKind::Reasoning, "先想一下\n```rust\nlet x = 1;\n```");
    let entry_id = view.transcript[0].id();
    view.toggle_reasoning_expanded(entry_id);
    let mut cache = HashMap::new();
    let theme = theme::Theme::omp();
    let plan = plan_window_simple(&mut view, theme, 80, 20, 0, false, &mut cache);
    let text: String = plan
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("先想一下") && text.contains("let x = 1"),
        "thinking 展开显示 md 内容: {text:?}"
    );
    // §用户诉求：代码块只做颜色变化——不得带 surface_subtle 背景。
    assert!(
        plan.window.iter().all(|l| {
            l.spans
                .iter()
                .all(|s| s.style.bg != Some(theme.surface_subtle))
        }),
        "代码块不得带高亮背景: {:?}",
        plan.window
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.style.bg.map(|b| (s.content.as_ref(), b)))
            .collect::<Vec<_>>()
    );
}

/// §用户诉求：思考卡片 / AI 输出卡片始终与其他卡片保持一格间隔——
/// 紧凑工具卡后若紧接 thinking 卡，强制补间隔（不再紧贴）。
#[test]
fn compact_tool_card_keeps_gap_before_thinking() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "out1",
        None,
    );
    view.push_line(LineKind::Reasoning, "先分析再动手");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    // 行序：工具卡主行、空行、thinking 折叠行、空行。
    assert!(
        plan.window.len() >= 4,
        "紧凑工具卡 + thinking 必须有间隔: {:?}",
        plan.window
    );
    assert!(
        plan.window[1].spans.is_empty(),
        "工具卡与 thinking 之间必须有一格空行: {:?}",
        plan.window[1]
    );
    let thinking_text: String = plan.window[2]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        thinking_text.contains("◆ 思考"),
        "第三行应是 thinking 折叠行: {thinking_text:?}"
    );
}

/// §用户诉求：紧凑工具卡后若紧接 AI 输出（assistant）卡，强制补间隔。
#[test]
fn compact_tool_card_keeps_gap_before_assistant() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "out1",
        None,
    );
    view.push_line(LineKind::Assistant, "这是回复");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    assert!(
        plan.window.len() >= 3 && plan.window[1].spans.is_empty(),
        "工具卡与 assistant 之间必须有一格空行: {:?}",
        plan.window
    );
}

/// BUG 回归：思考卡“展开后关不上”——live 流式时展开（按条目），
/// finalize 后 id 沿用，再次点击必须能收起（单套按条目状态，无全局 fallback）。
#[test]
fn reasoning_expand_then_finalize_then_collapse_works() {
    let mut view = ViewModel::default();
    // 流式 reasoning（live 区）。
    view.push_stream_delta(LineKind::Reasoning, "第一行思考\n第二行思考");
    let live_entry = view
        .live
        .reasoning
        .as_ref()
        .expect("live reasoning")
        .entry_id;
    // 点击展开（与 reducer ClickReasoning 一致：统一 toggle_reasoning_expanded）。
    view.toggle_reasoning_expanded(live_entry);
    assert!(
        view.is_reasoning_expanded(live_entry),
        "live 展开后为展开态"
    );

    // finalize：live → transcript，entry_id 沿用。
    view.finalize_streaming();
    assert_eq!(view.transcript.len(), 1, "思考卡已提交到 transcript");
    let finalized_id = view.transcript[0].id();
    assert_eq!(finalized_id, live_entry, "finalize 沿用同一 id");

    // 渲染：展开态（正文可见）。
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let text: String = plan
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(text.contains("第一行思考"), "finalize 后仍展开: {text:?}");

    // 再次点击（同一 id）→ 必须能收起。
    view.toggle_reasoning_expanded(finalized_id);
    assert!(
        !view.is_reasoning_expanded(finalized_id),
        "再次点击后必须能收起（展开后关不上）"
    );
    let mut cache2 = HashMap::new();
    let plan2 = plan_window_simple(
        &mut view,
        theme::Theme::omp(),
        80,
        20,
        0,
        false,
        &mut cache2,
    );
    let text2: String = plan2
        .window
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(
        text2.contains("点击展开") && !text2.contains("第一行思考"),
        "收起后显示折叠摘要: {text2:?}"
    );
}

/// BUG 回归：历史思考卡展开后点击能稳定收起（不依赖任何全局状态）。
#[test]
fn historical_reasoning_toggle_is_stable() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::Reasoning, "第一行思考\n第二行思考");
    let id = view.transcript[0].id();
    // 展开 → 收起 → 展开 → 收起，每次点击都有效。
    for _ in 0..2 {
        view.toggle_reasoning_expanded(id);
        assert!(view.is_reasoning_expanded(id), "展开态");
        view.toggle_reasoning_expanded(id);
        assert!(!view.is_reasoning_expanded(id), "收起态");
    }
    // Alt+T 全量切换：展开所有、再收起所有。
    view.toggle_all_reasoning();
    assert!(view.is_reasoning_expanded(id), "Alt+T 全部展开");
    view.toggle_all_reasoning();
    assert!(!view.is_reasoning_expanded(id), "Alt+T 全部收起");
}

/// §用户诉求：live 区 AI 输出卡与运行中工具卡之间保持一格间隔。
#[test]
fn live_assistant_keeps_gap_before_running_tool_card() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.append_tool_output("c1", "out1");
    view.push_stream_delta(LineKind::Assistant, "流式输出");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let texts: Vec<String> = plan
        .semantic_rows
        .iter()
        .map(|r| r.as_ref().map(|s| s.text.clone()).unwrap_or_default())
        .collect();
    assert_eq!(texts[0], "流式输出", "首行是 AI 输出: {texts:?}");
    assert_eq!(texts[1], "", "AI 输出与工具卡之间必须有一格空行: {texts:?}");
    assert!(!texts[2].is_empty(), "第三行是工具卡主行: {texts:?}");
}

/// §用户诉求：live 区思考卡与 AI 输出卡之间保持一格间隔。
#[test]
fn live_reasoning_keeps_gap_before_assistant() {
    let mut view = ViewModel {
        collapsed_lines: 10, // 单行 thinking 不折叠，正文可复制。
        ..Default::default()
    };
    view.push_stream_delta(LineKind::Reasoning, "思考中");
    let live_entry = view
        .live
        .reasoning
        .as_ref()
        .expect("live reasoning")
        .entry_id;
    view.toggle_reasoning_expanded(live_entry); // 展开 live thinking
    view.push_stream_delta(LineKind::Assistant, "回答");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    let texts: Vec<String> = plan
        .semantic_rows
        .iter()
        .map(|r| r.as_ref().map(|s| s.text.clone()).unwrap_or_default())
        .collect();
    let think_idx = texts
        .iter()
        .position(|t| t == "思考中")
        .unwrap_or_else(|| panic!("思考行必须存在: {texts:?}"));
    let answer_idx = texts
        .iter()
        .position(|t| t == "回答")
        .unwrap_or_else(|| panic!("AI 输出行必须存在: {texts:?}"));
    assert!(think_idx < answer_idx, "思考在前: {texts:?}");
    assert!(
        texts[think_idx + 1..answer_idx]
            .iter()
            .any(|t| t.is_empty()),
        "思考卡与 AI 输出之间必须有一格空行: {texts:?}"
    );
}

/// §用户诉求：历史末尾紧凑工具卡与 live 思考卡之间保持一格间隔。
#[test]
fn history_tail_compact_tool_keeps_gap_before_live_reasoning() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "out1",
        None,
    );
    view.push_stream_delta(LineKind::Reasoning, "思考中");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    // 行序：工具卡主行、空行、live thinking 折叠行（live 末尾无 trailing gap）。
    assert!(
        plan.window.len() >= 3 && plan.window[1].spans.is_empty(),
        "历史紧凑工具卡与 live thinking 之间必须有一格空行: {:?}",
        plan.window
    );
}

/// BUG 回归：历史末尾紧凑工具卡与 live thinking 之间的空行被 wrap_cache 吞掉。
///
/// 时序：thinking 开始流式前，历史末尾工具卡的 wrap 结果（无 gap）被缓存；
/// thinking 流式开始后需要补 gap，但 wrap_cache 命中旧行，思考卡顶部缺空行。
/// §修复：历史末尾与 live 首组的间隔改由 build_live_group 在 live 首组前补
/// （live 组不缓存），历史 wrap 结果不再依赖 live 状态。
#[test]
fn live_thinking_keeps_gap_across_wrap_cache() {
    let mut view = ViewModel::default();
    view.push_line(LineKind::User, "问题");
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.finish_tool(
        ("c1", "bash"),
        tpi_core::outcome::ToolStatus::Succeeded,
        10,
        Some(0),
        "out1",
        None,
    );
    let mut cache = HashMap::new();
    let mut wrap_cache: HashMap<(EntryId, u16), Arc<Vec<WrappedRow>>> = HashMap::new();
    // 帧 1：无 live 内容 → 历史末尾工具卡被缓存（无 gap）。
    plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        40,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    // 帧 2：thinking 开始流式（复用同一 wrap_cache，模拟跨帧）。
    view.push_stream_delta(LineKind::Reasoning, "思考中");
    let plan = plan_window(
        &mut view,
        theme::Theme::omp(),
        80,
        40,
        0,
        false,
        &mut cache,
        &mut wrap_cache,
    );
    let window_text: Vec<String> = plan
        .window
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let think_idx = window_text
        .iter()
        .position(|t| t.contains("思考"))
        .unwrap_or_else(|| panic!("live thinking 折叠行必须存在: {window_text:?}"));
    assert!(think_idx >= 2, "思考行前应有工具卡+空行: {window_text:?}");
    assert!(
        window_text[think_idx - 1].trim().is_empty(),
        "思考卡顶部必须有一格空行（跨 wrap_cache 复用后仍有效）: {window_text:?}"
    );
    assert!(
        window_text[think_idx - 2].contains("bash"),
        "空行上方应是工具卡主行: {window_text:?}"
    );
}

/// §用户诉求（回归）：live 区两张紧凑工具卡之间仍保持紧凑（无空行）。
#[test]
fn live_compact_tool_cards_have_no_gap_between() {
    let mut view = ViewModel::default();
    view.begin_tool("c1", "bash", Some("cmd1".into()), None);
    view.append_tool_output("c1", "out1");
    view.begin_tool("c2", "read", Some("file.rs".into()), None);
    view.append_tool_output("c2", "out2");
    let mut cache = HashMap::new();
    let plan = plan_window_simple(&mut view, theme::Theme::omp(), 80, 20, 0, false, &mut cache);
    assert!(
        plan.window.iter().all(|l| !l.spans.is_empty()),
        "两张紧凑 live 工具卡之间无空行: {:?}",
        plan.window
    );
}

/// P8：/model 菜单 Enter → pending_model 设置 + 菜单关闭（app 执行切换）。
#[test]
fn model_menu_enter_sets_pending_model() {
    let mut state = UiState::new(ViewModel::default());
    // 挂载模型菜单（app 的 /model 分支等价构造）。
    state.view.menu = Some(crate::model::MenuView {
        items: vec![
            ("gpt-4o".to_string(), "openai（当前）".to_string()),
            ("claude-sonnet".to_string(), "anthropic".to_string()),
        ],
        selected: 1,
        kind: crate::model::MenuKind::Model,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    state.view.modal = Some(crate::model::ModalState::new(
        "/model",
        "当前模型".to_string(),
    ));
    let effects = reducer::update(
        &mut state,
        UiEvent::Key(key_event(ratatui::crossterm::event::KeyCode::Enter)),
    );
    assert_eq!(
        state.pending_model.as_deref(),
        Some("claude-sonnet"),
        "选中项写入 pending_model"
    );
    assert!(state.view.menu.is_none(), "菜单关闭");
    assert!(state.view.modal.is_none(), "modal 关闭");
    // 菜单 Enter 短路：不产生任何 effect（切换由 app 层消费 pending_model 执行）。
    assert!(effects.is_empty(), "菜单选中不产生 effect");
}

/// 菜单上下移动（↑/↓）不触发 refresh（Model 菜单保持，导航不清除）。
#[test]
fn model_menu_navigation_keeps_menu() {
    let mut state = UiState::new(ViewModel::default());
    state.view.menu = Some(crate::model::MenuView {
        items: vec![
            ("gpt-4o".to_string(), "openai".to_string()),
            ("claude-sonnet".to_string(), "anthropic".to_string()),
        ],
        selected: 0,
        kind: crate::model::MenuKind::Model,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    let _ = reducer::update(
        &mut state,
        UiEvent::Key(key_event(ratatui::crossterm::event::KeyCode::Down)),
    );
    assert!(state.view.menu.is_some(), "模型菜单导航后保持");
    assert_eq!(state.view.menu.as_ref().unwrap().selected, 1);
}

/// §oh-my-pi（type-to-filter）：Modal 型菜单（/model）打开时，字符键进菜单
/// 过滤（不落 composer）、Backspace 删除过滤词、过滤后导航用过滤列表。
#[test]
fn browser_menu_type_to_filter() {
    let mut state = UiState::new(ViewModel::default());
    state.view.menu = Some(crate::model::MenuView {
        items: vec![
            ("gpt-4o".to_string(), "openai".to_string()),
            ("claude-sonnet".to_string(), "anthropic".to_string()),
            ("deepseek-v3".to_string(), "deepseek".to_string()),
        ],
        selected: 0,
        kind: crate::model::MenuKind::Model,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    // 字符键 → 进 filter（不落 composer）。
    // Modal 型菜单在真实路径与 modal 同开（/model 的 open_modal）→ blocking。
    state.view.modal = Some(crate::model::ModalState::new("/model", String::from("x")));
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char('c'))));
    let menu = state.view.menu.as_ref().unwrap();
    assert_eq!(menu.filter, "c", "字符进过滤词");
    assert_eq!(menu.filtered_len(), 1, "过滤后只剩 claude");
    assert!(state.view.input.is_empty(), "不落 composer");
    // 过滤后 Enter → 选中过滤后的项。
    let effects = reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Enter)));
    assert_eq!(state.pending_model.as_deref(), Some("claude-sonnet"));
    assert!(effects.is_empty());
}

/// §oh-my-pi（type-to-filter）：Backspace 删除过滤词；Esc 先清空过滤词（不关
/// 菜单），再按 Esc 才关闭菜单。
#[test]
fn browser_menu_filter_backspace_and_esc_clears_first() {
    let mut state = UiState::new(ViewModel::default());
    state.view.menu = Some(crate::model::MenuView {
        items: vec![("gpt-4o".to_string(), "openai".to_string())],
        selected: 0,
        kind: crate::model::MenuKind::Model,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    state.view.modal = Some(crate::model::ModalState::new("/model", String::from("x")));
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char('g'))));
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char('p'))));
    assert_eq!(state.view.menu.as_ref().unwrap().filter, "gp");
    // Backspace 删除末字符。
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Backspace)));
    assert_eq!(state.view.menu.as_ref().unwrap().filter, "g");
    // Esc：过滤词非空 → 先清空，菜单保持。
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Esc)));
    assert!(state.view.menu.is_some(), "Esc 先清过滤词不关菜单");
    assert!(state.view.menu.as_ref().unwrap().filter.is_empty());
    // 再 Esc：过滤已空 → 关闭菜单。
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Esc)));
    assert!(state.view.menu.is_none(), "二次 Esc 关闭菜单");
}

/// §oh-my-pi（type-to-filter）：命令/文件菜单（非 Modal 型）字符键仍走
/// composer（过滤由输入框驱动），filter 保持为空。
#[test]
fn command_menu_typing_goes_to_composer() {
    let mut state = UiState::new(ViewModel::default());
    state.view.menu = Some(crate::model::MenuView {
        items: vec![("help".to_string(), "帮助".to_string())],
        selected: 0,
        kind: crate::model::MenuKind::SlashCommand,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    // 命令菜单没有 modal → 不 blocking → 字符键落 composer（走 TypedChar）。
    // 输入 'x'（无 / 前缀）后 refresh_command_menu 会关闭菜单（既有行为：
    // 命令菜单由输入驱动）——这正说明命令菜单不走内部 filter。
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char('x'))));
    assert_eq!(state.view.input, "x", "字符进输入框（composer 过滤）");
    assert!(
        state.view.menu.as_ref().is_none_or(|m| m.filter.is_empty()),
        "命令菜单 filter 恒空（输入驱动，无内部过滤）"
    );
}

/// §oh-my-pi（type-to-filter）：过滤无匹配时 Enter 不提交（selected_menu_item
/// 返回 None）；菜单保持打开，用户可清过滤或改词。
#[test]
fn browser_menu_no_match_enter_does_nothing() {
    let mut state = UiState::new(ViewModel::default());
    state.view.menu = Some(crate::model::MenuView {
        items: vec![("gpt-4o".to_string(), "openai".to_string())],
        selected: 0,
        kind: crate::model::MenuKind::Model,
        session_previews: Vec::new(),
        filter: String::new(),
    });
    state.view.modal = Some(crate::model::ModalState::new("/model", String::from("x")));
    reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Char('z')))); // 无匹配
    assert_eq!(state.view.menu.as_ref().unwrap().filtered_len(), 0);
    let effects = reducer::update(&mut state, UiEvent::Key(key_event(KeyCode::Enter)));
    assert!(state.pending_model.is_none(), "无匹配不提交");
    assert!(state.view.menu.is_some(), "菜单保持打开");
    assert!(effects.is_empty());
}

/// 构造简单 Key 事件（reducer 不检查 kind；app 层已过滤）。
fn key_event(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// P8-06：SubagentReported 投影为系统行（summary + evidence）。
#[test]
fn subagent_reported_projects_to_system_line() {
    let mut state = UiState::new(ViewModel::default());
    let _ = reducer::update(
        &mut state,
        UiEvent::Agent(tpi_agent::agent::RuntimeEvent::SubagentReported {
            child_session: tpi_core::ids::SessionId::from_u128(1),
            summary: "发现 3 处可疑调用".to_string(),
            evidence: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
        }),
    );
    // 投影为系统行（transcript 内 Message 行）。
    let texts: Vec<String> = state
        .view
        .transcript
        .iter()
        .filter_map(|e| match e {
            crate::model::Entry::Message { line, .. } => Some(line.text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("子代理调查完成") && t.contains("发现 3 处可疑调用")),
        "系统行含 summary: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("src/main.rs") && t.contains("src/lib.rs")),
        "系统行含 evidence: {texts:?}"
    );
}

/// P13（opencode 形态）request_input 交互模态：单选/多选/多问题/自定义/拒绝。
mod question_modal_tests {
    use crate::effect::UiEffect;
    use crate::event::UiEvent;
    use crate::model::{
        QuestionModalState, QuestionMode, QuestionOptionView, QuestionView, ViewModel,
    };
    use crate::reducer;
    use crate::state::UiState;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn single_q() -> Vec<QuestionView> {
        vec![QuestionView {
            question: "发布到哪个环境？".into(),
            header: Some("部署".into()),
            options: vec![
                QuestionOptionView {
                    label: "生产".into(),
                    description: "线上环境".into(),
                },
                QuestionOptionView {
                    label: "staging".into(),
                    description: "预发".into(),
                },
            ],
            multiple: false,
            custom: true,
        }]
    }

    fn state_with(q: Vec<QuestionView>) -> UiState {
        let mut state = UiState::new(ViewModel::default());
        state.view.question = Some(QuestionModalState::new(q));
        state
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn single_select_submits_label() {
        let mut state = state_with(single_q());
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        assert!(effects.is_empty(), "移动不产生 effect");
        assert_eq!(state.view.question.as_ref().unwrap().selected, 1);
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert_eq!(effects.len(), 1, "Enter 提交");
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => {
                assert!(text.contains("staging"), "选中第 2 项提交: {text}")
            }
            other => panic!("期望 QuestionSubmitted，实际 {other:?}"),
        }
    }

    #[test]
    fn digit_select_submits() {
        let mut state = state_with(single_q());
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char('1'))));
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => assert!(text.contains("生产"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn escape_rejects() {
        let mut state = state_with(single_q());
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Esc)));
        assert_eq!(effects, vec![UiEffect::QuestionRejected]);
        assert!(state.view.question.as_ref().unwrap().rejected);
    }

    #[test]
    fn multiple_toggles() {
        let mut state = state_with(vec![QuestionView {
            question: "选框架？".into(),
            header: None,
            options: vec![
                QuestionOptionView {
                    label: "react".into(),
                    description: String::new(),
                },
                QuestionOptionView {
                    label: "vue".into(),
                    description: String::new(),
                },
            ],
            multiple: true,
            custom: false,
        }]);
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert_eq!(
            state.view.question.as_ref().unwrap().answers[0],
            vec!["react", "vue"],
            "多选两个"
        );
        // 再 Enter 取消第一个。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Up)));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert_eq!(
            state.view.question.as_ref().unwrap().answers[0],
            vec!["vue"],
            "取消 react"
        );
    }

    #[test]
    fn multi_question_tabs_and_review() {
        let mut state = state_with(vec![
            QuestionView {
                question: "Q1".into(),
                header: Some("一".into()),
                options: vec![QuestionOptionView {
                    label: "a".into(),
                    description: String::new(),
                }],
                multiple: false,
                custom: false,
            },
            QuestionView {
                question: "Q2".into(),
                header: Some("二".into()),
                options: vec![QuestionOptionView {
                    label: "b".into(),
                    description: String::new(),
                }],
                multiple: false,
                custom: false,
            },
        ]);
        // Q1 选 a → 自动进 Q2 tab。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert_eq!(state.view.question.as_ref().unwrap().tab, 1, "进 Q2");
        // Q2 选 b → Review。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert_eq!(
            state.view.question.as_ref().unwrap().mode,
            QuestionMode::Review,
            "全答完进 Review"
        );
        // Review Enter → 提交（含两个问题答案）。
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => {
                assert!(text.contains("Q1: a") && text.contains("Q2: b"), "{text}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn custom_edit_submits_text() {
        let mut state = state_with(single_q());
        // 选中自定义项（最后一项）。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert_eq!(
            state.view.question.as_ref().unwrap().mode,
            QuestionMode::EditingCustom,
            "自定义项进入编辑"
        );
        for c in "我的环境".chars() {
            reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char(c))));
        }
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => assert!(text.contains("我的环境"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    /// §askuser 修复：Selecting 模式直接打字即进入自定义回答（此前字符键
    /// 全部被拦截——单问题/有选项时用户直接输入没有任何反应）。
    #[test]
    fn typing_enters_custom_edit_directly() {
        let mut state = state_with(single_q()); // custom = true
        // 不打导航键/数字键，直接打字符：进入编辑并插入字符。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char('我'))));
        let q = state.view.question.as_ref().unwrap();
        assert_eq!(q.mode, QuestionMode::EditingCustom, "字符键直接进编辑");
        assert_eq!(q.custom_input, "我");
        for c in "的环境".chars() {
            reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char(c))));
        }
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => assert!(text.contains("我的环境"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    /// custom=false 时字符键仍被拦截（只能选选项，不能输入自定义文本）。
    #[test]
    fn typing_blocked_when_custom_disabled() {
        let mut state = state_with(vec![QuestionView {
            question: "确认删除？".into(),
            header: None,
            options: vec![QuestionOptionView {
                label: "是".into(),
                description: String::new(),
            }],
            multiple: false,
            custom: false,
        }]);
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char('x'))));
        let q = state.view.question.as_ref().unwrap();
        assert_eq!(q.mode, QuestionMode::Selecting, "custom=false 时字符键拦截");
        assert!(q.custom_input.is_empty());
    }

    #[test]
    fn multiple_single_question_done_item_submits() {
        // multiple + custom=false：选两个后到“完成”项 Enter 提交。
        let mut state = state_with(vec![QuestionView {
            question: "选框架？".into(),
            header: None,
            options: vec![
                QuestionOptionView {
                    label: "react".into(),
                    description: String::new(),
                },
                QuestionOptionView {
                    label: "vue".into(),
                    description: String::new(),
                },
            ],
            multiple: true,
            custom: false,
        }]);
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // 选 react
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // 选 vue
        // 到“完成”项（第 3 项）。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        assert!(state.view.question.as_ref().unwrap().on_done(), "在完成项");
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => {
                assert!(text.contains("react") && text.contains("vue"), "{text}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// §13 修复：多问题 Review 页未全部回答时 Enter 不提交（模型不能收到
    /// 空答案）；模态保持 Review，用户可返回补答或 Esc 拒绝。
    #[test]
    fn review_with_unanswered_question_does_not_submit() {
        let mut state = state_with(vec![
            QuestionView {
                question: "Q1".into(),
                header: Some("一".into()),
                options: vec![QuestionOptionView {
                    label: "a".into(),
                    description: String::new(),
                }],
                multiple: false,
                custom: false,
            },
            QuestionView {
                question: "Q2".into(),
                header: Some("二".into()),
                options: vec![QuestionOptionView {
                    label: "b".into(),
                    description: String::new(),
                }],
                multiple: false,
                custom: false,
            },
        ]);
        // 答 Q1 → 自动进 Q2 tab。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        // 不答 Q2，直接用 Tab 推进到 Review（Tab 可跳过未答问题）。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Tab)));
        assert_eq!(
            state.view.question.as_ref().unwrap().mode,
            QuestionMode::Review,
            "Tab 可进入 Review（即使 Q2 未答）"
        );
        assert!(
            !state.view.question.as_ref().unwrap().all_answered(),
            "Q2 未答"
        );
        // §askuser 修复：Review Enter 未全答 → 不提交，跳到第一个未答问题
        // 并给出明确提示（此前静默拦截，用户以为已提交、看不到任何反应）。
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert!(effects.is_empty(), "未全答时 Enter 不得提交: {effects:?}");
        assert_eq!(
            state.view.question.as_ref().unwrap().mode,
            QuestionMode::Selecting,
            "未全答时跳回编辑页而非保持 Review"
        );
        assert_eq!(
            state.view.question.as_ref().unwrap().tab,
            1,
            "跳到第一个未答问题 Q2"
        );
        assert!(
            state
                .view
                .transient_hint
                .as_deref()
                .is_some_and(|h| h.contains("未回答")),
            "给出未答提示"
        );
        // 已在 Q2 编辑页：直接答 Q2 → 自动进 Review → 提交。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // 答 Q2
        assert!(state.view.question.as_ref().unwrap().all_answered());
        // 答完自动进 Review（advance_tab 到末尾）。
        assert_eq!(
            state.view.question.as_ref().unwrap().mode,
            QuestionMode::Review
        );
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert!(
            matches!(&effects[0], UiEffect::QuestionSubmitted(text) if text.contains("Q2: b")),
            "补答后应提交: {effects:?}"
        );
    }

    /// §13 修复：无选项且 custom=false 的问题在模态里必须有输入通道
    ///（app 投影层强制 custom；这里验证 reducer 对无选项 custom 问题的
    /// 编辑路径可用）。
    #[test]
    fn optionless_question_enters_custom_edit() {
        let mut state = state_with(vec![QuestionView {
            question: "随便说点什么".into(),
            header: None,
            options: Vec::new(),
            multiple: false,
            custom: true,
        }]);
        // 无选项：唯一一项就是自定义项，Enter 直接进入编辑。
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert!(effects.is_empty());
        assert_eq!(
            state.view.question.as_ref().unwrap().mode,
            QuestionMode::EditingCustom,
            "无选项问题 Enter 应进入自定义编辑"
        );
        for c in "我的回答".chars() {
            reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char(c))));
        }
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        assert!(
            matches!(&effects[0], UiEffect::QuestionSubmitted(text) if text.contains("我的回答")),
            "{effects:?}"
        );
    }

    /// §bug 修复：multiple + custom=true（默认投影）时必须有“完成”项可提交——
    /// 否则用户勾选后无法确定（Enter 只 toggle、自定义编辑后也不提交），
    /// 只能 Esc 拒绝。完成项位于自定义项之后（index = options.len()+1）。
    #[test]
    fn multiple_with_custom_has_done_item() {
        let mut state = state_with(vec![QuestionView {
            question: "选框架？".into(),
            header: None,
            options: vec![
                QuestionOptionView {
                    label: "react".into(),
                    description: String::new(),
                },
                QuestionOptionView {
                    label: "vue".into(),
                    description: String::new(),
                },
            ],
            multiple: true,
            custom: true,
        }]);
        assert_eq!(
            state.view.question.as_ref().unwrap().option_count(),
            4,
            "2 选项 + 自定义 + 完成"
        );
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // 选 react
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // 选 vue
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down))); // 自定义项
        assert!(
            state.view.question.as_ref().unwrap().on_custom(),
            "第 3 项为自定义项"
        );
        assert!(!state.view.question.as_ref().unwrap().on_done());
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down))); // 完成项
        assert!(
            state.view.question.as_ref().unwrap().on_done(),
            "第 4 项为完成项"
        );
        assert!(
            !state.view.question.as_ref().unwrap().on_custom(),
            "完成项不得误判为自定义项"
        );
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => {
                assert!(text.contains("react") && text.contains("vue"), "{text}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// §bug 修复：多问题 + multiple + custom——非最后一题的完成项进下一 tab
    ///（此前误进 Review 把未答问题一并展示，用户以为要“全部提交”）；
    /// 最后一题的完成项才进 Review，Review Enter 提交全部。
    #[test]
    fn multi_question_multiple_done_goes_review_then_submits() {
        let mk = |q: &str, h: &str| QuestionView {
            question: q.into(),
            header: Some(h.into()),
            options: vec![QuestionOptionView {
                label: "a".into(),
                description: String::new(),
            }],
            multiple: true,
            custom: true,
        };
        let mut state = state_with(vec![mk("Q1", "一"), mk("Q2", "二")]);
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // Q1 选 a
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down))); // Q1 自定义
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down))); // Q1 完成项
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // 完成项
        let q = state.view.question.as_ref().unwrap();
        assert_eq!(q.tab, 1, "非最后一题完成项应进下一 tab Q2");
        assert_eq!(q.mode, QuestionMode::Selecting, "Q2 处于选择态而非 Review");
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // Q2 选 a
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down))); // Q2 自定义
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down))); // Q2 完成项
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter))); // 最后一题 → Review
        assert_eq!(
            state.view.question.as_ref().unwrap().mode,
            QuestionMode::Review,
            "最后一题完成项进 Review"
        );
        let effects = reducer::update(&mut state, UiEvent::Key(key(KeyCode::Enter)));
        match &effects[0] {
            UiEffect::QuestionSubmitted(text) => {
                assert!(text.contains("Q1: a") && text.contains("Q2: a"), "{text}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// §bug 修复：多问题 multiple 完成项被数字快选选中（选项数 + 完成项 ≤ 9）
    /// 也必须进下一 tab，而不是停在 Review（与 Enter 路径一致）。
    #[test]
    fn multi_question_multiple_digit_done_advances_tab() {
        let mk = |q: &str, h: &str| QuestionView {
            question: q.into(),
            header: Some(h.into()),
            options: vec![QuestionOptionView {
                label: "a".into(),
                description: String::new(),
            }],
            multiple: true,
            custom: false,
        };
        let mut state = state_with(vec![mk("Q1", "一"), mk("Q2", "二")]);
        // Q1：1 个选项 + 完成项（custom=false，option_count = 2）。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char('1')))); // 选 a（toggle 停留）
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char('2')))); // 完成项
        let q = state.view.question.as_ref().unwrap();
        assert_eq!(q.tab, 1, "数字快选完成项应进下一 tab");
        assert_eq!(q.mode, QuestionMode::Selecting, "Q2 处于选择态而非 Review");
    }
}

/// §子代理内部视图（opencode 形态）：点击 subagent 卡进入实时观察、←/→
/// 切换、Esc/Backspace 返回、↑↓ 滚动；普通工具卡点击仍展开；折叠预览显示
/// 最新 child 活动。
mod subagent_view_tests {
    use super::*;
    use crate::model::ViewModel;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn subagent_state(ids: &[&str]) -> UiState {
        let mut state = UiState::new(ViewModel::default());
        for id in ids {
            state
                .view
                .begin_tool(*id, "subagent", Some("调查 src/main.rs".into()), None);
        }
        state
    }

    #[test]
    fn click_subagent_card_opens_internal_view() {
        let mut state = subagent_state(&["c1"]);
        reducer::update(&mut state, UiEvent::ClickTool("c1".into()));
        assert_eq!(
            state.view.subagent.active.as_deref(),
            Some("c1"),
            "点击 subagent 卡打开内部视图"
        );
    }

    #[test]
    fn click_normal_tool_still_toggles_expand() {
        let mut state = UiState::new(ViewModel::default());
        state
            .view
            .begin_tool("c1", "bash", Some("echo hi".into()), None);
        reducer::update(&mut state, UiEvent::ClickTool("c1".into()));
        assert!(
            state.view.subagent.active.is_none(),
            "普通工具卡点击不打开内部视图"
        );
        assert!(
            state.view.live.tools.get("c1").unwrap().card.expanded,
            "普通工具卡点击仍展开"
        );
    }

    #[test]
    fn left_right_cycles_between_subagents() {
        let mut state = subagent_state(&["c1", "c2"]);
        state.view.open_subagent("c1".into());
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Right)));
        assert_eq!(state.view.subagent.active.as_deref(), Some("c2"));
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Left)));
        assert_eq!(state.view.subagent.active.as_deref(), Some("c1"));
        // 循环：c1 向左 → c2。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Left)));
        assert_eq!(state.view.subagent.active.as_deref(), Some("c2"));
    }

    #[test]
    fn esc_or_backspace_closes_view() {
        let mut state = subagent_state(&["c1"]);
        state.view.open_subagent("c1".into());
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Esc)));
        assert!(state.view.subagent.active.is_none(), "Esc 返回父代理");
        // Backspace 同样关闭。
        state.view.open_subagent("c1".into());
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Backspace)));
        assert!(state.view.subagent.active.is_none(), "Backspace 返回父代理");
    }

    #[test]
    fn view_scroll_clamps_to_content() {
        let mut state = subagent_state(&["c1"]);
        state.view.open_subagent("c1".into());
        // 多行内容（4 行正文）。
        state
            .view
            .append_tool_output("c1", "第 0 行\n第 1 行\n第 2 行\n第 3 行");
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        assert_eq!(state.view.subagent.scroll, 1);
        // 大量 Down 不越界（4 行 → 最大 scroll 3）。
        for _ in 0..10 {
            reducer::update(&mut state, UiEvent::Key(key(KeyCode::Down)));
        }
        assert_eq!(state.view.subagent.scroll, 3, "滚动 clamp 到内容尾部");
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Up)));
        assert_eq!(state.view.subagent.scroll, 2);
    }

    #[test]
    fn keys_blocked_while_view_open() {
        let mut state = subagent_state(&["c1"]);
        state.view.open_subagent("c1".into());
        // 打字应被拦截（浏览模式，不落 composer）。
        reducer::update(&mut state, UiEvent::Key(key(KeyCode::Char('x'))));
        assert!(
            state.view.input.is_empty(),
            "内部视图打开时按键不落 composer"
        );
    }

    /// §子代理：subagent 卡永不紧凑——两张 subagent 卡之间保持 1 行空行
    ///（多个并行子代理之间分隔清晰；普通紧凑工具卡之间无间隔）。
    #[test]
    fn subagent_cards_keep_gap_between() {
        let mut view = ViewModel::default();
        // 两张折叠的 subagent 卡（带 output → 若按普通工具卡规则会紧凑）。
        view.begin_tool("c1", "subagent", Some("调查 a".into()), None);
        view.append_tool_output("c1", "活动行 1\n活动行 2");
        view.finish_tool(
            ("c1", "subagent"),
            tpi_core::outcome::ToolStatus::Succeeded,
            500,
            None,
            "子代理调查完成",
            None,
        );
        view.begin_tool("c2", "subagent", Some("调查 b".into()), None);
        view.append_tool_output("c2", "活动行 3");
        view.finish_tool(
            ("c2", "subagent"),
            tpi_core::outcome::ToolStatus::Succeeded,
            300,
            None,
            "子代理调查完成",
            None,
        );
        let mut cache = HashMap::new();
        let plan = plan_window_simple(
            &mut view,
            crate::theme::Theme::omp(),
            80,
            30,
            0,
            false,
            &mut cache,
        );
        let texts: Vec<String> = plan
            .window
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // 两个子代理卡之间必须有空行（间隔）。
        let pos1 = texts.iter().position(|t| t.contains("调查 a")).unwrap_or(0);
        let pos2 = texts.iter().position(|t| t.contains("调查 b")).unwrap_or(0);
        assert!(
            pos2 > pos1 && texts[pos1 + 1..pos2].iter().any(|t| t.is_empty()),
            "两张 subagent 卡之间必须保留 1 行空行: {texts:?}"
        );
    }

    /// 卡片折叠预览：subagent 卡折叠态显示最新 2 行 child 活动。
    #[test]
    fn subagent_card_collapsed_preview_shows_latest_two_lines() {
        let theme = crate::theme::Theme::omp();
        let card = crate::model::ToolCard {
            id: "c1".into(),
            name: "subagent".into(),
            target: Some("调查 src".into()),
            command: None,
            state: ToolCardState::Done {
                status: tpi_core::outcome::ToolStatus::Succeeded,
                duration_ms: 1200,
                exit_code: None,
            },
            output: Some("第 0 行\n第 1 行\n第 2 行\n第 3 行\n第 4 行".into()),
            diff: None,
            output_truncated: false,
            expanded: false,
            tail: None,
            line_number_start: None,
            collapsed_lines: 0,
            started_at_ms: None,
        };
        let lines = crate::tool_card::tool_card_lines(&card, 0, theme, 100);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            text.contains("第 3 行") && text.contains("第 4 行"),
            "折叠预览显示最新 2 行: {text:?}"
        );
        assert!(
            !text.contains("第 0 行") && !text.contains("第 1 行") && !text.contains("第 2 行"),
            "折叠预览不含旧行: {text:?}"
        );
    }
}
