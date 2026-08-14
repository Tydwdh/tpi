//! 应用层：输入路由、会话选择与交互生命周期。
//!
//! 活动 run 中的输入路由（§6.2）：
//! - 普通 Enter：输入排队（pending_message），**当前 run 完成后**作为下一条
//!   消息提交（§12 稳定化任务书：不是 run 内的 boundary steering）；
//! - Esc：取消当前 run，保留 session（Ctrl-C 只用于复制，见 reducer）。
//!
//! M5：Ratatui inline renderer——只有 renderer 写 stdout（§3.2 不变量 11、
//! §16.1），FRAME_INTERVAL 帧合并，synchronized update。
//! M6+：键盘由独立线程持续读取（运行中也响应输入/翻页/折叠），
//! 命令补全菜单（Tab）、输入历史（↑/↓）、多行输入（Alt+Enter）、
//! 思考折叠（Alt+T）、动画时钟（spinner）。

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent;
use crate::config::Config;
use crate::ids::SessionId;
use crate::provider::openai_compat::OpenAiCompatClient;
use crate::provider::{ChatMessage, Provider};
use crate::session::conversation::Conversation;
use crate::session::{self, SessionEvent, SessionLog};
use crate::tui::effect::UiEffect;
use crate::tui::state::UiState;
use crate::tui::{Renderer, model::LineKind, model::StatusLine, model::ViewModel};
use ratatui::crossterm::event::Event;

/// P0-4：系统消息 + 立即重绘。
/// slash 命令输出此前 push 后直接 continue 不 draw，用户看不到任何反馈，
/// 直到下一次键盘事件触发重绘。
fn push_system_line(
    view: &mut ViewModel,
    renderer: &mut Renderer,
    text: String,
) -> Result<(), String> {
    view.push_line(LineKind::System, text);
    renderer.draw(view).map_err(|e| e.to_string())
}

/// 会话定位方式（§18.3）。
pub enum SessionTarget {
    New,
    Continue,
    Resume(String),
}

/// slash 命令分派结果：`interactive_loop` 据其短路 run 路径。
enum SlashAction {
    /// 退出交互循环（/quit、/exit）。
    Quit,
    /// 已作为命令消费，跳过 run（continue）。
    Consumed,
    /// 不是 slash 命令，按普通消息处理。
    NotCommand,
}

#[derive(Debug, Clone)]
struct RetryTarget {
    message: String,
    reason: crate::session::CompletionReason,
}

/// 键盘线程到 UI 主循环的输入。crossterm 事件保持原样；仅旧终端逐键粘贴
/// 达到长文本阈值时增加两个语义事件，用于折叠及补齐旁路全文。
enum TerminalInput {
    Event(Event),
    CollapseKeyStreamPaste {
        rendered_suffix: String,
        text: String,
    },
    FinishKeyStreamPaste(crate::tui::paste::FinishedKeyStreamPaste),
}

/// 应用入口。
pub async fn run(
    config: Config,
    session_target: SessionTarget,
    prompt: &str,
    non_interactive: bool,
    no_session: bool,
) -> Result<(), String> {
    let mut config = config;
    // README2 Phase 4：启动时发现 skills（metadata-only；~/.tpi/skills +
    // <workspace>/.agent/skills）。
    crate::skills::manager::refresh_global(&config.workspace_root);
    let _ephemeral_root = if no_session {
        let root =
            std::env::temp_dir().join(format!("tpi-ephemeral-{}", crate::ids::EventId::new_v7()));
        std::fs::create_dir_all(&root).map_err(|e| format!("创建临时 session 目录失败: {e}"))?;
        config.sessions_root = root.join("sessions");
        config.artifacts_root = root.join("artifacts");
        std::fs::create_dir_all(&config.sessions_root)
            .map_err(|e| format!("创建临时 sessions 目录失败: {e}"))?;
        std::fs::create_dir_all(&config.artifacts_root)
            .map_err(|e| format!("创建临时 artifacts 目录失败: {e}"))?;
        Some(root)
    } else {
        None
    };

    if no_session {
        match session_target {
            SessionTarget::Continue | SessionTarget::Resume(_) => {
                return Err("--no-session 不能与 --continue/--resume 同时使用".into());
            }
            SessionTarget::New => {}
        }
    }
    let workspace_root = config.workspace_root.clone();
    let sessions_root = config.sessions_root.clone();
    let api_key = crate::config::read_api_key(&config)?;
    let mut provider = OpenAiCompatClient::new(
        config.model.base_url.clone(),
        config.model.name.clone(),
        api_key,
        config.model.reasoning.clone(),
        config.model.max_output_tokens,
        config.model.context_window,
    );

    // 共享的当前取消 token（Ctrl-C 第一次取消 run，空闲时退出）。
    let current_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    spawn_ctrl_c_handler(current_cancel.clone());

    let mut conversation = match session_target {
        SessionTarget::New => Conversation::new(),
        SessionTarget::Continue => {
            let session_id = latest_session_id(&sessions_root, &workspace_root)?;
            Conversation::resume(&sessions_root, &workspace_root, session_id)?
        }
        SessionTarget::Resume(id) => {
            let session_id = parse_session_id(&id)?;
            Conversation::resume(&sessions_root, &workspace_root, session_id)?
        }
    };

    if non_interactive {
        if prompt.is_empty() {
            return Err("非交互模式需要提供 prompt：tpi -p \"...\"".into());
        }
        if prompt.len() > crate::tui::editor::MAX_INPUT_BYTES {
            return Err(format!(
                "prompt 超过 {} KiB 上限",
                crate::tui::editor::MAX_INPUT_BYTES / 1024
            ));
        }
        let message = prompt.to_string();
        conversation.ensure_started(&sessions_root, &workspace_root)?;
        let outcome = {
            let (session, history) = conversation.parts_for_run()?;
            run_prompt_once(
                &mut provider,
                session,
                &config,
                history,
                message,
                current_cancel.clone(),
            )
            .await?
        };
        // P0-3：Cancel/ProviderInterrupted 后 runtime history 必须与 session
        // 事实源一致——统一从 durable log 重建（outcome.messages 在部分提交
        // 路径可能与 log 投影分叉）。
        conversation.refresh_from_log()?;
        // §16：wall-time 自动取消不是用户取消，-p 也要明确提示（stderr）。
        if outcome.reason == crate::session::CompletionReason::WallTimeExceeded {
            eprintln!("警告：run 达到 wall-time 预算被自动取消（非用户取消）");
        }
        // §4.3：流中断且已有 partial——输出 partial 但明确提示不完整（stderr）。
        if outcome.reason == crate::session::CompletionReason::ProviderInterrupted {
            eprintln!("警告：模型连接中断，以下为不完整的部分输出；session 已保留。");
        }
        // §18.3：`-p` 模式 stdout 只输出最终答案。
        if !outcome.assistant_text.is_empty() {
            println!("{}", outcome.assistant_text);
        }
        return Ok(());
    }

    // README2 Phase 3：MCP server 生命周期由 interactive_loop 管理。
    let mut mcp_manager = crate::mcp::manager::McpManager::new();
    interactive_loop(
        &mut provider,
        &mut conversation,
        &config,
        prompt,
        current_cancel.clone(),
        &mut mcp_manager,
    )
    .await
}

/// 非交互单次 run（输出经 stdout，仅最终答案）。
///
/// `pub`：`-p` 模式的执行路径，集成测试直接覆盖（P0-1 死锁回归）。
pub async fn run_prompt_once<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    history: &[ChatMessage],
    message: String,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<agent::AgentOutcome, String> {
    let cancel = CancellationToken::new();
    *crate::util::lock_mutex(&current_cancel, "current_cancel") = Some(cancel.clone());
    let (ui_tx, mut ui_rx) = mpsc::channel(128);
    // P0-1：`-p` 模式没有 TUI 消费 UI 事件；直接丢弃 rx 会在 channel 满后
    // 让 agent 的 `ui.send().await` 永久等待（挂死）。drain task 持续消费。
    tokio::spawn(async move { while ui_rx.recv().await.is_some() {} });
    let outcome = agent::run(
        provider,
        session,
        config,
        agent::RunInput {
            history,
            user_message: message,
            ui: ui_tx,
            cancel: cancel.clone(),
            interactive: false,
            force_compaction: false,
            workspace: None,
        },
    )
    .await;
    *crate::util::lock_mutex(&current_cancel, "current_cancel") = None;
    let outcome = outcome.map_err(|failure| failure.to_string())?;
    if outcome.reason == crate::session::CompletionReason::Error {
        return Err(format!(
            "run 以 Error 结束（长度限制/内容过滤/协议错误）；session 记录: {}",
            session.path().display()
        ));
    }
    if outcome.reason == crate::session::CompletionReason::ProviderUnavailable {
        // 未收到任何内容：明确报错（无 partial 可输出）。
        return Err(format!(
            "无法连接模型（重试后仍失败）；session 记录: {}",
            session.path().display()
        ));
    }
    Ok(outcome)
}

/// 交互主循环（§16：只有 renderer 写 stdout；帧合并 + synchronized update）。
///
/// 键盘由独立线程持续读取（运行中也响应输入/翻页/折叠，对标成熟 TUI Agent）；
/// 输入事件立即标 dirty，动画时钟只在 run 中推进（§16.1）。
async fn interactive_loop<P: Provider>(
    provider: &mut P,
    conversation: &mut Conversation,
    config: &Config,
    initial_prompt: &str,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
    mcp_manager: &mut crate::mcp::manager::McpManager,
) -> Result<(), String> {
    use crate::tui::event::UiEvent;
    use crate::tui::model::LineKind;
    use crate::tui::state::UiState;
    use ratatui::crossterm::event::{self, Event, KeyEventKind};

    let mut renderer = Renderer::new(
        crate::tui::theme::Theme::named(&config.ui_theme),
        config.ui_mode,
    )
    .map_err(|e| format!("初始化终端失败: {e}"))?;
    // §31：panic 时尽力恢复终端（不把 Windows Terminal/PowerShell 留在 raw mode）。
    install_terminal_panic_hook();
    let mut view = ViewModel {
        model_name: config.model.name.clone(),
        workspace: config
            .workspace_root
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        // §16.2：模型单价（config [model.primary] price_input/price_output）注入，
        // add_usage 按 token 累计花费显示。
        price_input: config.model.price_input,
        price_output: config.model.price_output,
        // §用户诉求：卡片折叠时显示的正文行数（[ui] collapsed_lines；
        // 0 = 折叠态只显示主行摘要）。
        collapsed_lines: config.ui_collapsed_lines,
        ..Default::default()
    };
    view.push_line(
        LineKind::System,
        "TPI：/help 查看命令与快捷键 · Esc 取消当前 run · 空闲 Ctrl+C 连按两次退出",
    );
    // README2 Phase 3：启动 MCP servers（从 ~/.tpi/config.toml；无配置则无操作）。
    {
        let started = mcp_manager.start_from_config().await;
        if started > 0 {
            view.push_line(
                LineKind::System,
                format!("MCP: {started} 个 server 已启动（/mcp 查看状态）"),
            );
        }
    }
    // §26-27：UiState 是 UI 单一事实源；交互循环只做
    // event → reducer → effects → draw（T3）。
    // §成熟化：注入 `[ui.keymap]` 生效键位（未配置动作保持内建默认）。
    let mut ui_state = UiState::with_keymap(view, config.ui_keymap.clone());
    // §用户诉求：右侧边栏默认打开（todo + 用户消息大纲；Ctrl+B 切换）。
    ui_state.view.sidebar.open = true;
    // BUG-006：--continue/--resume 启动时把已加载的 history 重建到屏幕，
    // 避免“模型有历史、屏幕空白/显示旧内容”的不一致。
    if !conversation.history().is_empty() {
        ui_state.view.load_history(conversation.history());
    }
    ui_state.view.plan = conversation.plan().cloned();
    // §用户诉求：--continue/--resume 恢复后明确显示当前会话（首条消息预览 +
    // 短 id），避免“恢复后不知道是哪一个会话”。
    if conversation.log().is_some() {
        ui_state
            .view
            .push_line(LineKind::System, session_resume_label(conversation.log()));
    }
    // 排队语义（§12 稳定化任务书）：运行中输入先存 ui_state.pending_message，
    // 当前 run 完成后由主循环作为下一条消息提交——不是 run 内的
    // boundary steering。
    if !initial_prompt.is_empty() {
        ui_state.push_pending(initial_prompt.to_string());
    }
    renderer
        .draw(&mut ui_state.view)
        .map_err(|e| e.to_string())?;

    // `@` 文件索引：后台扫描一次（跟随 .gitignore，有界 2000），不阻塞启动。
    // P0-3：不用一次性 channel + select 分支——sender drop 后 `recv()` 每次
    // poll 都立即返回 None，空闲时主循环忙转（CPU 空转）。改为共享状态，
    // 由键盘事件驱动的重绘路径顺带检查（索引到达不需要立即重绘）。
    let file_index: std::sync::Arc<std::sync::Mutex<Option<Vec<String>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    {
        let index_slot = file_index.clone();
        let index_root = config.workspace_root.clone();
        tokio::spawn(async move {
            let files = tokio::task::spawn_blocking(move || {
                crate::tool::search::index_files(&index_root, 2000)
            })
            .await
            .unwrap_or_default();
            *crate::util::lock_mutex(&index_slot, "file_index") = Some(files);
        });
    }

    // 键盘线程：整个交互期间由独立线程读取 crossterm 事件（§16.1：模型生成中
    // 也响应输入/翻页/折叠；输入不因生成被阻塞）。
    // §用户诉求（粘贴整段上屏）：支持 bracketed paste 的终端（WT/conhost 新
    // 版）把 Ctrl+V 注入为 `\x1b[200~ … \x1b[201~` 按键流；crossterm Windows
    // 后端不解析该序列。键盘线程按 Esc+`[` 检测粘贴开始、消费前缀 `[200~`、
    // 缓冲内容按键直到 `[201~`，整段作为一次 Event::Paste 上屏；行尾 Enter
    // 转 \n 不触发 submit，也不出现 `[200~` 垃圾前缀。
    // 普通 Ctrl+V/Shift+Insert 直接读取剪贴板，支持 bracketed paste 的终端
    // 解析完整控制序列。旧终端若退化成逐键流，字符仍一律立即转发，只把紧跟
    // 可插入字符的普通 Enter 改写为 Shift+Enter；不再缓冲/回溯整串输入。
    let (key_tx, mut key_rx) = mpsc::channel::<TerminalInput>(128);
    std::thread::spawn(move || {
        use crate::tui::paste;
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        // 待处理事件队列：bracketed-paste 前后缀探测读出的非控制事件放回
        // 这里再处理，保证探测过程中读出的任何事件都不丢失。
        let mut backlog: std::collections::VecDeque<Event> = std::collections::VecDeque::new();
        let mut fast_enter_guard = paste::FastPasteEnterGuard::default();
        macro_rules! send_event {
            ($event:expr) => {
                key_tx.blocking_send(TerminalInput::Event($event))
            };
        }
        macro_rules! finish_key_stream_paste {
            () => {{
                match fast_enter_guard.finish() {
                    Some(finished) => key_tx
                        .blocking_send(TerminalInput::FinishKeyStreamPaste(finished))
                        .is_ok(),
                    None => true,
                }
            }};
        }
        // 取下一个事件：backlog 优先，否则阻塞 read（不增加输入延迟）。
        // Windows Console 同时产生 key-down/key-up；Release 不是编辑动作，且
        // 若夹在 bracketed-paste 控制序列中会打断前后缀解析，因此在最外层
        // 统一丢弃。
        macro_rules! next_event {
            () => {{
                loop {
                    let candidate = if let Some(e) = backlog.pop_front() {
                        e
                    } else {
                        match event::read() {
                            Ok(e) => e,
                            Err(_) => return,
                        }
                    };
                    if !matches!(
                        &candidate,
                        Event::Key(k) if k.kind == KeyEventKind::Release
                    ) {
                        break candidate;
                    }
                }
            }};
        }
        loop {
            let event = next_event!();
            let event_at = std::time::Instant::now();
            if let Some(finished) = fast_enter_guard.finish_if_stale(event_at)
                && key_tx
                    .blocking_send(TerminalInput::FinishKeyStreamPaste(finished))
                    .is_err()
            {
                return;
            }
            let (is_key, is_press) = match &event {
                Event::Key(k) => (true, k.kind == KeyEventKind::Press),
                _ => (false, false),
            };
            if !is_key {
                // Paste/Mouse/Resize：无需解析，立即转发。
                if !finish_key_stream_paste!() {
                    return;
                }
                fast_enter_guard.clear();
                if send_event!(event).is_err() {
                    break;
                }
                continue;
            }
            // §优化（粘贴瞬间出现）：Ctrl+V / Shift+Insert 直读系统剪贴板，
            // 整段一次 Event::Paste（一次 draw 完成），任何终端都生效。
            if is_press && is_paste_shortcut(&event) {
                if !finish_key_stream_paste!() {
                    return;
                }
                fast_enter_guard.clear();
                if let Some(text) = crate::clipboard::read_text().ok().flatten()
                    && !text.is_empty()
                    && send_event!(Event::Paste(text)).is_err()
                {
                    return;
                }
                continue;
            }
            // §bracketed paste 序列解析（WT/conhost 等支持 bracketed paste 的
            // 终端）：Ctrl+V 粘贴被注入为 `\x1b[200~ … \x1b[201~` 按键流——
            // crossterm Windows 后端不解析该序列（逐键产生 KeyEvent），逐键转发
            // 会把 `[200~` 垃圾送进输入框，内容里的 Enter 也会被当作提交。
            // 检测无修饰 Esc 后紧跟 `[` 键 → 判定粘贴
            // 开始：消费前缀 `[200~`，内容按键全部缓冲，直到 `[201~`（Esc 后
            // 紧接 `2,0,1,~`）结束，整段作为一次 Event::Paste 上屏。不依赖间隔
            // 猜测、不读剪贴板；行尾 Enter 经 paste_char 转 \n，不会误提交。
            // 空闲超时兜底：异常/误判时 PASTE_IDLE_TIMEOUT 无新键即 flush 退出。
            if is_press
                && paste::is_plain_escape(match &event {
                    Event::Key(k) => k,
                    _ => unreachable!(),
                })
            {
                if !finish_key_stream_paste!() {
                    return;
                }
                fast_enter_guard.clear();
                if event::poll(paste::BRACKETED_PROBE_GAP).unwrap_or(false) {
                    let next = next_event!();
                    // 只匹配字符码：终端注入 `[` 时可能带 SHIFT 修饰（美式键盘
                    // `[` 是 Shift+[），不检查修饰，避免探测失败。Esc+Char('[')
                    // 组合正常输入只可能来自 bracketed paste 前缀。
                    let is_bracket = matches!(
                        &next,
                        Event::Key(k)
                            if k.kind == KeyEventKind::Press && k.code == KeyCode::Char('[')
                    );
                    if !is_bracket {
                        backlog.push_front(next.clone());
                    }
                    if is_bracket {
                        // 已消费 `[`，精确验证剩余 `200~`。旧实现只数 5 个事件
                        // 而不校验字符，普通 Esc 序列也可能被误吞。
                        let mut buf = String::new();
                        let mut prefix_ok = true;
                        let mut prefix_events = Vec::new();
                        for expected in paste::BRACKETED_START_TAIL {
                            if !event::poll(paste::BRACKETED_PROBE_GAP).unwrap_or(false) {
                                prefix_ok = false;
                                break;
                            }
                            let e = next_event!();
                            let matches = matches!(
                                &e,
                                Event::Key(k) if paste::is_sequence_char(k, expected)
                            );
                            prefix_events.push(e);
                            if !matches {
                                prefix_ok = false;
                                break;
                            }
                        }
                        if !prefix_ok {
                            // 不是 bracketed paste：原样转发探测期间消费的事件。
                            if send_event!(event).is_err() || send_event!(next).is_err() {
                                return;
                            }
                            for e in prefix_events {
                                if send_event!(e).is_err() {
                                    return;
                                }
                            }
                            continue;
                        }
                        // 内容收集：直到 `[201~` 或空闲超时。内容完整缓冲，
                        // 一次 Event::Paste 上屏（大粘贴由 reducer 占位符路径处理）。
                        loop {
                            if !event::poll(paste::PASTE_IDLE_TIMEOUT).unwrap_or(false) {
                                // 空闲超时：粘贴流结束但未收到 201~（异常/误判）。
                                if !buf.is_empty()
                                    && send_event!(Event::Paste(std::mem::take(&mut buf))).is_err()
                                {
                                    return;
                                }
                                break;
                            }
                            let e = next_event!();
                            match &e {
                                Event::Key(k) if k.kind == KeyEventKind::Press => {
                                    if k.code == KeyCode::Esc && k.modifiers == KeyModifiers::NONE {
                                        // 可能是 `[201~` 结束序列：Esc 后必须紧接
                                        // '[','2','0','1','~'。旧实现漏掉 '['，且把
                                        // 已匹配事件塞回队首，导致永远重复读取首字符。
                                        let mut is_end = true;
                                        let mut consumed = Vec::new();
                                        for expected in paste::BRACKETED_END_TAIL {
                                            if !event::poll(paste::BRACKETED_PROBE_GAP)
                                                .unwrap_or(false)
                                            {
                                                is_end = false;
                                                break;
                                            }
                                            let nxt = next_event!();
                                            let ok = matches!(
                                                &nxt,
                                                Event::Key(k2)
                                                    if paste::is_sequence_char(k2, expected)
                                            );
                                            consumed.push(nxt);
                                            if !ok {
                                                is_end = false;
                                                break;
                                            }
                                        }
                                        if is_end {
                                            // `[201~`：粘贴结束，整段一次上屏。
                                            if !buf.is_empty()
                                                && send_event!(Event::Paste(std::mem::take(
                                                    &mut buf
                                                )))
                                                .is_err()
                                            {
                                                return;
                                            }
                                            break;
                                        }
                                        // 不是结束序列：Esc 是粘贴内容（极罕见），保留。
                                        buf.push('\u{1b}');
                                        for consumed_event in consumed {
                                            match consumed_event {
                                                Event::Key(k2) => match paste::paste_char(&k2) {
                                                    Some(c) => buf.push(c),
                                                    None => backlog.push_back(Event::Key(k2)),
                                                },
                                                other => backlog.push_back(other),
                                            }
                                        }
                                        continue;
                                    }
                                    match paste::paste_char(k) {
                                        Some(c) => buf.push(c),
                                        None => {
                                            // 异常键（修饰组合/特殊键）：flush 已收，
                                            // 原样转发该键，退出收集。
                                            if !buf.is_empty()
                                                && send_event!(Event::Paste(std::mem::take(
                                                    &mut buf
                                                )))
                                                .is_err()
                                            {
                                                return;
                                            }
                                            if send_event!(Event::Key(*k)).is_err() {
                                                return;
                                            }
                                            break;
                                        }
                                    }
                                }
                                other => {
                                    // 非按键事件：flush 已收，转发，退出收集。
                                    if !buf.is_empty()
                                        && send_event!(Event::Paste(std::mem::take(&mut buf)))
                                            .is_err()
                                    {
                                        return;
                                    }
                                    if send_event!(other.clone()).is_err() {
                                        return;
                                    }
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                }
                // 无后续 `[`：普通 Esc，立即转发。
                if send_event!(event).is_err() {
                    return;
                }
                continue;
            }
            // Gemini CLI 同类旧终端兜底：达到长文本阈值前字符完全不缓冲，
            // 只把紧跟可插入字符的普通 Enter 改成 Shift+Enter（换行）。达到
            // 300 字符/第 6 行后立即折叠，余下尾部改走旁路，避免继续撑大 Editor。
            // `record_forwarded` 放在 blocking_send 之后，channel 背压不会把长粘贴
            // 内本来相邻的字符/Enter 人为拉出保护窗口。
            let Event::Key(key) = event else {
                unreachable!("non-key events were forwarded above");
            };
            let (key, protected_enter) = fast_enter_guard.rewrite_key(key, event_at);
            if fast_enter_guard.capture_if_collapsed(&key, protected_enter, event_at) {
                // 尾部静默后主动完成，不必等用户再按一个键才刷新实际字符数。
                if !event::poll(paste::FAST_PASTE_ENTER_GAP).unwrap_or(false)
                    && !finish_key_stream_paste!()
                {
                    return;
                }
                continue;
            }
            if fast_enter_guard.is_collapsed() && !finish_key_stream_paste!() {
                return;
            }
            if send_event!(Event::Key(key)).is_err() {
                break;
            }
            let forwarded_at = std::time::Instant::now();
            if let Some(rendered_suffix) =
                fast_enter_guard.record_forwarded(&key, protected_enter, forwarded_at)
            {
                if key_tx
                    .blocking_send(TerminalInput::CollapseKeyStreamPaste {
                        text: rendered_suffix.clone(),
                        rendered_suffix,
                    })
                    .is_err()
                {
                    return;
                }
                fast_enter_guard.mark_delivery_complete(std::time::Instant::now());
            }
        }
    });

    // `/retry` 目标：上一次因 provider 失败而中断的 turn。
    // reason 决定是否先移除 TUI 中未提交的 partial；成功后必须清空。
    let mut last_failed: Option<RetryTarget> = None;
    // §13（AGENTS.md）：run 因 request_input 挂起时，模型提出的问题；
    // 用户下一条普通消息即是对该问题的回答（记录 UserInputReceived 后继续）。
    let mut pending_question: Option<String> = None;
    // §InteractionRefactor：统一指针状态机（idle 与 run 共用），
    // 取代旧的 mouse_down/drag_selecting 两套路径。
    let mut pointer_gesture = crate::tui::interaction::PointerGesture::default();

    loop {
        // BUG-003：存在排队输入（初始 prompt / run 期间提交的消息 / /sessions 选择）
        // 时必须立即消费，不能先阻塞等待键盘事件——否则 `tpi "prompt"` 与
        // “run 结束后自动执行下一条”都要再按一次键才生效。
        let mut need_draw = false;
        if !ui_state.has_pending_work() {
            // 处理键盘事件（空闲时阻塞等待，不空转）。
            tokio::select! {
                event = key_rx.recv() => {
                    match event {
                        Some(TerminalInput::Event(Event::Key(key))) if is_actionable_key_kind(key.kind) => {
                            need_draw = true;
                            let effects = crate::tui::reducer::update(&mut ui_state, UiEvent::Key(key));
                            let mut quit = false;
                            for effect in effects {
                                if execute_ui_effect(effect, &mut ui_state, &mut renderer, current_cancel.clone()) {
                                    quit = true;
                                }
                            }
                            if quit {
                                // BUG-004：空闲 Ctrl-C 请求退出（走循环后的正常终端 restore）。
                                break;
                            }
                        }
                        Some(TerminalInput::Event(Event::Paste(text))) => {
                            need_draw = true;
                            crate::tui::reducer::update(&mut ui_state, UiEvent::Paste(text));
                        }
                        Some(TerminalInput::CollapseKeyStreamPaste { rendered_suffix, text }) => {
                            need_draw = true;
                            crate::tui::reducer::update(
                                &mut ui_state,
                                UiEvent::CollapseKeyStreamPaste { rendered_suffix, text },
                            );
                        }
                        Some(TerminalInput::FinishKeyStreamPaste(finished)) => {
                            need_draw = true;
                            crate::tui::reducer::update(
                                &mut ui_state,
                                UiEvent::FinishKeyStreamPaste {
                                    initial_text: finished.initial_text,
                                    full_text: finished.full_text,
                                },
                            );
                        }
                        // 鼠标：统一走 Pointer State Machine（§InteractionRefactor），
                        // §PointerHit ⑥：统一鼠标 dispatch（idle 与 run 同一实现）。
                        Some(TerminalInput::Event(Event::Mouse(mouse))) => {
                            let overlay_open = ui_state.view.overlay.is_some()
                                || ui_state.view.modal.is_some();
                            let events = handle_mouse(
                                mouse,
                                &renderer,
                                &mut pointer_gesture,
                                overlay_open,
                            );
                            if !events.is_empty() {
                                for event in events {
                                    crate::tui::reducer::update(&mut ui_state, event);
                                }
                                // Moved 不产生 reducer 事件；点击/拖选/滚动一旦
                                // 产生状态变化就必须画尾帧，不能因帧限速一直停在
                                // 旧状态直到下一次按键。
                                need_draw = true;
                            }
                        }
                        Some(TerminalInput::Event(Event::Resize(_, _))) => {
                            renderer.autoresize().map_err(|e| e.to_string())?;
                            need_draw = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        // `@` 文件索引就绪（P0-3：共享状态顺带检查，不占 select 分支）。
        if let Some(files) = crate::util::lock_mutex(&file_index, "file_index").take() {
            ui_state.view.file_index = files;
            need_draw = true;
        }

        // 空闲时不重绘：否则没有任何状态变化也会以 60 FPS 占用终端和 CPU。
        if need_draw {
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
        }

        // 主题选择（/theme 菜单 Enter）：应用主题 + 持久化到 home 配置。
        if let Some(theme_name) = ui_state.pending_theme.take() {
            renderer.set_theme(crate::tui::theme::Theme::named(&theme_name));
            match crate::config::set_ui_theme(&theme_name) {
                Ok(path) => {
                    ui_state.view.push_line(
                        LineKind::System,
                        format!("已切换主题 {theme_name}（已保存到 {}）", path.display()),
                    );
                }
                Err(error) => {
                    ui_state
                        .view
                        .push_line(LineKind::System, format!("主题已切换（保存失败: {error}）"));
                }
            }
            ui_state.view.menu = None;
            ui_state.view.modal = None;
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
        }

        // 会话恢复选择（/sessions 菜单 Enter）。
        if let Some(session_id) = ui_state.pending_session.take() {
            match parse_session_id(&session_id) {
                Ok(id) => {
                    match Conversation::resume(&config.sessions_root, &config.workspace_root, id) {
                        Ok(new_conversation) => {
                            *conversation = new_conversation;
                            // BUG-006：屏幕必须重建为新 session 的对话（不能残留旧屏幕）。
                            ui_state.view.load_history(conversation.history());
                            ui_state.view.plan = conversation.plan().cloned();
                            last_failed = None;
                            let label = session_resume_label(conversation.log());
                            ui_state.view.push_line(LineKind::System, label);
                        }
                        Err(error) => {
                            ui_state
                                .view
                                .push_line(LineKind::System, format!("恢复失败: {error}"));
                        }
                    }
                }
                Err(error) => ui_state.view.push_line(LineKind::System, error),
            }
            ui_state.view.menu = None;
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
        }

        // `/retry`：重试上一次失败 turn（空 user_message → agent 不重复 UserSubmitted）。
        if let Some(retry_target) = ui_state.take_pending_retry() {
            if matches!(
                last_failed.as_ref().map(|target| target.reason),
                Some(crate::session::CompletionReason::ProviderInterrupted)
            ) {
                ui_state.view.discard_last_interrupted_attempt();
            }
            conversation.ensure_started(&config.sessions_root, &config.workspace_root)?;
            // retry：user_message 传空（§4.3：不重复记录 UserSubmitted，复用 history）。
            let run_result = {
                let (session_log, history) = conversation.parts_for_run()?;
                run_interactive(
                    provider,
                    session_log,
                    config,
                    history,
                    String::new(),
                    InteractiveIo {
                        ui_state: &mut ui_state,
                        renderer: &mut renderer,
                        key_rx: &mut key_rx,
                        current_cancel: current_cancel.clone(),
                    },
                )
                .await
            };
            let outcome = match run_result {
                Ok(outcome) => outcome,
                Err(error) => {
                    conversation.refresh_from_log()?;
                    ui_state.view.status = crate::tui::model::StatusLine::Idle;
                    ui_state.view.turn = 0;
                    ui_state.view.push_line(
                        LineKind::System,
                        format!(
                            "run 失败，session 已保留：{}",
                            friendly_provider_failure(&error)
                        ),
                    );
                    renderer
                        .draw(&mut ui_state.view)
                        .map_err(|e| e.to_string())?;
                    last_failed = Some(RetryTarget {
                        message: retry_target,
                        reason: crate::session::CompletionReason::Error,
                    });
                    continue;
                }
            };
            // P0-3：统一从 durable log 重建 history（与 -p 路径一致）。
            conversation.refresh_from_log()?;
            last_failed = match outcome.reason {
                crate::session::CompletionReason::ProviderInterrupted
                | crate::session::CompletionReason::ProviderUnavailable
                | crate::session::CompletionReason::Error => {
                    // retry 失败也要明确反馈——否则用户看不到“重试未成功”，
                    // 会盲目反复 /retry（相同提示累积刷屏，且无进展）。
                    let label = match outcome.reason {
                        crate::session::CompletionReason::ProviderInterrupted => "模型连接中断",
                        crate::session::CompletionReason::ProviderUnavailable => "模型不可用",
                        crate::session::CompletionReason::Error => {
                            "长度限制/内容过滤/协议错误"
                        }
                        _ => unreachable!("失败类 reason 已穷尽"),
                    };
                    ui_state.view.push_line(
                        LineKind::System,
                        format!("⚠ 重试未成功（{label}）；可再次 /retry 或重新发送"),
                    );
                    Some(RetryTarget {
                        message: retry_target,
                        reason: outcome.reason,
                    })
                }
                _ => None,
            };
            ui_state.view.push_line(LineKind::System, "─".repeat(40));
            continue;
        }

        // 有提交的消息：运行。
        if let Some(message) = ui_state.pop_pending() {
            match handle_slash_command(
                &message,
                &mut ui_state,
                &mut renderer,
                config,
                conversation,
                &current_cancel,
                &mut last_failed,
                mcp_manager,
            )? {
                SlashAction::Quit => break,
                SlashAction::Consumed => continue,
                SlashAction::NotCommand => {}
            }
            ui_state.view.push_line(LineKind::User, message.clone());
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;

            conversation.ensure_started(&config.sessions_root, &config.workspace_root)?;
            // §13：若上一个 run 因 request_input 挂起，本次提交就是对该问题的回答
            // ——app 层先记录 UserInputReceived（durable 事实），再以普通 User
            // 消息继续（投影 + 完整历史）。
            let resume_answer = pending_question.take().map(|_| message.clone());
            let run_result = {
                let (session_log, history) = conversation.parts_for_run()?;
                if let Some(answer) = &resume_answer {
                    session_log
                        .append_event(&SessionEvent::UserInputReceived {
                            content: answer.clone(),
                        })
                        .and_then(|_| session_log.sync_data())
                        .map_err(|e| e.to_string())?;
                }
                run_interactive(
                    provider,
                    session_log,
                    config,
                    history,
                    message.clone(),
                    InteractiveIo {
                        ui_state: &mut ui_state,
                        renderer: &mut renderer,
                        key_rx: &mut key_rx,
                        current_cancel: current_cancel.clone(),
                    },
                )
                .await
            };
            let outcome = match run_result {
                Ok(outcome) => outcome,
                Err(error) => {
                    // 失败点可能位于 Assistant/Tool durable commit 之后；只追加 User
                    // 会构造残缺 context。始终从 session 事实源重建。
                    conversation.refresh_from_log()?;
                    // 交互模式：run 失败（provider/工具基础设施）不得杀死整个 TUI。
                    // 显示实际错误并保留 session，用户可以继续对话。
                    ui_state.view.status = crate::tui::model::StatusLine::Idle;
                    ui_state.view.turn = 0;
                    ui_state.view.push_line(
                        LineKind::System,
                        format!(
                            "run 失败，session 已保留：{}",
                            friendly_provider_failure(&error)
                        ),
                    );
                    renderer
                        .draw(&mut ui_state.view)
                        .map_err(|e| e.to_string())?;
                    // 记录失败 turn 供 /retry（§4.3）。
                    last_failed = Some(RetryTarget {
                        message: message.clone(),
                        reason: crate::session::CompletionReason::Error,
                    });
                    continue;
                }
            };
            // P0-3：统一从 durable log 重建 history——Cancel/ProviderInterrupted/
            // 正常完成都由 session 事实源投影，杜绝 runtime 与 log 分叉。
            conversation.refresh_from_log()?;
            // §13（AGENTS.md）：request_input 挂起 → 记录待回答问题，
            // 用户下一条普通消息即作为回答继续（UserInputReceived 已在此前分支记录）。
            // 选择器确认的选项也走同一条 pending_messages 路径。
            if outcome.reason == crate::session::CompletionReason::AwaitingUserInput {
                pending_question = outcome
                    .awaiting_input
                    .as_ref()
                    .map(|awaiting| awaiting.text.clone());
            }
            // 成功（或正常中断）后清空 retry 目标；ProviderInterrupted 是失败类，
            // 保留以便用户 /retry。
            match outcome.reason {
                crate::session::CompletionReason::ProviderInterrupted
                | crate::session::CompletionReason::ProviderUnavailable
                | crate::session::CompletionReason::Error => {
                    last_failed = Some(RetryTarget {
                        message: message.clone(),
                        reason: outcome.reason,
                    });
                }
                _ => last_failed = None,
            }
            ui_state.view.push_line(LineKind::System, "─".repeat(40));
        }
    }

    // README2 Phase 3：退出前关闭所有 MCP server（不留孤儿进程，§9）。
    mcp_manager.shutdown_all().await;
    renderer.restore().map_err(|e| e.to_string())?;
    Ok(())
}

/// §PointerHit ⑥：统一 idle/run 的鼠标 dispatch——同一实现，杜绝两处 drift。
/// 分派 slash 命令（从 `interactive_loop` 内联块提取）：命令在循环内短路
/// run 路径。返回 [`SlashAction`] 由主循环解释；错误（draw/IO）向上传播，
/// 与原内联 `?` 语义一致。
fn handle_slash_command(
    message: &str,
    ui_state: &mut UiState,
    renderer: &mut Renderer,
    config: &Config,
    conversation: &mut Conversation,
    current_cancel: &Arc<Mutex<Option<CancellationToken>>>,
    last_failed: &mut Option<RetryTarget>,
    mcp_manager: &mut crate::mcp::manager::McpManager,
) -> Result<SlashAction, String> {
    use crate::tui::SLASH_COMMANDS;
    match message {
        "/quit" | "/exit" => Ok(SlashAction::Quit),
        // README2 Phase 3：/mcp 状态页（Server/Status/Tools）+ restart。
        // 交互式环境无 registry 传入；McpManager 内部持有运行时状态。
        msg if msg == "/mcp" || msg.starts_with("/mcp ") => {
            let body = render_mcp_status(mcp_manager);
            ui_state.view.open_modal("/mcp", body);
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/settings" => {
            let shell = config
                .shell_path
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "未配置（自动查找 Git Bash）".to_string());
            // §成熟化：展示 [ui.keymap] 生效绑定（默认 + 自定义合并后）。
            let mut keymap_text = String::new();
            for (action, keys) in config.ui_keymap.display_bindings() {
                keymap_text.push_str(&format!("  {action}: {keys}\n"));
            }
            ui_state.view.open_modal(
                "/settings",
                format!(
                    "配置来源: {}
workspace: {}
sessions: {}
artifacts: {}
shell: {shell}
主题: {}（omp / dark / light / opencode / onedarkpro；/theme 切换）
web_search: DuckDuckGo（免费，无需 API key）
自动打开浏览器: {}
保留 token: {}
允许访问 workspace 外路径: {}
模型单价: {}/百万输入 · {}/百万输出（未配置则不在 footer 显示花费）

键位（[ui.keymap]，{}）:
{keymap_text}",
                    config.source,
                    config.workspace_root,
                    config.sessions_root.display(),
                    config.artifacts_root.display(),
                    config.ui_theme,
                    if config.auto_open_browser {
                        "是"
                    } else {
                        "否"
                    },
                    config.safety_reserve_tokens,
                    if config.allow_outside_workspace {
                        "是（AI 自由模式）"
                    } else {
                        "否（严格沙箱）"
                    },
                    fmt_price(config.model.price_input),
                    fmt_price(config.model.price_output),
                    "未配置时为内建默认",
                ),
            );
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/model" => {
            ui_state.view.open_modal(
                "/model",
                format!(
                    "primary:
  名称: {}
  provider: {}
  base_url: {}
  reasoning: {}
  max_output_tokens: {}
  context_window: {}
  api_key_env: {}",
                    config.model.name,
                    config.model.provider,
                    config.model.base_url,
                    config
                        .model
                        .reasoning
                        .clone()
                        .unwrap_or_else(|| "默认".to_string()),
                    config
                        .model
                        .max_output_tokens
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "默认".to_string()),
                    config
                        .model
                        .context_window
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "未配置".to_string()),
                    config.model.api_key_env,
                ),
            );
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/help" => {
            let mut text = String::from(
                "命令：
",
            );
            for (name, desc) in SLASH_COMMANDS {
                text.push_str(&format!(
                    "/{name} —— {desc}
"
                ));
            }
            text.push_str(
                "快捷键：Shift+Enter 换行 · ↑/↓ 多行/历史 · Tab 命令补全 ·
                        @文件 引用补全 · Alt+T 思考折叠 · Alt+E/Alt+O 工具详情 ·
                        Alt+[/] 切换工具 · Ctrl+F 搜索 · Ctrl+U 清空 ·
                        Ctrl+Z 撤销 · Ctrl+Y 重做 ·
                        Ctrl+A/E 行首/行尾 · PgUp/PgDn 翻页 · 滚轮/滚动条滚动 ·
                        Ctrl+Home 顶部 · Ctrl+End 最新 · Modal ↑/↓ 滚动 ·
                        点击工具卡片（任意行）展开 · 悬停卡片微高亮 ·
                        点击链接打开 · 拖选自动滚动 ·
                        Esc 取消 run · Ctrl+C 有选区复制/运行中取消/空闲连按两次退出
                        键位可在配置 [ui.keymap] 中自定义（/settings 查看当前绑定）",
            );
            ui_state.view.open_modal("/help", text);
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/session" => {
            let info = match conversation.log() {
                Some(log) => format!(
                    "session: {}
workspace: {}
事件数: {}",
                    log.session_id(),
                    log.workspace_id(),
                    log.seq()
                ),
                None => "尚无 session（第一条消息后创建）".to_string(),
            };
            ui_state.view.open_modal("/session", info);
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/sessions" => {
            // 会话浏览器：列出当前 workspace 的 session，Enter 恢复。
            let sessions = match list_sessions(&config.sessions_root, &config.workspace_root) {
                Ok(sessions) => sessions,
                Err(error) => {
                    push_system_line(
                        &mut ui_state.view,
                        renderer,
                        format!("无法列出 session: {error}"),
                    )?;
                    return Ok(SlashAction::Consumed);
                }
            };
            if sessions.is_empty() {
                push_system_line(
                    &mut ui_state.view,
                    renderer,
                    "当前 workspace 没有历史 session".to_string(),
                )?;
                return Ok(SlashAction::Consumed);
            }
            let wid = session::workspace_id_for(config.workspace_root.as_std_path());
            let sessions_dir = config.sessions_root.join(&wid);
            let menu_items: Vec<(String, String)> = sessions
                .iter()
                .map(|(id, modified, count, preview)| {
                    // §用户诉求：会话标题 = 首条用户消息（preview）置前；
                    // 无预览时显示“无标题”兜底，不让用户面对哈希 id。
                    let title = if preview.is_empty() {
                        "(无标题)".to_string()
                    } else {
                        preview.clone()
                    };
                    // §用户诉求：会话名（首条消息）置前；时间带日期
                    // （MM-DD HH:MM），跨天会话不再难以分辨。
                    let label =
                        format!("{} · {} · {} 事件", title, fmt_time_short(*modified), count);
                    (id.to_string(), label)
                })
                .collect();
            // §用户诉求：/sessions 菜单内预览选中会话的对话（只取 User 与
            // AI 消息，不含工具输出），帮助分辨会话内容。
            let session_previews: Vec<Vec<crate::tui::model::MenuPreviewLine>> = sessions
                .iter()
                .map(|(id, ..)| session_dialogue_preview(&sessions_dir.join(format!("{id}.jsonl"))))
                .collect();
            // §用户诉求：/sessions 悬浮窗（Modal）显示选中会话的对话预览——
            // 初始为第一个会话；↑/↓ 移动时 reducer 同步更新。
            let preview_body = preview_lines_to_body(
                session_previews
                    .first()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
            ui_state.view.menu = Some(crate::tui::model::MenuView {
                items: menu_items,
                selected: 0,
                kind: crate::tui::model::MenuKind::Session,
                session_previews,
            });
            ui_state.view.open_modal("/sessions", preview_body);
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/theme" => {
            // 主题选择菜单（Modal 说明 + Theme 菜单；↑/↓ 选择 Enter 应用）。
            // 主题名 → 描述（含绑定的代码高亮主题；与 theme.rs 绑定保持一致）。
            const THEME_ITEMS: &[(&str, &str)] = &[
                ("omp", "默认 Catppuccin 系 · 高亮 base16-mocha"),
                ("dark", "简洁深色 · 高亮 base16-ocean"),
                ("light", "简洁浅色 · 高亮 base16-ocean light"),
                ("opencode", "opencode 风格 · 高亮 base16-eighties"),
                ("onedarkpro", "One Dark Pro 官方色板 · 高亮 Solarized dark"),
            ];
            let items: Vec<(String, String)> = THEME_ITEMS
                .iter()
                .map(|(name, desc)| (name.to_string(), desc.to_string()))
                .collect();
            ui_state.view.menu = Some(crate::tui::model::MenuView {
                items,
                selected: 0,
                kind: crate::tui::model::MenuKind::Theme,
                session_previews: Vec::new(),
            });
            ui_state.view.open_modal(
                "/theme",
                format!(
                    "当前主题: {}（代码高亮随主题联动）\n\n↑/↓ 选择 · Enter 应用并保存到 {} · Esc 取消\n\n注：若 workspace 配置了 [ui] theme，下次启动以 workspace 为准。",
                    config.ui_theme,
                    crate::config::tpi_home().join("config.toml").display(),
                ),
            );
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/new" => {
            conversation.reset();
            *last_failed = None;
            // BUG-006：屏幕投影必须与已清空的上下文同步（否则显示旧 session）。
            ui_state.view.reset_for_new_session();
            push_system_line(&mut ui_state.view, renderer, "已开始新会话".to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/cancel" => {
            if let Some(cancel) = crate::util::lock_mutex(current_cancel, "current_cancel").clone()
            {
                cancel.cancel();
                push_system_line(
                    &mut ui_state.view,
                    renderer,
                    "已发送取消（§11.5：保留 session）".to_string(),
                )?;
            } else {
                push_system_line(
                    &mut ui_state.view,
                    renderer,
                    "当前没有正在运行的 run".to_string(),
                )?;
            }
            Ok(SlashAction::Consumed)
        }
        "/thinking" => {
            let value = config
                .model
                .reasoning
                .clone()
                .unwrap_or_else(|| "未配置（默认）".to_string());
            ui_state.view.open_modal(
                "/thinking",
                format!(
                    "reasoning: {value}
说明: 透传给 provider 的推理设置（§18.1 [model.primary] reasoning）；
未配置时使用 provider 默认。",
                ),
            );
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/diff" => {
            // §19：diff 是查看型内容，走 Modal 不污染 transcript。
            let diff = match conversation.log() {
                Some(log) => last_edit_diff(log),
                None => "尚无 session".to_string(),
            };
            ui_state.view.open_modal("/diff", diff);
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/doctor" => {
            // §19：环境检查报告走 Modal（此前 push 进 transcript 污染聊天历史）。
            ui_state.view.open_modal(
                "/doctor",
                crate::doctor::render_report(&config.workspace_root),
            );
            renderer
                .draw(&mut ui_state.view)
                .map_err(|e| e.to_string())?;
            Ok(SlashAction::Consumed)
        }
        "/compact" => {
            // P1-10：手动压缩——在下一次 run 开始时的完整边界执行。
            ui_state.force_compaction = true;
            push_system_line(
                &mut ui_state.view,
                renderer,
                "将在下一次 run 开始时执行手动压缩（压缩旧历史为摘要）".to_string(),
            )?;
            Ok(SlashAction::Consumed)
        }
        "/retry" => {
            // §4.3：重试上一次失败/中断的 ModelTurn——不是重发 User 消息。
            // 目标消息入 pending_retry，主循环以空 user_message 发起 run，
            // 不重复记录 UserSubmitted，也不追加 User 消息（不污染对话）。
            // 去重：相同 target 的连续重试提示只保留一行（用户反复 /retry 时
            // 不刷屏——失败反馈由 retry run 结束后的分支给出）。
            match last_failed.clone() {
                Some(target) => {
                    ui_state.push_retry(target.message.clone());
                    let text = format!("⟳ 重试上一次 turn（{}）", target.message);
                    if ui_state.view.push_line_dedup(LineKind::System, text) {
                        renderer
                            .draw(&mut ui_state.view)
                            .map_err(|e| e.to_string())?;
                    }
                }
                None => {
                    push_system_line(
                        &mut ui_state.view,
                        renderer,
                        "没有可重试的 turn（上一次 run 成功或尚无 run）".to_string(),
                    )?;
                }
            }
            Ok(SlashAction::Consumed)
        }
        _ => Ok(SlashAction::NotCommand),
    }
}

/// 输入 crossterm MouseEvent + renderer + 状态机，输出语义化 UiEvent。
/// `overlay_open`：弹层打开时点击不动作。
/// README2 Phase 3：/mcp 状态页文本（Server/Status/Tools 表）。
fn render_mcp_status(mcp_manager: &crate::mcp::manager::McpManager) -> String {
    let statuses = mcp_manager.statuses();
    if statuses.is_empty() {
        return "未配置 MCP server。\n\n在 ~/.tpi/config.toml 添加：\n[mcp.servers.<name>]\ncommand = \"...\"\nargs = []\nenabled = true".to_string();
    }
    let mut out = String::from("Server     Status            Tools\n");
    out.push_str("---------  ----------------  -----\n");
    for (name, status) in statuses {
        match status {
            crate::mcp::manager::McpServerStatus::Connected { tool_count } => {
                out.push_str(&format!("{name:<10} connected         {tool_count}\n"));
            }
            crate::mcp::manager::McpServerStatus::Failed(detail) => {
                out.push_str(&format!("{name:<10} failed            -\n  {detail}\n"));
            }
            crate::mcp::manager::McpServerStatus::Stopped => {
                out.push_str(&format!("{name:<10} stopped           0\n"));
            }
        }
    }
    out
}

fn handle_mouse(
    mouse: ratatui::crossterm::event::MouseEvent,
    renderer: &Renderer,
    gesture: &mut crate::tui::interaction::PointerGesture,
    overlay_open: bool,
) -> Vec<crate::tui::event::UiEvent> {
    use crate::tui::interaction::PointerInput;
    use ratatui::crossterm::event::MouseButton;
    use ratatui::crossterm::event::MouseEventKind;
    // §用户诉求：侧边栏区域的滚轮/滚动条点击在进入 pointer 状态机前拦截——
    // 滚轮滚动侧边栏（而非转录区）；最右滚动条列点击/拖拽按比例跳转。
    if !overlay_open
        && let Some(rect) = renderer.sidebar_rect()
        && mouse.column >= rect.x
        && mouse.column < rect.x + rect.width
        && mouse.row >= rect.y
        && mouse.row < rect.y + rect.height
    {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                return vec![crate::tui::event::UiEvent::SidebarScroll(true)];
            }
            MouseEventKind::ScrollDown => {
                return vec![crate::tui::event::UiEvent::SidebarScroll(false)];
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
                if mouse.column == rect.x + rect.width - 1 =>
            {
                // 侧边栏滚动条列：按比例跳转（不走 pointer 状态机，避免误入
                // transcript 的 DraggingScrollbar）。
                return vec![crate::tui::event::UiEvent::SidebarScrollbarClick(
                    mouse.row.saturating_sub(rect.y),
                )];
            }
            _ => {}
        }
    }
    let hit = if overlay_open {
        crate::tui::interaction::PointerHit::none()
    } else {
        pointer_target(renderer, mouse.column, mouse.row)
    };
    let input = match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => PointerInput::Down {
            column: mouse.column,
            row: mouse.row,
            hit,
        },
        MouseEventKind::Drag(MouseButton::Left) => PointerInput::Drag {
            column: mouse.column,
            row: mouse.row,
            hit,
        },
        MouseEventKind::Up(MouseButton::Left) => PointerInput::Up {
            column: mouse.column,
            row: mouse.row,
            hit,
        },
        MouseEventKind::Moved => PointerInput::Move {
            column: mouse.column,
            row: mouse.row,
            hit,
        },
        MouseEventKind::ScrollUp => PointerInput::ScrollUp,
        MouseEventKind::ScrollDown => PointerInput::ScrollDown,
        _ => return Vec::new(),
    };
    // §成熟化：拖选自动滚动——把转录区矩形传给指针状态机，
    // Selecting 拖出视口上下边缘时自动滚动（选区跨越屏幕扩展）。
    gesture.feed_in_viewport(input, renderer.transcript_rect())
}

/// §PointerHit：屏幕坐标 → 组合命中（文本 + 可选动作 + 区域）。
/// 一个 cell 可以是「可选择文本 + 可点击控件」——不再互斥。
fn pointer_target(
    renderer: &Renderer,
    column: u16,
    row: u16,
) -> crate::tui::interaction::PointerHit {
    use crate::tui::interaction::{PointerAction, PointerHit, PointerRegion};
    // 右侧边栏命中优先（§用户诉求：大纲行点击跳转；非转录文本，不可选择）。
    if let Some(rect) = renderer.sidebar_rect()
        && column >= rect.x
        && column < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
    {
        // 边栏最右列是滚动条：handle_mouse 已拦截（SidebarScrollbarClick），
        // 这里只对内容列返回大纲跳转/空白（避免滚动条列误入 transcript 状态机）。
        return match renderer.sidebar_hit(column, row) {
            Some(entry) => PointerHit::sidebar_jump(entry),
            None => PointerHit::sidebar_blank(),
        };
    }
    // scrollbar 优先（1 列窄条，避免误判为文本）。
    if let Some(rect) = renderer.scrollbar_rect()
        && column >= rect.x
        && column < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
    {
        return PointerHit::scrollbar();
    }
    // 转录区：先取文本位置，再叠加动作（若该行可点击）。
    if let Some(position) = renderer.hit_text(column, row) {
        let action = match renderer.hit_target(column, row) {
            Some(crate::tui::HitTarget::Tool(id)) => Some(PointerAction::Tool(id)),
            Some(crate::tui::HitTarget::Reasoning(id)) => Some(PointerAction::Reasoning(id)),
            None => None,
        };
        // §成熟化：链接优先于文本选择区域判定——命中链接文本即 Link 动作
        // （文本+动作可共存；链接文本上轻点 = 打开 Link Overlay）。
        let action = action.or_else(|| renderer.link_at(column, row).map(PointerAction::Link));
        return PointerHit {
            text: Some(position),
            action,
            region: PointerRegion::Transcript,
        };
    }
    // 非转录文本位置（footer/header 等）：仍可能命中动作（不常见，但保留）。
    if let Some(target) = renderer.hit_target(column, row) {
        let action = match target {
            crate::tui::HitTarget::Tool(id) => Some(PointerAction::Tool(id)),
            crate::tui::HitTarget::Reasoning(id) => Some(PointerAction::Reasoning(id)),
        };
        return PointerHit {
            text: None,
            action,
            region: PointerRegion::Transcript,
        };
    }
    PointerHit::none()
}
/// 执行 reducer 返回的跨边界效果（§27：app 层执行 effect）。
/// 返回 true 表示请求退出主循环（BUG-004：空闲 Ctrl-C）。
fn execute_ui_effect(
    effect: UiEffect,
    ui_state: &mut UiState,
    _renderer: &mut Renderer,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) -> bool {
    match effect {
        UiEffect::CancelRun => {
            if let Some(cancel) = crate::util::lock_mutex(&current_cancel, "current_cancel").clone()
            {
                cancel.cancel();
            }
            // §PointerHit：瞬时反馈用 footer hint，不污染 transcript。
            ui_state.view.transient_hint = Some("已发送取消（Esc）；保留 session".into());
            false
        }
        // BUG-004：空闲 Ctrl-C 产生 Quit → app 层 break 主循环走正常退出（含终端 restore）。
        UiEffect::Quit => true,
        // ResumeSession 由交互主循环处理（/sessions 菜单）。
        UiEffect::ResumeSession(_) => false,
        // §成熟化：Link Overlay Enter → 打开 URL。仅允许 http/https scheme
        //（防 file:// 等危险协议）；显式用户动作，不违反"绝不自动打开浏览器"。
        UiEffect::OpenUrl(url) => {
            let lower = url.trim().to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                open_url_external(&url);
                ui_state.view.transient_hint = Some("已在默认浏览器打开".into());
            } else {
                ui_state.view.transient_hint = Some(format!(
                    "仅支持 http/https 链接，已拒绝打开：{}",
                    crate::tui::text::truncate_middle_utf8(&url, 40, "…")
                ));
            }
            false
        }
        // §成熟化：Link Overlay `c` → 复制 URL 到剪贴板。
        UiEffect::CopyText(url) => {
            if crate::clipboard::set_text(&url) {
                ui_state.view.transient_hint = Some("URL 已复制到剪贴板".into());
            } else {
                ui_state.view.transient_hint =
                    Some("复制失败：剪贴板不可用（可能被占用或平台不支持）".into());
            }
            false
        }
        // §PointerHit：复制选中文本到剪贴板。copy 从 ViewModel 语义文本提取
        //（不依赖当前 viewport 快照；Renderer 只负责几何映射）。
        UiEffect::CopySelection => {
            let text = ui_state.view.selected_text();
            if !text.is_empty() {
                // §PointerHit 11：剪贴板失败如实提示，不伪装"已复制"。
                if crate::clipboard::set_text(&text) {
                    ui_state.view.transient_hint =
                        Some(format!("已复制 {} 行到剪贴板", text.lines().count()));
                    // 复制成功后清除选区（高亮消失）。
                    ui_state.view.selection_clear();
                } else {
                    // §修复：失败保留选区——否则第一次失败后选区消失，再按
                    // Ctrl+C 变成无选区（无动作），只能重新拖选。
                    ui_state.view.transient_hint =
                        Some("复制失败：剪贴板不可用，可再按一次 Ctrl+C 重试".into());
                }
            }
            false
        }
    }
}

/// §修复：run 失败的错误文案——连接类失败（Connection/StreamInterrupted）对
/// 用户显示可操作提示而非裸露内部错误（"stream ended before [DONE]..." 等），
/// 完整错误仍在 tracing 日志与 session 记录中可诊断；非连接类保留原文。
fn friendly_provider_failure(error: &str) -> String {
    if error.contains("connection failed") || error.contains("stream interrupted") {
        "模型连接中断（重试后仍失败）；session 已保留，可稍后重试或检查网络/代理".to_string()
    } else {
        error.to_string()
    }
}

/// 在默认浏览器打开 URL（Windows：`cmd /c start`；显式用户动作才调用）。
fn open_url_external(url: &str) {
    use std::process::Command;
    // `start` 是 cmd 内建命令，必须经 cmd 执行；url 已校验 http/https 前缀。
    #[cfg(windows)]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(windows))]
    let _ = Command::new("xdg-open").arg(url).spawn();
}

struct InteractiveIo<'a> {
    ui_state: &'a mut UiState,
    renderer: &'a mut Renderer,
    key_rx: &'a mut mpsc::Receiver<TerminalInput>,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
}

async fn run_interactive<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    history: &[ChatMessage],
    message: String,
    io: InteractiveIo<'_>,
) -> Result<agent::AgentOutcome, String> {
    use crate::tui::effect::UiEffect;
    use crate::tui::event::UiEvent;
    use crate::tui::model::LineKind;
    let InteractiveIo {
        ui_state,
        renderer,
        key_rx,
        current_cancel,
    } = io;
    let cancel = CancellationToken::new();
    *crate::util::lock_mutex(&current_cancel, "current_cancel") = Some(cancel.clone());
    ui_state.view.status = StatusLine::Running {
        turn: 0,
        tool: "正在连接模型".into(),
    };
    ui_state.running = true;
    // §InteractionRefactor：run 期间与空闲用同一指针状态机（拖选复制在
    // Agent 运行时可用的关键能力）。
    let mut pointer_gesture = crate::tui::interaction::PointerGesture::default();
    let (ui_tx, mut ui_rx) = mpsc::channel(128);

    let force = ui_state.force_compaction;
    ui_state.force_compaction = false;
    let run_future = agent::run(
        provider,
        session,
        config,
        agent::RunInput {
            history,
            user_message: message.clone(),
            ui: ui_tx,
            cancel: cancel.clone(),
            interactive: true,
            force_compaction: force,
            workspace: None,
        },
    );
    tokio::pin!(run_future);

    // 动画时钟（§16.1）：独立推进，驱动 spinner/工具卡动画。150 ms/帧
    // （≈6.7 FPS）视觉已流畅——动画只有 footer spinner 与工具卡 spinner，
    // 过密的 tick 只会让整屏全量重绘更频繁（性能：终端卡顿）。
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(150));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome = loop {
        tokio::select! {
            event = ui_rx.recv() => {
                if let Some(event) = event {
                    // Agent 事件 → reducer（纯状态转换；T3）。
                    crate::tui::reducer::update(ui_state, UiEvent::Agent(event));
                }
                if renderer.should_draw() {
                    renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                }
            }
            _ = ticker.tick() => {
                crate::tui::reducer::update(ui_state, UiEvent::Tick);
                if renderer.should_draw() {
                    renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                }
            }
            key = key_rx.recv() => {
                match key {
                    Some(TerminalInput::Event(Event::Key(key))) if is_actionable_key_kind(key.kind) => {
                        // 键盘事件 → reducer；Esc 取消语义在 reducer 内
                        // 依据 running 状态决策（§6.2 保留 session）。
                        let effects = crate::tui::reducer::update(ui_state, UiEvent::Key(key));
                        for effect in effects {
                            match effect {
                                UiEffect::CancelRun => {
                                    cancel.cancel();
                                    // §PointerHit：瞬时反馈用 footer hint。
                                    ui_state.view.transient_hint =
                                        Some("已发送取消（Esc）；保留 session".into());
                                }
                                // §PointerHit 11：Ctrl+C 有选区时复制（run 中也可复制）。
                                UiEffect::CopySelection => {
                                    let text = ui_state.view.selected_text();
                                    if !text.is_empty() {
                                        if crate::clipboard::set_text(&text) {
                                            ui_state.view.transient_hint = Some(format!(
                                                "已复制 {} 行到剪贴板",
                                                text.lines().count()
                                            ));
                                            // 复制成功后清除选区。
                                            ui_state.view.selection_clear();
                                        } else {
                                            // §修复：失败保留选区，可重试（同主循环 handler）。
                                            ui_state.view.transient_hint = Some(
                                                "复制失败：剪贴板不可用，可再按一次 Ctrl+C 重试".into(),
                                            );
                                        }
                                    }
                                }
                                UiEffect::Quit
                                | UiEffect::ResumeSession(_)
                                | UiEffect::OpenUrl(_)
                                | UiEffect::CopyText(_) => {
                                    // run 中不会产生（reducer 仅空闲时产生）。
                                }
                            }
                        }
                        renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                    }
                    Some(TerminalInput::Event(Event::Paste(text))) => {
                        crate::tui::reducer::update(ui_state, UiEvent::Paste(text));
                        renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                    }
                    Some(TerminalInput::CollapseKeyStreamPaste { rendered_suffix, text }) => {
                        crate::tui::reducer::update(
                            ui_state,
                            UiEvent::CollapseKeyStreamPaste { rendered_suffix, text },
                        );
                        renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                    }
                    Some(TerminalInput::FinishKeyStreamPaste(finished)) => {
                        crate::tui::reducer::update(
                            ui_state,
                            UiEvent::FinishKeyStreamPaste {
                                initial_text: finished.initial_text,
                                full_text: finished.full_text,
                            },
                        );
                        renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                    }
                    Some(TerminalInput::Event(Event::Mouse(mouse))) => {
                        // §PointerHit ⑥：统一鼠标 dispatch（idle 与 run 同一实现）。
                        let overlay_open = ui_state.view.overlay.is_some()
                            || ui_state.view.modal.is_some();
                        let events = handle_mouse(mouse, renderer, &mut pointer_gesture, overlay_open);
                        let changed = !events.is_empty();
                        for event in events {
                            crate::tui::reducer::update(ui_state, event);
                        }
                        // 未启用 any-event hover；只有真实状态变化才重绘。
                        if changed {
                            renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                        }
                    }
                    Some(TerminalInput::Event(Event::Resize(_, _))) => {
                        renderer.autoresize().map_err(|e| e.to_string())?;
                        renderer.draw(&mut ui_state.view).map_err(|e| e.to_string())?;
                    }
                    _ => {}
                }
            }
            result = &mut run_future => {
                // §修复：agent 返回前发出的最后几个流式 delta 可能还滞留在
                // ui_rx（select 在 run_future 与 recv 同时 ready 时随机分支，
                // 可能先命中 run_future 完成）。收净后再 break——否则
                // finalize_live 提交的 live.assistant 缺最后一段，屏幕上
                // 最后一句被截断（如“要我做”只剩“要我做”）。
                while let Ok(event) = ui_rx.try_recv() {
                    crate::tui::reducer::update(ui_state, UiEvent::Agent(event));
                }
                break result;
            }
        }
    };
    *crate::util::lock_mutex(&current_cancel, "current_cancel") = None;
    ui_state.running = false;
    // TUI v2 §7.2：run 结束（成功或失败）→ 提交全部剩余 live 内容。
    ui_state.view.finalize_live();
    let outcome = outcome.map_err(|failure| failure.to_string())?;
    match outcome.reason {
        crate::session::CompletionReason::Error => {
            ui_state.view.push_line(
                LineKind::System,
                "run 以 Error 结束（长度限制/内容过滤/协议错误，见 session 记录）",
            );
        }
        crate::session::CompletionReason::ContextOverflow => {
            // P1-4：压缩与 prune 后仍超窗口——明确提示而不是让请求失败。
            ui_state.view.push_line(
                LineKind::System,
                "上下文仍超出模型窗口（压缩后仍无法容纳）。请 /new 开启新会话，或检查配置中的 context_window。",
            );
        }
        crate::session::CompletionReason::ProviderInterrupted => {
            // §4.3：流中断且已有 partial——partial 已保留在 transcript 与 session，
            // 明确提示而不是让用户以为整个 turn 丢了。
            ui_state.view.push_line(
                LineKind::System,
                "⚠ 模型连接中断，已保留本次部分输出和 session 状态；可重新发送继续。",
            );
        }
        crate::session::CompletionReason::ProviderUnavailable => {
            ui_state.view.push_line(
                LineKind::System,
                "⚠ 无法连接模型（重试后仍失败）；session 已保留，可稍后重试。",
            );
        }
        crate::session::CompletionReason::WallTimeExceeded => {
            // §16：watchdog 自动取消≠用户取消，明确提示。
            ui_state.view.push_line(
                LineKind::System,
                "run 达到 wall-time 预算被自动取消（非用户取消）；已保留已提交内容".to_string(),
            );
        }
        // §用户诉求：所有结束原因显式提示——此前 Stop/MaxTurns/MaxToolCalls/
        // Cancelled 走 `_ => {}` 完全静默，用户看到"bash 卡片后无下文"却不知为何结束。
        crate::session::CompletionReason::Stop => {
            ui_state
                .view
                .push_line(LineKind::System, "run 完成".to_string());
        }
        crate::session::CompletionReason::MaxTurns => {
            ui_state.view.push_line(
                LineKind::System,
                "run 达到最大模型回合数（limits.max_model_turns）已停止".to_string(),
            );
        }
        crate::session::CompletionReason::MaxToolCalls => {
            ui_state.view.push_line(
                LineKind::System,
                "run 达到工具调用预算（limits.max_tool_calls）已停止".to_string(),
            );
        }
        crate::session::CompletionReason::Cancelled => {
            ui_state
                .view
                .push_line(LineKind::System, "run 已取消（Esc）".to_string());
        }
        // §13（AGENTS.md）：request_input 挂起——不是完成也不是失败；
        // 明确显示模型的问题，提示用户输入回答后继续。多问题渲染为多行：
        // 逐行 push（每行独立 bound），避免单行超长被截断。
        // §13 升级：所有问题都带 options 时同时打开键盘选项选择器
        // （模态覆盖；↑/↓+Enter 选择，Esc 关闭后自由输入）。
        // 块级去重：连续相同挂起提示（中间无用户回答插入）只保留一个块，
        // 避免反复挂起时 transcript 被相同提示刷屏。
        crate::session::CompletionReason::AwaitingUserInput => {
            let awaiting = outcome.awaiting_input.as_ref();
            let question = awaiting
                .map(|a| a.text.as_str())
                .unwrap_or("请提供你的输入");
            let mut block: Vec<String> = Vec::with_capacity(question.lines().count() + 2);
            block.push("⏸ run 挂起，等待你的输入：".to_string());
            block.extend(question.lines().map(String::from));
            block.push(
                "（输入回答后继续；↑/↓ + Enter 可从选项中选择，Esc 关闭后自由输入）".to_string(),
            );
            ui_state.view.push_system_block_dedup(&block);
            // 全部问题都有选项 → 打开键盘选择器（逐题导航，确认后作为回答）。
            // 与提示块去重独立：即使提示被去重（连续挂起），选择器仍要重新打开
            //（用户上次可能 Esc 关闭了）。
            if let Some(questions) = awaiting.map(|a| &a.questions)
                && !questions.is_empty()
                && questions.iter().all(|q| !q.options.is_empty())
            {
                let items: Vec<crate::tui::model::InputChoiceItem> = questions
                    .iter()
                    .map(|q| crate::tui::model::InputChoiceItem {
                        header: q.header.clone(),
                        question: q.question.clone(),
                        options: q.options.clone(),
                    })
                    .collect();
                ui_state.view.input_choice =
                    Some(crate::tui::model::InputChoiceState::new(items));
            }
        }
    }
    ui_state.view.status = StatusLine::Idle;
    ui_state.view.turn = 0;
    renderer
        .draw(&mut ui_state.view)
        .map_err(|e| e.to_string())?;
    Ok(outcome)
}

/// 从 session 读取最近一次 run 内所有成功的 edit（含 unified diff，§18.3 /diff）。
fn last_edit_diff(log: &SessionLog) -> String {
    use crate::session::read_events;
    let events = match read_events(log.path()) {
        Ok(events) => events,
        Err(_) => return "读取 session 失败".to_string(),
    };
    // §PointerHit 8：只聚合**最近一次 RunStarted 之后**的 edit（/diff = 本轮），
    // 避免长会话/resume 后聚合全部历史 edit。
    let recent_start = events
        .iter()
        .rposition(|e| matches!(e, SessionEvent::RunStarted { .. }))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let diffs: Vec<String> = events[recent_start..]
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolCompleted { outcome, .. }
                if matches!(outcome.session_metadata.tool.as_str(), "edit" | "write")
                    && outcome.status == crate::tool::outcome::ToolStatus::Succeeded =>
            {
                outcome.session_metadata.diff.clone()
            }
            _ => None,
        })
        .collect();
    if diffs.is_empty() {
        return "本轮还没有带 diff 的成功 edit/write".to_string();
    }
    let mut out = String::new();
    const MAX_DIFF_BYTES: usize = 48 * 1024;
    for (i, diff) in diffs.iter().enumerate() {
        if i > 0 {
            out.push_str("\n---\n\n");
        }
        let remaining = MAX_DIFF_BYTES.saturating_sub(out.len());
        if diff.len() > remaining {
            let marker = "\n…（diff 超过 48KiB 预算，已截断）";
            let content_budget = remaining.saturating_sub(marker.len());
            let keep = crate::tui::text::floor_char_boundary(diff, content_budget);
            out.push_str(&diff[..keep]);
            if remaining >= marker.len() {
                out.push_str(marker);
            }
            break;
        }
        out.push_str(diff);
    }
    out
}

/// 粘贴快捷键判定（§优化：应用直读剪贴板，不依赖终端 bracketed paste）。
///
/// - Ctrl+V：Windows 标准粘贴；
/// - Shift+Insert：旧式粘贴（部分终端仍以此映射）。
///
/// 支持 bracketed paste 的终端会把这两个键转为 `Event::Paste` 直接送达
/// （键盘线程第一分支转发），不会到这里；这里只处理终端把按键原样转发
/// 的情况（mintty/旧 conhost/SSH 嵌入终端等）。两条路径互斥，不重复粘贴。
fn is_paste_shortcut(event: &Event) -> bool {
    let ratatui::crossterm::event::Event::Key(k) = event else {
        return false;
    };
    let mods = k.modifiers;
    match k.code {
        ratatui::crossterm::event::KeyCode::Char('v')
            if mods.contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
        {
            true
        }
        ratatui::crossterm::event::KeyCode::Insert
            if mods.contains(ratatui::crossterm::event::KeyModifiers::SHIFT) =>
        {
            true
        }
        _ => false,
    }
}

/// Press 与按住产生的 Repeat 都是编辑动作；Release 只用于增强键盘协议，
/// 不应重复插入。此前主循环只收 Press，导致长按退格/方向键完全不重复。
fn is_actionable_key_kind(kind: ratatui::crossterm::event::KeyEventKind) -> bool {
    matches!(
        kind,
        ratatui::crossterm::event::KeyEventKind::Press
            | ratatui::crossterm::event::KeyEventKind::Repeat
    )
}

fn spawn_ctrl_c_handler(current_cancel: Arc<Mutex<Option<CancellationToken>>>) {
    tokio::spawn(async move {
        // P0-4：空闲 Ctrl+C 连按两次退出（2 秒窗口）——与 reducer 的 TUI 双击
        // 语义一致；只覆盖非 raw mode 路径（-p 模式等 Ctrl+C 走 SIGINT 的场景）。
        let exit_armed: Arc<std::sync::atomic::AtomicU64> =
            Arc::new(std::sync::atomic::AtomicU64::new(0));
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            let has_run = crate::util::lock_mutex(&current_cancel, "current_cancel").is_some();
            if has_run {
                if let Some(cancel) =
                    crate::util::lock_mutex(&current_cancel, "current_cancel").clone()
                {
                    cancel.cancel();
                }
            } else {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let first = exit_armed.swap(now_ms, std::sync::atomic::Ordering::SeqCst);
                if first != 0 && now_ms.saturating_sub(first) < 2000 {
                    // 2 秒内第二次：退出。process::exit 不执行析构，必须显式
                    // 恢复终端，否则残留 raw mode（无回显）。
                    restore_terminal_on_exit();
                    std::process::exit(130);
                }
                // 第一次：仅记录，不退出（避免一次误触直接终止）。
            }
        }
    });
}

/// 退出前恢复终端（inline TUI 打开过 raw mode；process::exit 不触发 Drop）。
fn restore_terminal_on_exit() {
    use std::io::Write;
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::Show,
        ratatui::crossterm::event::DisableMouseCapture,
        ratatui::crossterm::event::DisableBracketedPaste,
        ratatui::crossterm::terminal::LeaveAlternateScreen,
        ratatui::crossterm::style::ResetColor,
    );
    let _ = std::io::stdout().flush();
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
}

/// §31：panic hook——先尽力恢复终端，再走默认 panic 输出。
/// 只安装一次（进程级）；恢复逻辑不依赖具体实例（TerminalDriver::restore_global）。
fn install_terminal_panic_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            crate::tui::terminal::TerminalDriver::restore_global();
            default(info);
        }));
    });
}

/// 把 session 事件重建为对话消息（§15.1：session log 是事实源，上下文是投影）。
///
/// §15.4：采用最新 active compaction summary 作为前缀，排除其覆盖的 raw events；
/// 旧 summary 不重复注入。
///
/// 委托 [`crate::session::project_messages`]（P0-3 统一投影语义）；
/// 无 seq 输入时以 index+1 近似 seq（仅用于测试构造的事件数组）。
pub fn session_to_messages(events: &[SessionEvent]) -> Vec<ChatMessage> {
    let with_seq: Vec<(u64, SessionEvent)> = events
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, event)| (index as u64 + 1, event))
        .collect();
    crate::session::project_messages(&with_seq)
}

pub fn parse_session_id(value: &str) -> Result<SessionId, String> {
    let uuid = uuid::Uuid::parse_str(value).map_err(|_| format!("无效 session id: {value}"))?;
    Ok(SessionId(uuid))
}

/// §用户诉求（恢复会话可判断）：`--resume` 支持 UUID 前缀匹配。
///
/// 完整 UUID 直接解析；否则在当前 workspace 的 session 目录里按前缀唯一匹配
/// （不区分大小写）。无匹配/多匹配时给出可操作错误（提示 `tpi sessions` 列表），
/// 不猜测、不歧义恢复。
pub fn resolve_session_id_prefix(
    value: &str,
    sessions_root: &std::path::Path,
    workspace_root: &Utf8PathBuf,
) -> Result<SessionId, String> {
    if let Ok(id) = parse_session_id(value) {
        return Ok(id);
    }
    let prefix = value.trim().to_ascii_lowercase();
    if prefix.is_empty() {
        return Err("session id 为空；用 `tpi sessions` 列出可恢复会话".into());
    }
    let dir = sessions_root.join(session::workspace_id_for(workspace_root.as_std_path()));
    let mut matches: Vec<String> = Vec::new();
    if dir.exists() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let Ok(entry) = entry else { continue };
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if stem.to_ascii_lowercase().starts_with(&prefix) && uuid::Uuid::parse_str(stem).is_ok()
            {
                matches.push(stem.to_string());
            }
        }
    }
    match matches.len() {
        1 => parse_session_id(&matches[0]),
        0 => Err(format!(
            "没有找到以 `{prefix}` 开头的 session；用 `tpi sessions` 列出全部会话"
        )),
        _ => Err(format!(
            "`{prefix}` 匹配 {} 个 session，请提供更长前缀：{}（`tpi sessions` 可查看完整 id）",
            matches.len(),
            matches.join(", ")
        )),
    }
}

/// 当前 workspace 最近的 session（§18.3 `--continue`）。
fn latest_session_id(
    sessions_root: &std::path::Path,
    workspace_root: &Utf8PathBuf,
) -> Result<SessionId, String> {
    let sessions = list_sessions(sessions_root, workspace_root)?;
    sessions
        .first()
        .map(|(id, _, _, _)| *id)
        .ok_or_else(|| "当前 workspace 没有历史 session".into())
}

/// 列出当前 workspace 的全部 session（按修改时间倒序）：
/// (id, 最后修改, 事件数, 首条用户消息预览)。
/// `pub`：CLI `tpi sessions` 复用同一列表（§用户诉求：恢复前可浏览）。
pub fn list_sessions(
    sessions_root: &std::path::Path,
    workspace_root: &Utf8PathBuf,
) -> Result<Vec<(SessionId, std::time::SystemTime, usize, String)>, String> {
    let workspace_id = session::workspace_id_for(workspace_root.as_std_path());
    let dir = sessions_root.join(&workspace_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    const MAX_SESSION_CHOICES: usize = 100;
    let mut candidates: Vec<(std::time::SystemTime, SessionId, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        if entry
            .path()
            .extension()
            .map(|e| e == "jsonl")
            .unwrap_or(false)
            && let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && let Some(name) = entry.path().file_stem().and_then(|s| s.to_str())
            && let Ok(id) = parse_session_id(name)
        {
            candidates.push((modified, id, entry.path()));
        }
    }
    candidates.sort_by_key(|(time, _, _)| std::cmp::Reverse(*time));
    candidates.truncate(MAX_SESSION_CHOICES);
    Ok(candidates
        .into_iter()
        .map(|(modified, id, path)| {
            let count = count_jsonl_lines(&path);
            let preview = first_user_preview(&path);
            (id, modified, count, preview)
        })
        .collect())
}

/// §用户诉求（恢复会话可判断）：恢复时显示人类可读标识——首条用户消息
/// 预览 + 短 id，而不是让用户面对完整 UUID 哈希。
fn session_resume_label(log: Option<&SessionLog>) -> String {
    let Some(log) = log else {
        return "已恢复会话".to_string();
    };
    let id = log.session_id().to_string();
    let short: String = id.chars().take(13).collect(); // 019feea2-e01d
    let preview = first_user_preview(log.path());
    if preview.is_empty() {
        format!("已恢复会话（{short}…）")
    } else {
        format!("已恢复会话「{preview}」（{short}…）")
    }
}

/// P2：从 session 文件提取首条用户消息摘要（≤40 字符，单行）。
/// UX/性能：只流式读文件头部（≤500 行），不解析整个 session——
/// 长会话下 `/sessions` 列表保持轻量（此前 read_events 解析全部事件）。
fn first_user_preview(path: &std::path::Path) -> String {
    const MAX_LINES: usize = 500;
    const MAX_PREVIEW_EVENT_BYTES: usize = 1024 * 1024;
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut reader = std::io::BufReader::new(file);
    let mut lines_read = 0usize;
    loop {
        let line = match crate::util::read_line_bounded(&mut reader, MAX_PREVIEW_EVENT_BYTES) {
            Ok(crate::util::BoundedLineRead::Eof) | Err(_) => break,
            Ok(crate::util::BoundedLineRead::TooLong) => {
                lines_read = lines_read.saturating_add(1);
                if lines_read >= MAX_LINES {
                    break;
                }
                continue;
            }
            Ok(crate::util::BoundedLineRead::Line(line)) => line.bytes,
        };
        lines_read = lines_read.saturating_add(1);
        if lines_read > MAX_LINES {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(envelope) = serde_json::from_slice::<crate::session::Envelope>(&line) else {
            continue;
        };
        if let crate::session::EventBody::UserSubmitted { payload } = envelope.body {
            let content = payload.content;
            let one_line = content.lines().next().unwrap_or_default();
            let truncated: String = one_line.chars().take(40).collect();
            return if one_line.chars().count() > 40 {
                format!("{truncated}…")
            } else {
                truncated
            };
        }
    }
    String::new()
}
/// 会话对话预览（§用户诉求：/sessions 菜单内预览选中会话）：流式读文件头部，
/// 只收集 UserSubmitted 与 AssistantMessageCommitted 消息（不含工具/系统事件），
/// 每条取首行前 [`PREVIEW_LINE_CHARS`] 字符；最多 [`PREVIEW_MAX_LINES`] 条。
/// 与 [`first_user_preview`] 同一有界读策略（≤500 行 / 1 MiB 预算）。
fn session_dialogue_preview(path: &std::path::Path) -> Vec<crate::tui::model::MenuPreviewLine> {
    const MAX_LINES: usize = 500;
    const MAX_PREVIEW_EVENT_BYTES: usize = 1024 * 1024;
    const PREVIEW_MAX_LINES: usize = 6;
    const PREVIEW_LINE_CHARS: usize = 60;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(file);
    let mut out: Vec<crate::tui::model::MenuPreviewLine> = Vec::new();
    let mut lines_read = 0usize;
    loop {
        let line = match crate::util::read_line_bounded(&mut reader, MAX_PREVIEW_EVENT_BYTES) {
            Ok(crate::util::BoundedLineRead::Eof) | Err(_) => break,
            Ok(crate::util::BoundedLineRead::TooLong) => {
                lines_read = lines_read.saturating_add(1);
                if lines_read >= MAX_LINES {
                    break;
                }
                continue;
            }
            Ok(crate::util::BoundedLineRead::Line(line)) => line.bytes,
        };
        lines_read = lines_read.saturating_add(1);
        if lines_read > MAX_LINES || out.len() >= PREVIEW_MAX_LINES {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(envelope) = serde_json::from_slice::<crate::session::Envelope>(&line) else {
            continue;
        };
        let content = match envelope.body {
            crate::session::EventBody::UserSubmitted { payload } => Some((true, payload.content)),
            crate::session::EventBody::AssistantMessageCommitted { payload } => {
                Some((false, payload.message.content))
            }
            _ => None,
        };
        let Some((is_user, content)) = content else {
            continue;
        };
        // §用户诉求（菜单预览净化）：preview 是纯文本渲染，历史消息含
        // markdown 代码围栏（```/~~~）时不得露出围栏标记或代码内容当标题。
        // 遍历行并跟踪围栏开关：围栏行与其间的代码行都跳过，取第一条有效文本。
        let mut in_fence = false;
        let mut first_line = "";
        for line in content.lines().map(str::trim) {
            if line.starts_with("```") || line.starts_with("~~~") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence || line.is_empty() {
                continue;
            }
            first_line = line;
            break;
        }
        if first_line.is_empty() {
            continue;
        }
        let chars: String = first_line.chars().take(PREVIEW_LINE_CHARS).collect();
        let text = if first_line.chars().count() > PREVIEW_LINE_CHARS {
            format!("{chars}…")
        } else {
            chars
        };
        out.push(crate::tui::model::MenuPreviewLine { is_user, text });
    }
    out
}

/// 把对话预览行格式化为 Modal 正文（§用户诉求：/sessions 悬浮窗显示预览）。
/// 每行 `你 {text}` / `AI {text}`，draw_modal 按前缀着色；空预览给占位提示。
pub fn preview_lines_to_body(lines: &[crate::tui::model::MenuPreviewLine]) -> String {
    if lines.is_empty() {
        return "（无对话记录）".to_string();
    }
    let mut out = String::new();
    for line in lines {
        let prefix = if line.is_user { "你 " } else { "AI " };
        out.push_str(prefix);
        out.push_str(&line.text);
        out.push('\n');
    }
    out.pop(); // 去掉末尾换行
    out
}

/// P1-13：事件数 = JSONL 行数（每行一个事件，§14.2）；只数行不 serde 解析，
/// session 增多时 `/sessions` 列表仍保持轻量（此前对每个文件解析全部事件）。
fn count_jsonl_lines(path: &std::path::Path) -> usize {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut buffer = [0u8; 64 * 1024];
    let mut count = 0usize;
    let mut line_has_content = false;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for byte in &buffer[..read] {
            if *byte == b'\n' {
                if line_has_content {
                    count = count.saturating_add(1);
                }
                line_has_content = false;
            } else if !byte.is_ascii_whitespace() {
                line_has_content = true;
            }
        }
    }
    if line_has_content {
        count = count.saturating_add(1);
    }
    count
}

/// 模型单价格式化（§16.2）：None → "未配置"；有值 → "$X"。
fn fmt_price(price: Option<f64>) -> String {
    match price {
        Some(p) => format!("${p}"),
        None => "未配置".into(),
    }
}

/// 会话菜单展示用的时间（MM-DD HH:MM）：跨天会话带日期，一眼可分辨。
/// 非 UTC 偏移 = 本机时间（mtime 已是本地语义）。
fn fmt_time_short(t: std::time::SystemTime) -> String {
    let dt = time::OffsetDateTime::from(t);
    format!(
        "{:02}-{:02} {:02}:{:02}",
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::ViewModel;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn key_repeat_is_an_edit_event_but_release_is_not() {
        assert!(is_actionable_key_kind(KeyEventKind::Press));
        assert!(is_actionable_key_kind(KeyEventKind::Repeat));
        assert!(!is_actionable_key_kind(KeyEventKind::Release));
    }

    /// §用户诉求：--resume 前缀匹配——完整 UUID 直解、唯一前缀补全（大小写
    /// 不敏感）、多匹配/无匹配给出可操作错误，不歧义恢复。
    #[test]
    fn resume_prefix_matches_uniquely() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_root = dir.path().join("sessions");
        let workspace_root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let wid = session::workspace_id_for(workspace_root.as_std_path());
        let sd = sessions_root.join(&wid);
        std::fs::create_dir_all(&sd).unwrap();
        let id1 = "019fda3c-c299-7890-b9ab-4cc7f311475a";
        let id2 = "019fdabf-2250-7310-b988-673a49d1c675";
        std::fs::write(sd.join(format!("{id1}.jsonl")), "").unwrap();
        std::fs::write(sd.join(format!("{id2}.jsonl")), "").unwrap();
        // 完整 UUID 直接解析。
        assert_eq!(
            resolve_session_id_prefix(id1, &sessions_root, &workspace_root)
                .unwrap()
                .to_string(),
            id1
        );
        // 唯一前缀补全（大小写不敏感）。
        assert_eq!(
            resolve_session_id_prefix("019FDA3C", &sessions_root, &workspace_root)
                .unwrap()
                .to_string(),
            id1
        );
        // 多匹配：报错列出候选，不猜测。
        let err = resolve_session_id_prefix("019fd", &sessions_root, &workspace_root).unwrap_err();
        assert!(err.contains("匹配 2 个"), "{err}");
        // 无匹配：给出可操作提示。
        let err = resolve_session_id_prefix("zzz", &sessions_root, &workspace_root).unwrap_err();
        assert!(err.contains("没有找到"), "{err}");
        // 空输入。
        let err = resolve_session_id_prefix("", &sessions_root, &workspace_root).unwrap_err();
        assert!(err.contains("为空"), "{err}");
    }

    /// P1-13：事件数 = JSONL 行数（不 serde 解析全部事件）。
    #[test]
    fn count_jsonl_lines_counts_events_not_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{}\n{}\n\n{}\n").unwrap();
        assert_eq!(count_jsonl_lines(&path), 3, "空行不计，行数即事件数");
        std::fs::write(&path, "this is not json\n").unwrap();
        assert_eq!(count_jsonl_lines(&path), 1, "损坏行仍计入（列表展示用）");
    }

    /// P2：/sessions 预览——首条用户消息摘要（≤40 字符、单行）。
    #[test]
    fn first_user_preview_is_bounded_and_single_line() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut log = crate::session::SessionLog::create(
            &dir.path().join("sessions"),
            workspace.as_std_path(),
            crate::ids::RunId::new_v7(),
        )
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: format!("{}\n第二行", "长消息".repeat(20)),
        })
        .unwrap();
        log.append_event(&SessionEvent::UserSubmitted {
            content: "later".into(),
        })
        .unwrap();
        let preview = first_user_preview(log.path());
        assert!(preview.chars().count() <= 41, "预览必须截断: {preview}");
        assert!(!preview.contains('\n'), "预览必须单行: {preview}");
        assert!(preview.ends_with('…'), "超长必须带省略号");
    }

    /// P2：/diff 聚合本 run 内所有成功的 edit（此前只返回最近一次）。
    #[test]
    fn last_edit_diff_aggregates_all_successful_edits() {
        use crate::tool::outcome::{ModelPayload, StoredToolOutcome, ToolMetadata, ToolStatus};
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut log = crate::session::SessionLog::create(
            &dir.path().join("sessions"),
            workspace.as_std_path(),
            crate::ids::RunId::new_v7(),
        )
        .unwrap();
        let edit_outcome = |output: &str, status: ToolStatus| StoredToolOutcome {
            status,
            model_payload: ModelPayload {
                status,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: output.to_string(),
                effect: None,
                artifact: None,
            },
            session_metadata: ToolMetadata {
                tool: "edit".into(),
                target: Some("a.rs".into()),
                diff: Some(output.to_string()),
                ..Default::default()
            },
        };
        // 成功 1（带 diff）、失败（不应出现）、成功 2。
        let append_edit =
            |log: &mut crate::session::SessionLog, output: &str, status: ToolStatus| {
                let call_id = crate::ids::ToolCallId::new_v7();
                log.append_event(&SessionEvent::ToolRequested {
                    call: crate::provider::ToolCall {
                        call_id,
                        provider_id: format!("provider-{call_id}"),
                        name: "edit".into(),
                        arguments: "{}".into(),
                    },
                })?;
                log.append_event(&SessionEvent::ToolCompleted {
                    call_id,
                    outcome: edit_outcome(output, status),
                })
            };
        append_edit(&mut log, "diff-one", ToolStatus::Succeeded).unwrap();
        append_edit(&mut log, "diff-failed", ToolStatus::Failed).unwrap();
        append_edit(&mut log, "diff-two", ToolStatus::Succeeded).unwrap();
        let text = last_edit_diff(&log);
        assert!(
            text.contains("diff-one"),
            "第一个成功 edit 必须出现: {text}"
        );
        assert!(
            text.contains("diff-two"),
            "第二个成功 edit 必须出现: {text}"
        );
        assert!(
            !text.contains("diff-failed"),
            "失败的 edit 不得出现在聚合里: {text}"
        );
    }

    /// P0-5 回归：Tab 补全必须同步到 editor（此前只改 view.input，
    /// 主循环 `view.input = editor.text()` 会把补全结果覆盖掉，Enter 提交原文）。
    /// T3：走 reducer 单向流（UiEvent::Key → UiState）。
    #[test]
    fn menu_completion_syncs_editor_text() {
        use crate::tui::event::UiEvent;
        use crate::tui::reducer;
        let mut state = UiState::new(ViewModel::default());
        for ch in "/set".chars() {
            reducer::update(
                &mut state,
                UiEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
            );
        }
        assert!(state.view.menu.is_some(), "输入 /set 应弹出命令菜单");
        reducer::update(
            &mut state,
            UiEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        assert_eq!(
            state.editor.text(),
            "/settings",
            "Tab 补全必须同步 editor（当前: {}）",
            state.editor.text()
        );
        reducer::update(
            &mut state,
            UiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(
            state.pop_pending().as_deref(),
            Some("/settings"),
            "Enter 提交的必须是补全后的命令"
        );
    }
}

/// UX：首条用户消息预览只读文件头部——超过 500 行才出现的 UserSubmitted 不读取
/// （长会话 `/sessions` 列表保持轻量，不解析整个 session）。
#[test]
fn first_user_preview_is_bounded_to_file_head() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.jsonl");
    let mut content = String::new();
    // 600 个非用户事件（合法 envelope 行），UserSubmitted 在第 600 行之后。
    for i in 0..600 {
        let envelope = serde_json::json!({
            "schema": 1,
            "seq": i + 1,
            "event_id": format!("event-{i}"),
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": "00000000-0000-0000-0000-000000000000",
            "run_id": "00000000-0000-0000-0000-000000000001",
            "type": "run_started",
            "payload": {"model": {"name": "m", "provider": "p"}, "limits": {"max_turns": 1, "max_tool_calls": 1}}
        });
        content.push_str(&serde_json::to_string(&envelope).unwrap());
        content.push('\n');
    }
    content.push_str(
        &serde_json::to_string(&serde_json::json!({
            "schema": 1,
            "seq": 601,
            "event_id": "event-600",
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": "00000000-0000-0000-0000-000000000000",
            "run_id": "00000000-0000-0000-0000-000000000001",
            "type": "user_submitted",
            "payload": {"content": "太晚的消息"}
        }))
        .unwrap(),
    );
    content.push('\n');
    std::fs::write(&path, content).unwrap();
    assert_eq!(
        first_user_preview(&path),
        "",
        "超过头部预算的 UserSubmitted 不应被读取（保持 /sessions 轻量）"
    );
}

#[test]
fn first_user_preview_skips_a_whole_oversized_line_without_losing_alignment() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(&vec![b'x'; 1024 * 1024 + 1]).unwrap();
    file.write_all(b"\n").unwrap();
    let envelope = serde_json::json!({
        "schema": 1,
        "seq": 1,
        "event_id": "00000000-0000-7000-8000-000000000001",
        "timestamp": "2026-01-01T00:00:00Z",
        "session_id": "00000000-0000-7000-8000-000000000002",
        "run_id": "00000000-0000-7000-8000-000000000003",
        "type": "user_submitted",
        "payload": {"content": "仍可读取"}
    });
    writeln!(file, "{}", serde_json::to_string(&envelope).unwrap()).unwrap();
    drop(file);

    assert_eq!(first_user_preview(&path), "仍可读取");
}

/// §用户诉求：/sessions 菜单对话预览——只取 User 与 AI 消息（工具/系统事件
/// 跳过），角色标记正确，多行消息取首行并截断，超过 6 条封顶。
#[test]
fn session_dialogue_preview_collects_user_and_ai_only() {
    use crate::tui::model::MenuPreviewLine;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dialogue.jsonl");
    let mut content = String::new();
    let env = |seq: u64, ty: &str, payload: serde_json::Value| {
        serde_json::to_string(&serde_json::json!({
            "schema": 1,
            "seq": seq,
            "event_id": format!("00000000-0000-7000-8000-{:012}", seq),
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": "00000000-0000-7000-8000-000000000000",
            "run_id": "00000000-0000-7000-8000-000000000001",
            "type": ty,
            "payload": payload
        }))
        .unwrap()
    };
    // 1 user、1 assistant（多行）、1 工具事件（应跳过）、1 assistant、超长 user。
    content.push_str(&env(
        1,
        "user_submitted",
        serde_json::json!(
            {"content": "帮我看看这个仓库"}
        ),
    ));
    content.push('\n');
    content.push_str(&env(
        2,
        "assistant_message_committed",
        serde_json::json!(
            {"message": {"content": "好的，我先看下结构\n第二行", "tool_calls": []}}
        ),
    ));
    content.push('\n');
    content.push_str(&env(3, "tool_completed", serde_json::json!(
        {"call_id": "c1", "outcome": {"status": "succeeded", "program": "bash", "exit_code": 0, "duration_ms": 1, "output": "xxx", "effect": null, "artifact": null}}
    )));
    content.push('\n');
    content.push_str(&env(
        4,
        "assistant_message_committed",
        serde_json::json!(
            {"message": {"content": "结论：这是一个 Rust 项目", "tool_calls": []}}
        ),
    ));
    content.push('\n');
    let long_user = "x".repeat(200);
    content.push_str(&env(
        5,
        "user_submitted",
        serde_json::json!(
            {"content": long_user}
        ),
    ));
    content.push('\n');
    std::fs::write(&path, content).unwrap();

    let preview = session_dialogue_preview(&path);
    let texts: Vec<(bool, &str)> = preview
        .iter()
        .map(|MenuPreviewLine { is_user, text }| (*is_user, text.as_str()))
        .collect();
    assert_eq!(
        texts,
        vec![
            (true, "帮我看看这个仓库"),
            (false, "好的，我先看下结构"),
            (false, "结论：这是一个 Rust 项目"),
            (true, &format!("{}…", "x".repeat(60))),
        ],
        "只取 User/AI 首行、工具跳过、多行取首行、超长截断: {texts:?}"
    );
    assert!(
        preview.iter().all(|l| !l.text.is_empty()),
        "空内容消息不进入预览"
    );
}

/// §用户诉求：对话预览有界——收集满 6 条即停（长会话不解析整个文件）。
#[test]
fn session_dialogue_preview_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("many.jsonl");
    let mut content = String::new();
    for i in 0..20 {
        let envelope = serde_json::json!({
            "schema": 1,
            "seq": i + 1,
            "event_id": format!("00000000-0000-7000-8000-{:012}", i + 1),
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": "00000000-0000-7000-8000-000000000000",
            "run_id": "00000000-0000-7000-8000-000000000001",
            "type": "user_submitted",
            "payload": {"content": format!("消息 {i}")}
        });
        content.push_str(&serde_json::to_string(&envelope).unwrap());
        content.push('\n');
    }
    std::fs::write(&path, content).unwrap();
    assert_eq!(
        session_dialogue_preview(&path).len(),
        6,
        "预览最多 6 条（长会话不解析整个文件）"
    );
}

/// §用户诉求（菜单预览净化）：历史消息以 markdown 代码围栏开头时，预览
/// 不得露出 ``` / ~~~ 标记——跳过围栏与空行，取第一条有效文本作标题。
#[test]
fn session_dialogue_preview_strips_code_fences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fence.jsonl");
    let env = |seq: u64, ty: &str, content: &str| {
        serde_json::json!({
            "schema": 1,
            "seq": seq,
            "event_id": format!("00000000-0000-7000-8000-{:012}", seq),
            "timestamp": "2026-01-01T00:00:00Z",
            "session_id": "00000000-0000-7000-8000-000000000000",
            "run_id": "00000000-0000-7000-8000-000000000001",
            "type": ty,
            "payload": {"content": content}
        })
    };
    let mut content = String::new();
    content.push_str(
        &serde_json::to_string(&env(
            1,
            "user_submitted",
            "```toml\n[agent.limits]\nmax_turns = 0\n```",
        ))
        .unwrap(),
    );
    content.push('\n');
    // assistant 事件的 payload 是 {message: {content}}——需包装 message 字段。
    let assistant_env = serde_json::json!({
        "schema": 1,
        "seq": 2,
        "event_id": "00000000-0000-7000-8000-000000000002",
        "timestamp": "2026-01-01T00:00:00Z",
        "session_id": "00000000-0000-7000-8000-000000000000",
        "run_id": "00000000-0000-7000-8000-000000000001",
        "type": "assistant_message_committed",
        "payload": {"message": {"content": "~~~bash\necho hi\n~~~\n然后我做了 X", "tool_calls": []}}
    });
    content.push_str(&serde_json::to_string(&assistant_env).unwrap());
    content.push('\n');
    std::fs::write(&path, content).unwrap();

    let preview = session_dialogue_preview(&path);
    let texts: Vec<&str> = preview.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["然后我做了 X"],
        "纯代码块消息无标题跳过，围栏标记与代码内容不得当标题: {texts:?}"
    );
    assert!(
        preview
            .iter()
            .all(|l| !l.text.contains("```") && !l.text.contains("~~~")),
        "预览不得含围栏标记: {texts:?}"
    );
}
