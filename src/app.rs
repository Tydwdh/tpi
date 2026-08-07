//! 应用层（文档 §4.1）：输入路由、生命周期。
//!
//! 活动 run 中的输入路由（§6.2）：
//! - 普通 Enter：输入排队（pending_message），**当前 run 完成后**作为下一条
//!   消息提交（§12 稳定化任务书：不是 run 内的 boundary steering）；
//! - Ctrl-C：取消当前 run，保留 session；空闲时退出。
//!
//! M5：Ratatui inline renderer——只有 renderer 写 stdout（§3.2 不变量 11、
//! §16.1），16 ms 帧合并，synchronized update。
//! M6+：键盘由独立线程持续读取（运行中也响应输入/翻页/折叠），
//! 命令补全菜单（Tab）、输入历史（↑/↓）、多行输入（Alt+Enter）、
//! 思考折叠（Alt+T）、动画时钟（spinner）。

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent;
use crate::config::Config;
use crate::ids::{RunId, SessionId};
use crate::provider::openai_compat::OpenAiCompatClient;
use crate::provider::{ChatMessage, Provider};
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

/// 应用入口。
pub async fn run(
    config: Config,
    session_target: SessionTarget,
    prompt: &str,
    non_interactive: bool,
    no_session: bool,
) -> Result<(), String> {
    let mut config = config;
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

    let (mut session, history) = match session_target {
        SessionTarget::New => (None, Vec::new()),
        SessionTarget::Continue => {
            let session_id = latest_session_id(&sessions_root, &workspace_root)?;
            resume_session(&sessions_root, &workspace_root, session_id)?
        }
        SessionTarget::Resume(id) => {
            let session_id = parse_session_id(&id)?;
            resume_session(&sessions_root, &workspace_root, session_id)?
        }
    };

    if non_interactive {
        if prompt.is_empty() {
            return Err("非交互模式需要提供 prompt：tpi -p \"...\"".into());
        }
        let message = prompt.to_string();
        let mut session = match session {
            Some(session) => session,
            None => create_session(&sessions_root, &workspace_root)?,
        };
        let outcome = run_prompt_once(
            &mut provider,
            &mut session,
            &config,
            &history,
            message,
            current_cancel.clone(),
        )
        .await?;
        // §18.3：`-p` 模式 stdout 只输出最终答案。
        if !outcome.assistant_text.is_empty() {
            println!("{}", outcome.assistant_text);
        }
        return Ok(());
    }

    let mut history = history;
    interactive_loop(
        &mut provider,
        &mut session,
        &config,
        &mut history,
        prompt,
        current_cancel.clone(),
    )
    .await
}

/// 把一次 run 的 outcome 合并进调用方持有的对话历史（P0-1：唯一合并入口）。
///
/// `AgentOutcome.messages` 的语义是**完整 context**（agent 从调用方传入的
/// history 复制构造并逐步追加，compaction 后还可能重建），因此调用方必须
/// **replace** 而不是 extend——extend 会让旧历史每轮整体重复（token 膨胀、
/// 模型看到重复 user/tool 信息）。interactive 与 -p 模式共用本入口。
pub fn merge_outcome_history(history: &mut Vec<ChatMessage>, outcome: &agent::AgentOutcome) {
    *history = outcome.messages.clone();
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
    *current_cancel.lock().unwrap() = Some(cancel.clone());
    let (ui_tx, mut ui_rx) = mpsc::channel(128);
    // P0-1：`-p` 模式没有 TUI 消费 UI 事件；直接丢弃 rx 会在 channel 满后
    // 让 agent 的 `ui.send().await` 永久等待（挂死）。drain task 持续消费。
    tokio::spawn(async move { while ui_rx.recv().await.is_some() {} });
    let outcome = agent::run(
        provider,
        session,
        config,
        history,
        message,
        ui_tx,
        cancel.clone(),
        false,
        false,
    )
    .await
    .map_err(|failure| failure.to_string())?;
    *current_cancel.lock().unwrap() = None;
    if outcome.reason == crate::session::CompletionReason::Error {
        return Err("run 以 Error 结束（长度限制/内容过滤/协议错误，见 session 记录）".into());
    }
    Ok(outcome)
}

/// 交互主循环（§16：只有 renderer 写 stdout；帧合并 + synchronized update）。
///
/// 键盘由独立线程持续读取（运行中也响应输入/翻页/折叠，对标成熟 TUI Agent）；
/// 输入事件立即标 dirty，动画时钟只在 run 中推进（§16.1）。
async fn interactive_loop<P: Provider>(
    provider: &mut P,
    session: &mut Option<SessionLog>,
    config: &Config,
    history: &mut Vec<ChatMessage>,
    initial_prompt: &str,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<(), String> {
    use crate::tui::SLASH_COMMANDS;
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
        ..Default::default()
    };
    view.push_line(
        LineKind::System,
        "TPI：/help 查看命令与快捷键 · Ctrl-C 取消当前 run",
    );
    // §26-27：UiState 是 UI 单一事实源；交互循环只做
    // event → reducer → effects → draw（T3）。
    let mut ui_state = UiState::new(view);
    // 排队语义（§12 稳定化任务书）：运行中输入先存 ui_state.pending_message，
    // 当前 run 完成后由主循环作为下一条消息提交——不是 run 内的
    // boundary steering。
    if !initial_prompt.is_empty() {
        ui_state.pending_message = Some(initial_prompt.to_string());
    }
    renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;

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
            *index_slot.lock().unwrap() = Some(files);
        });
    }

    // 键盘线程：整个交互期间由独立线程读取 crossterm 事件（§16.1：模型生成中
    // 也响应输入/翻页/折叠；输入不因生成被阻塞）。
    let (key_tx, mut key_rx) = mpsc::channel::<Event>(128);
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if key_tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    loop {
        // 处理键盘事件（空闲时阻塞等待，不空转）。
        let mut need_draw = false;
        tokio::select! {
            event = key_rx.recv() => {
                match event {
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        need_draw = true;
                        let effects = crate::tui::reducer::update(&mut ui_state, UiEvent::Key(key));
                        for effect in effects {
                            execute_ui_effect(effect, &mut ui_state, current_cancel.clone());
                        }
                    }
                    Some(Event::Paste(text)) => {
                        need_draw = true;
                        crate::tui::reducer::update(&mut ui_state, UiEvent::Paste(text));
                    }
                    // 鼠标：滚轮翻页、点击工具卡片展开（空闲态）；
                    // hit-test 依赖 Renderer，在 app 层解析后送语义化事件。
                    Some(Event::Mouse(mouse)) => {
                        use ratatui::crossterm::event::MouseEventKind;
                        let event = match mouse.kind {
                            MouseEventKind::ScrollUp => Some(UiEvent::MouseScrollUp),
                            MouseEventKind::ScrollDown => Some(UiEvent::MouseScrollDown),
                            MouseEventKind::Down(_) => {
                                if ui_state.view.overlay.is_some() {
                                    // Overlay 打开时点击外部不动作。
                                    None
                                } else if let Some(target) =
                                    renderer.hit_target(mouse.column, mouse.row)
                                {
                                    match target {
                                        crate::tui::HitTarget::Tool(id) => {
                                            Some(UiEvent::ClickTool(id))
                                        }
                                        crate::tui::HitTarget::Reasoning(index) => {
                                            Some(UiEvent::ClickReasoning(index))
                                        }
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some(event) = event {
                            need_draw = true;
                            crate::tui::reducer::update(&mut ui_state, event);
                        }
                    }
                    Some(Event::Resize(_, _)) => {
                        renderer.autoresize().map_err(|e| e.to_string())?;
                        need_draw = true;
                    }
                    _ => {}
                }
            }
        }

        // `@` 文件索引就绪（P0-3：共享状态顺带检查，不占 select 分支）。
        if let Some(files) = file_index.lock().unwrap().take() {
            ui_state.view.file_index = files;
            need_draw = true;
        }

        // 空闲时不重绘：否则没有任何状态变化也会以 60 FPS 占用终端和 CPU。
        if need_draw {
            renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
        }

        // 会话恢复选择（/sessions 菜单 Enter）。
        if let Some(session_id) = ui_state.pending_session.take() {
            match parse_session_id(&session_id) {
                Ok(id) => match resume_session(&config.sessions_root, &config.workspace_root, id) {
                    Ok((new_session, new_history)) => {
                        *session = new_session;
                        *history = new_history;
                        ui_state.view.push_line(
                            LineKind::System,
                            format!("已恢复 session {id}（对话历史已加载）"),
                        );
                    }
                    Err(error) => {
                        ui_state
                            .view
                            .push_line(LineKind::System, format!("恢复失败: {error}"));
                    }
                },
                Err(error) => ui_state.view.push_line(LineKind::System, error),
            }
            ui_state.view.menu = None;
            renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
        }

        // 有提交的消息：运行。
        if let Some(message) = ui_state.pending_message.take() {
            match message.as_str() {
                "/quit" | "/exit" => break,
                "/settings" => {
                    let shell = config
                        .shell_path
                        .as_ref()
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "未配置（自动查找 Git Bash）".to_string());
                    let brave_key = if std::env::var(&config.web_brave_key_env).is_ok() {
                        "已配置 key"
                    } else {
                        "未配置 key"
                    };
                    push_system_line(
                        &mut ui_state.view,
                        &mut renderer,
                        format!(
                            "配置来源: {}\nworkspace: {}\nsessions: {}\nartifacts: {}\nshell: {shell}\nweb_search: Brave（{brave_key}）\n自动打开浏览器: {}\n保留 token: {}",
                            config.source,
                            config.workspace_root,
                            config.sessions_root.display(),
                            config.artifacts_root.display(),
                            if config.auto_open_browser {
                                "是"
                            } else {
                                "否"
                            },
                            config.safety_reserve_tokens,
                        ),
                    )?;
                    continue;
                }
                "/model" => {
                    push_system_line(
                        &mut ui_state.view,
                        &mut renderer,
                        format!(
                            "primary:\n  名称: {}\n  provider: {}\n  base_url: {}\n  reasoning: {}\n  max_output_tokens: {}\n  context_window: {}\n  api_key_env: {}",
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
                    )?;
                    continue;
                }
                "/help" => {
                    let mut text = String::from("命令：\n");
                    for (name, desc) in SLASH_COMMANDS {
                        text.push_str(&format!("/{name} —— {desc}\n"));
                    }
                    text.push_str(
                        "快捷键：Alt+Enter 换行 · ↑/↓ 输入历史 · Tab 命令补全 · \
                         @文件 引用补全 · Alt+T 思考折叠 · Alt+E 展开工具输出 · \
                         Ctrl+U 清空 · Ctrl+A/E 行首/行尾 · PgUp/PgDn 翻页 · \
                         滚轮滚动 · 点击工具卡片展开 · Ctrl-C 取消 run",
                    );
                    push_system_line(&mut ui_state.view, &mut renderer, text)?;
                    continue;
                }
                "/session" => {
                    let info = match session.as_ref() {
                        Some(log) => format!(
                            "session: {}\nworkspace: {}\n事件数: {}",
                            log.session_id(),
                            log.workspace_id(),
                            log.seq()
                        ),
                        None => "尚无 session（第一条消息后创建）".to_string(),
                    };
                    push_system_line(&mut ui_state.view, &mut renderer, info)?;
                    continue;
                }
                "/sessions" => {
                    // 会话浏览器：列出当前 workspace 的 session，Enter 恢复。
                    let sessions =
                        match list_sessions(&config.sessions_root, &config.workspace_root) {
                            Ok(sessions) => sessions,
                            Err(error) => {
                                push_system_line(
                                    &mut ui_state.view,
                                    &mut renderer,
                                    format!("无法列出 session: {error}"),
                                )?;
                                continue;
                            }
                        };
                    if sessions.is_empty() {
                        push_system_line(
                            &mut ui_state.view,
                            &mut renderer,
                            "当前 workspace 没有历史 session".to_string(),
                        )?;
                        continue;
                    }
                    ui_state.view.menu = Some(crate::tui::model::MenuView {
                        items: sessions
                            .iter()
                            .map(|(id, modified, count, preview)| {
                                let label = if preview.is_empty() {
                                    format!("{} · {} 事件", fmt_time(*modified), count)
                                } else {
                                    format!(
                                        "{} · {} 事件 · {}",
                                        fmt_time(*modified),
                                        count,
                                        preview
                                    )
                                };
                                (id.to_string(), label)
                            })
                            .collect(),
                        selected: 0,
                        kind: crate::tui::model::MenuKind::Session,
                    });
                    push_system_line(
                        &mut ui_state.view,
                        &mut renderer,
                        "会话列表：↑/↓ 选择，Enter 恢复（Esc 关闭）".to_string(),
                    )?;
                    continue;
                }
                "/new" => {
                    *session = None;
                    history.clear();
                    push_system_line(
                        &mut ui_state.view,
                        &mut renderer,
                        "已开始新会话".to_string(),
                    )?;
                    continue;
                }
                "/cancel" => {
                    if let Some(cancel) = current_cancel.lock().unwrap().clone() {
                        cancel.cancel();
                        push_system_line(
                            &mut ui_state.view,
                            &mut renderer,
                            "已发送取消（§11.5：保留 session）".to_string(),
                        )?;
                    } else {
                        push_system_line(
                            &mut ui_state.view,
                            &mut renderer,
                            "当前没有正在运行的 run".to_string(),
                        )?;
                    }
                    continue;
                }
                "/thinking" => {
                    let value = config
                        .model
                        .reasoning
                        .clone()
                        .unwrap_or_else(|| "未配置（默认）".to_string());
                    push_system_line(
                        &mut ui_state.view,
                        &mut renderer,
                        format!(
                            "reasoning: {value}\n说明: 透传给 provider 的推理设置（§18.1 [model.primary] reasoning）；\n未配置时使用 provider 默认。",
                        ),
                    )?;
                    continue;
                }
                "/diff" => {
                    let diff = match session.as_ref() {
                        Some(log) => last_edit_diff(log),
                        None => "尚无 session".to_string(),
                    };
                    push_system_line(&mut ui_state.view, &mut renderer, diff)?;
                    continue;
                }
                "/doctor" => {
                    // P2：TUI 内环境检查（与 `tpi doctor` 同一份报告）。
                    push_system_line(
                        &mut ui_state.view,
                        &mut renderer,
                        crate::doctor::render_report(&config.workspace_root),
                    )?;
                    continue;
                }
                "/compact" => {
                    // P1-10：手动压缩——在下一次 run 开始时的完整边界执行。
                    ui_state.force_compaction = true;
                    push_system_line(
                        &mut ui_state.view,
                        &mut renderer,
                        "将在下一次 run 开始时执行手动压缩（压缩旧历史为摘要）".to_string(),
                    )?;
                    continue;
                }
                _ => {}
            }
            ui_state.view.push_line(LineKind::User, message.clone());
            renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;

            if session.is_none() {
                *session = Some(create_session(
                    &config.sessions_root,
                    &config.workspace_root,
                )?);
            }
            let mut session_log = session.take().unwrap();
            let outcome = match run_interactive(
                provider,
                &mut session_log,
                config,
                history,
                message,
                &mut ui_state,
                &mut renderer,
                &mut key_rx,
                current_cancel.clone(),
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    // 交互模式：run 失败（provider/工具基础设施）不得杀死整个 TUI。
                    // 显示实际错误并保留 session，用户可以继续对话。
                    ui_state.view.status = crate::tui::model::StatusLine::Idle;
                    ui_state.view.turn = 0;
                    ui_state.view.push_line(
                        LineKind::System,
                        format!("run 失败，session 已保留：{error}"),
                    );
                    renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
                    *session = Some(session_log);
                    continue;
                }
            };
            ui_state.view.add_usage(&outcome.usage);
            // P0-1：outcome.messages 是完整 context（含旧历史），必须 replace。
            merge_outcome_history(history, &outcome);
            *session = Some(session_log);
            ui_state.view.push_line(LineKind::System, "─".repeat(40));
        }
    }

    renderer.restore().map_err(|e| e.to_string())?;
    Ok(())
}

/// 执行 reducer 返回的跨边界效果（§27：app 层执行 effect）。
fn execute_ui_effect(
    effect: UiEffect,
    ui_state: &mut UiState,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) {
    match effect {
        UiEffect::CancelRun => {
            if let Some(cancel) = current_cancel.lock().unwrap().clone() {
                cancel.cancel();
            }
            ui_state
                .view
                .push_line(LineKind::System, "已发送取消（Esc）；保留 session");
        }
        // Quit / ResumeSession 由交互主循环处理（/quit 与 /sessions 菜单）。
        UiEffect::Quit | UiEffect::ResumeSession(_) => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    history: &[ChatMessage],
    message: String,
    ui_state: &mut UiState,
    renderer: &mut Renderer,
    key_rx: &mut mpsc::Receiver<Event>,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<agent::AgentOutcome, String> {
    use crate::tui::effect::UiEffect;
    use crate::tui::event::UiEvent;
    use crate::tui::model::LineKind;
    use ratatui::crossterm::event::KeyEventKind;
    let cancel = CancellationToken::new();
    *current_cancel.lock().unwrap() = Some(cancel.clone());
    ui_state.view.status = StatusLine::Running {
        turn: 0,
        tool: "正在连接模型".into(),
    };
    ui_state.running = true;
    let (ui_tx, mut ui_rx) = mpsc::channel(128);

    let force = ui_state.force_compaction;
    ui_state.force_compaction = false;
    let run_future = agent::run(
        provider,
        session,
        config,
        history,
        message.clone(),
        ui_tx,
        cancel.clone(),
        true,
        force,
    );
    tokio::pin!(run_future);

    // 动画时钟（§16.1）：独立推进，活动时目标 60 FPS。
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome = loop {
        tokio::select! {
            event = ui_rx.recv() => {
                if let Some(event) = event {
                    // Agent 事件 → reducer（纯状态转换；T3）。
                    crate::tui::reducer::update(ui_state, UiEvent::Agent(event));
                }
                if renderer.should_draw() {
                    renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
                }
            }
            _ = ticker.tick() => {
                crate::tui::reducer::update(ui_state, UiEvent::Tick);
                if renderer.should_draw() {
                    renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
                }
            }
            key = key_rx.recv() => {
                match key {
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        // 键盘事件 → reducer；Esc 取消语义在 reducer 内
                        // 依据 running 状态决策（§6.2 保留 session）。
                        let effects = crate::tui::reducer::update(ui_state, UiEvent::Key(key));
                        for effect in effects {
                            match effect {
                                UiEffect::CancelRun => {
                                    cancel.cancel();
                                    ui_state.view.push_line(
                                        LineKind::System,
                                        "已发送取消（Esc）；保留 session",
                                    );
                                }
                                UiEffect::Quit | UiEffect::ResumeSession(_) => {
                                    // run 中不会产生（reducer 仅空闲时产生）。
                                }
                            }
                        }
                        renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
                    }
                    Some(Event::Paste(text)) => {
                        crate::tui::reducer::update(ui_state, UiEvent::Paste(text));
                        renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
                    }
                    // 鼠标：滚轮翻页；点击工具卡片展开/折叠（hit-test 在 app 层）。
                    Some(Event::Mouse(mouse)) => {
                        use ratatui::crossterm::event::MouseEventKind;
                        let event = match mouse.kind {
                            MouseEventKind::ScrollUp => Some(UiEvent::MouseScrollUp),
                            MouseEventKind::ScrollDown => Some(UiEvent::MouseScrollDown),
                            MouseEventKind::Down(_) => {
                                if ui_state.view.overlay.is_some() {
                                    // Overlay 打开时点击外部不动作。
                                    None
                                } else if let Some(target) =
                                    renderer.hit_target(mouse.column, mouse.row)
                                {
                                    match target {
                                        crate::tui::HitTarget::Tool(id) => {
                                            Some(UiEvent::ClickTool(id))
                                        }
                                        crate::tui::HitTarget::Reasoning(index) => {
                                            Some(UiEvent::ClickReasoning(index))
                                        }
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some(event) = event {
                            crate::tui::reducer::update(ui_state, event);
                            renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
                        }
                    }
                    Some(Event::Resize(_, _)) => {
                        renderer.autoresize().map_err(|e| e.to_string())?;
                        renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
                    }
                    _ => {}
                }
            }
            result = &mut run_future => break result,
        }
    };
    *current_cancel.lock().unwrap() = None;
    ui_state.running = false;
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
        _ => {}
    }
    ui_state.view.status = StatusLine::Idle;
    ui_state.view.turn = 0;
    renderer.draw(&ui_state.view).map_err(|e| e.to_string())?;
    Ok(outcome)
}

/// 从 session 读取最近一次 edit 的结果（含 unified diff，§18.3 /diff）。
fn last_edit_diff(log: &SessionLog) -> String {
    use crate::session::read_events;
    let events = match read_events(log.path()) {
        Ok(events) => events,
        Err(_) => return "读取 session 失败".to_string(),
    };
    // P2：/diff 聚合本 run 内所有成功的 edit（此前只返回最近一次）。
    // 输出每个文件的 model output（含 unified diff 与 revision 信息）。
    let diffs: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolCompleted { outcome, .. }
                if outcome.session_metadata.tool == "edit"
                    && outcome.status == crate::tool::outcome::ToolStatus::Succeeded =>
            {
                Some(outcome.model_payload.output.clone())
            }
            _ => None,
        })
        .collect();
    if diffs.is_empty() {
        return "本轮还没有成功的 edit（写文件请用 edit 工具）".to_string();
    }
    let mut out = String::new();
    for (i, diff) in diffs.iter().enumerate() {
        if i > 0 {
            out.push_str("\n---\n\n");
        }
        out.push_str(diff);
    }
    out
}

fn spawn_ctrl_c_handler(current_cancel: Arc<Mutex<Option<CancellationToken>>>) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            let has_run = current_cancel.lock().unwrap().is_some();
            if has_run {
                if let Some(cancel) = current_cancel.lock().unwrap().clone() {
                    cancel.cancel();
                }
            } else {
                // 空闲时 Ctrl-C：退出（§11.5 第二次快速 Ctrl-C 的简化）。
                // process::exit 不执行析构，必须显式恢复终端，否则残留 raw mode（无回显）。
                restore_terminal_on_exit();
                std::process::exit(130);
            }
        }
    });
}

/// 退出前恢复终端（inline TUI 打开过 raw mode；process::exit 不触发 Drop）。
fn restore_terminal_on_exit() {
    use std::io::Write;
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::Show,
        ratatui::crossterm::style::ResetColor,
    );
    let _ = std::io::stdout().flush();
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

fn create_session(
    sessions_root: &std::path::Path,
    workspace_root: &Utf8PathBuf,
) -> Result<SessionLog, String> {
    SessionLog::create(sessions_root, workspace_root.as_std_path(), RunId::new_v7())
        .map_err(|e| format!("创建 session 失败: {e}"))
}

/// 恢复既有 session：读取事件 + 注入 Interrupted 结果（§4.3）。
fn resume_session(
    sessions_root: &std::path::Path,
    workspace_root: &Utf8PathBuf,
    session_id: SessionId,
) -> Result<(Option<SessionLog>, Vec<ChatMessage>), String> {
    let path = agent::session_path(sessions_root, workspace_root, session_id);
    if !path.exists() {
        return Err(format!("session 不存在: {session_id}"));
    }
    let recovery =
        session::recovery::recover(&path).map_err(|e| format!("恢复 session 失败: {e}"))?;
    // P0-3：从文件重建（带真实 seq 的 replay 投影，与 runtime 语义对齐）；
    // recovery.events 用于中断工具的恢复信息。
    let mut history = session::replay_messages(&path).map_err(|e| format!("重建历史失败: {e}"))?;
    history.extend(agent::interrupted_as_messages(&recovery.interrupted));
    let log = SessionLog::open(sessions_root, workspace_root.as_std_path(), session_id)
        .map_err(|e| format!("打开 session 失败: {e}"))?;
    Ok((Some(log), history))
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
fn list_sessions(
    sessions_root: &std::path::Path,
    workspace_root: &Utf8PathBuf,
) -> Result<Vec<(SessionId, std::time::SystemTime, usize, String)>, String> {
    let workspace_id = session::workspace_id_for(workspace_root.as_std_path());
    let dir = sessions_root.join(&workspace_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(std::time::SystemTime, SessionId, usize, String)> = Vec::new();
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
            let count = count_jsonl_lines(&entry.path());
            // P2：首条用户消息预览（只解析事件头；列表辨识用）。
            let preview = first_user_preview(&entry.path());
            entries.push((modified, id, count, preview));
        }
    }
    entries.sort_by_key(|(time, _, _, _)| std::cmp::Reverse(*time));
    Ok(entries
        .into_iter()
        .map(|(t, id, n, p)| (id, t, n, p))
        .collect())
}

/// P2：从 session 文件提取首条用户消息摘要（≤40 字符，单行）。
fn first_user_preview(path: &std::path::Path) -> String {
    use crate::session::read_events;
    let Ok(events) = read_events(path) else {
        return String::new();
    };
    for event in events {
        if let SessionEvent::UserSubmitted { content } = event {
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

/// P1-13：事件数 = JSONL 行数（每行一个事件，§14.2）；只数行不 serde 解析，
/// session 增多时 `/sessions` 列表仍保持轻量（此前对每个文件解析全部事件）。
fn count_jsonl_lines(path: &std::path::Path) -> usize {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    std::io::BufReader::new(file)
        .lines()
        .filter(|line| line.as_ref().is_ok_and(|l| !l.trim().is_empty()))
        .count()
}

/// 会话列表展示用的时间格式（HH:MM:SS 或 MM-DD HH:MM）。
fn fmt_time(t: std::time::SystemTime) -> String {
    let Ok(duration) = t.duration_since(std::time::UNIX_EPOCH) else {
        return "-".into();
    };
    let secs = duration.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours:02}:{minutes:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::ViewModel;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
                ..Default::default()
            },
        };
        // 成功 1（带 diff）、失败（不应出现）、成功 2。
        log.append_event(&SessionEvent::ToolCompleted {
            call_id: crate::ids::ToolCallId::new_v7(),
            outcome: edit_outcome("diff-one", ToolStatus::Succeeded),
        })
        .unwrap();
        log.append_event(&SessionEvent::ToolCompleted {
            call_id: crate::ids::ToolCallId::new_v7(),
            outcome: edit_outcome("diff-failed", ToolStatus::Failed),
        })
        .unwrap();
        log.append_event(&SessionEvent::ToolCompleted {
            call_id: crate::ids::ToolCallId::new_v7(),
            outcome: edit_outcome("diff-two", ToolStatus::Succeeded),
        })
        .unwrap();
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
            state.pending_message.as_deref(),
            Some("/settings"),
            "Enter 提交的必须是补全后的命令"
        );
    }
}
