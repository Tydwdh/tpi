//! 应用层（文档 §4.1）：输入路由、生命周期。
//!
//! 活动 run 中的输入路由（§6.2）：
//! - 普通 Enter：排队到下一个安全 model boundary（steering，M4 起）；
//! - Ctrl-C：取消当前 run，保留 session；空闲时退出。
//!
//! M5：Ratatui inline renderer——只有 renderer 写 stdout（§3.2 不变量 11、
//! §16.1），16 ms 帧合并，synchronized update。

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, DeltaKind, RuntimeEvent};
use crate::config::Config;
use crate::ids::{RunId, SessionId};
use crate::provider::openai_compat::OpenAiCompatClient;
use crate::provider::{ChatMessage, Provider};
use crate::session::{self, SessionEvent, SessionLog};
use crate::tui::{Renderer, model::ViewModel};

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
) -> Result<(), String> {
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

/// 非交互单次 run（输出经 stdout，仅最终答案）。
async fn run_prompt_once<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    history: &[ChatMessage],
    message: String,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<agent::AgentOutcome, String> {
    let cancel = CancellationToken::new();
    *current_cancel.lock().unwrap() = Some(cancel.clone());
    let (ui_tx, _ui_rx) = mpsc::channel(128);
    let outcome = agent::run(
        provider,
        session,
        config,
        history,
        message,
        ui_tx,
        cancel.clone(),
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
async fn interactive_loop<P: Provider>(
    provider: &mut P,
    session: &mut Option<SessionLog>,
    config: &Config,
    history: &mut Vec<ChatMessage>,
    initial_prompt: &str,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<(), String> {
    use crate::tui::editor::Editor;
    use crate::tui::model::LineKind;
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

    let mut renderer = Renderer::new().map_err(|e| format!("初始化终端失败: {e}"))?;
    let mut view = ViewModel {
        model_name: config.model.name.clone(),
        ..Default::default()
    };
    view.push_line(
        LineKind::System,
        "TPI（文档：TPI_DESIGN.md；/quit 退出，Ctrl-C 取消当前 run）",
    );
    let mut editor = Editor::new();
    let mut pending_message: Option<String> = if initial_prompt.is_empty() {
        None
    } else {
        Some(initial_prompt.to_string())
    };
    renderer.draw(&view).map_err(|e| e.to_string())?;

    loop {
        // 处理键盘事件（非阻塞收集到帧间隔）。
        let mut need_draw = false;
        while event::poll(std::time::Duration::from_millis(1)).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    need_draw = true;
                    match key.code {
                        KeyCode::Enter => {
                            let text = editor.text().trim().to_string();
                            if !text.is_empty() {
                                pending_message = Some(text);
                                editor = Editor::new();
                            }
                        }
                        KeyCode::Backspace => editor.backspace(),
                        KeyCode::Delete => editor.delete(),
                        KeyCode::Left => editor.move_left(),
                        KeyCode::Right => editor.move_right(),
                        KeyCode::Home => editor.home(),
                        KeyCode::End => editor.end(),
                        KeyCode::Char(c) => {
                            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                                continue; // Ctrl-C 由 ctrl_c handler 处理。
                            }
                            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'u' {
                                editor = Editor::new();
                                continue;
                            }
                            editor.insert_char(c);
                        }
                        _ => {}
                    }
                }
                Event::Paste(text) => {
                    need_draw = true;
                    editor.insert_str(&text);
                }
                _ => {}
            }
        }

        // 有提交的消息：运行。
        if let Some(message) = pending_message.take() {
            match message.as_str() {
                "/quit" | "/exit" => break,
                "/settings" => {
                    view.push_line(
                        LineKind::System,
                        format!(
                            "配置来源: {}\n模型: {} ({})",
                            config.source, config.model.name, config.model.provider
                        ),
                    );
                    continue;
                }
                "/model" => {
                    view.push_line(
                        LineKind::System,
                        format!("primary: {} ({})", config.model.name, config.model.provider),
                    );
                    continue;
                }
                "/help" => {
                    view.push_line(
                        LineKind::System,
                        "/quit 退出；/settings 查看生效配置及来源；/model 查看模型；/session 查看会话与成本；/new 新会话；/cancel 取消当前 run；/diff 查看本轮文件 diff；/thinking 查看推理设置；/compact 手动压缩（在安全边界）；Ctrl-C 取消当前 run",
                    );
                    continue;
                }
                "/session" => {
                    let info = match session.as_ref() {
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
                    view.push_line(LineKind::System, info);
                    continue;
                }
                "/new" => {
                    *session = None;
                    history.clear();
                    view.push_line(LineKind::System, "已开始新会话");
                    continue;
                }
                "/cancel" => {
                    if let Some(cancel) = current_cancel.lock().unwrap().clone() {
                        cancel.cancel();
                        view.push_line(LineKind::System, "已发送取消（§11.5：保留 session）");
                    } else {
                        view.push_line(LineKind::System, "当前没有正在运行的 run");
                    }
                    continue;
                }
                "/thinking" => {
                    let value = config
                        .model
                        .reasoning
                        .clone()
                        .unwrap_or_else(|| "未配置（默认）".to_string());
                    view.push_line(LineKind::System, format!("reasoning: {value}"));
                    continue;
                }
                "/diff" => {
                    let diff = match session.as_ref() {
                        Some(log) => last_edit_diff(log),
                        None => "尚无 session".to_string(),
                    };
                    view.push_line(LineKind::System, diff);
                    continue;
                }
                "/compact" => {
                    view.push_line(
                        LineKind::System,
                        "compaction 会在上下文超过预算时自动在完整边界执行（§15.4）；运行中的自动压缩状态见状态栏。",
                    );
                    continue;
                }
                _ => {}
            }
            view.push_line(LineKind::User, message.clone());
            renderer.draw(&view).map_err(|e| e.to_string())?;

            if session.is_none() {
                *session = Some(create_session(
                    &config.sessions_root,
                    &config.workspace_root,
                )?);
            }
            let mut session_log = session.take().unwrap();
            let outcome = run_interactive(
                provider,
                &mut session_log,
                config,
                history,
                message,
                &mut view,
                &mut renderer,
                current_cancel.clone(),
            )
            .await?;
            history.extend(outcome.messages);
            *session = Some(session_log);
            view.push_line(LineKind::System, "─".repeat(40));
        }

        if need_draw || renderer.should_draw() {
            view.input = editor.text().to_string();
            view.input_cursor = editor.cursor;
            renderer.draw(&view).map_err(|e| e.to_string())?;
        }
    }

    renderer.restore().map_err(|e| e.to_string())?;
    Ok(())
}

/// 执行一次 run 并驱动 renderer（§16.1：事件 → ViewModel → 16 ms 帧合并 → draw）。
#[allow(clippy::too_many_arguments)]
async fn run_interactive<P: Provider>(
    provider: &mut P,
    session: &mut SessionLog,
    config: &Config,
    history: &[ChatMessage],
    message: String,
    view: &mut ViewModel,
    renderer: &mut Renderer,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
) -> Result<agent::AgentOutcome, String> {
    use crate::tui::model::{LineKind, StatusLine};
    let cancel = CancellationToken::new();
    *current_cancel.lock().unwrap() = Some(cancel.clone());
    let (ui_tx, mut ui_rx) = mpsc::channel(128);

    let run_future = agent::run(
        provider,
        session,
        config,
        history,
        message.clone(),
        ui_tx,
        cancel.clone(),
    );
    tokio::pin!(run_future);

    let outcome = loop {
        tokio::select! {
            event = ui_rx.recv() => {
                match event {
                    Some(RuntimeEvent::AssistantDelta { kind, text, .. }) => {
                        match kind {
                            DeltaKind::Text => {
                                for line in text.split('\n') {
                                    view.push_line(LineKind::Assistant, line.to_string());
                                }
                            }
                            DeltaKind::Reasoning => {
                                for line in text.split('\n') {
                                    view.push_line(LineKind::Reasoning, line.to_string());
                                }
                            }
                        }
                    }
                    Some(RuntimeEvent::ToolProgress { chunk, .. }) => {
                        view.push_line(LineKind::Tool, chunk);
                    }
                    None => {}
                }
                if renderer.should_draw() {
                    renderer.draw(view).map_err(|e| e.to_string())?;
                }
            }
            result = &mut run_future => break result,
        }
    };
    *current_cancel.lock().unwrap() = None;
    let outcome = outcome.map_err(|failure| failure.to_string())?;
    if outcome.reason == crate::session::CompletionReason::Error {
        view.push_line(
            LineKind::System,
            "run 以 Error 结束（长度限制/内容过滤/协议错误，见 session 记录）",
        );
    }
    view.status = StatusLine::Idle;
    renderer.draw(view).map_err(|e| e.to_string())?;
    Ok(outcome)
}

/// 从 session 读取最近一次 edit 的结果（含 unified diff，§18.3 /diff）。
fn last_edit_diff(log: &SessionLog) -> String {
    use crate::session::read_events;
    let events = match read_events(log.path()) {
        Ok(events) => events,
        Err(_) => return "读取 session 失败".to_string(),
    };
    for event in events.iter().rev() {
        if let SessionEvent::ToolCompleted { outcome, .. } = event
            && outcome.session_metadata.tool == "edit"
        {
            return outcome.model_payload.output.clone();
        }
    }
    "本轮还没有 edit".to_string()
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
                std::process::exit(130);
            }
        }
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
    let mut history = session_to_messages(&recovery.events);
    history.extend(agent::interrupted_as_messages(&recovery.interrupted));
    let log = SessionLog::open(sessions_root, workspace_root.as_std_path(), session_id)
        .map_err(|e| format!("打开 session 失败: {e}"))?;
    Ok((Some(log), history))
}

/// 把 session 事件重建为对话消息（§15.1：session log 是事实源，上下文是投影）。
///
/// §15.4：采用最新 active compaction summary 作为前缀，排除其覆盖的 raw events；
/// 旧 summary 不重复注入。
pub fn session_to_messages(events: &[SessionEvent]) -> Vec<ChatMessage> {
    // 最新 compaction 覆盖的结束 seq（事件顺序 == seq 顺序，append-only）。
    let mut compacted_up_to: Option<u64> = None;
    let mut summary_text: Option<String> = None;
    for event in events {
        if let SessionEvent::CompactionCommitted { covered, summary } = event {
            let end_seq = covered.end.0.as_u128() as u64;
            if compacted_up_to.is_none_or(|prev| end_seq > prev) {
                compacted_up_to = Some(end_seq);
                summary_text = Some(summary.text.clone());
            }
        }
    }

    let mut messages = Vec::new();
    let mut last_assistant_idx: Option<usize> = None;
    let mut pending_calls: Vec<crate::provider::ToolCall> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        // 跳过被最新 compaction 覆盖的 raw events（§15.4：旧 raw 不重复注入）。
        if let Some(up_to) = compacted_up_to
            && (index as u64) < up_to.saturating_sub(1)
        {
            continue;
        }
        match event {
            SessionEvent::UserSubmitted { content } | SessionEvent::UserSteered { content } => {
                messages.push(ChatMessage::User(content.clone()));
                last_assistant_idx = None;
            }
            SessionEvent::AssistantMessageCommitted { message } => {
                messages.push(ChatMessage::Assistant {
                    content: message.content.clone(),
                    tool_calls: Vec::new(),
                });
                last_assistant_idx = Some(messages.len() - 1);
            }
            SessionEvent::ToolRequested { call } => {
                if let Some(idx) = last_assistant_idx
                    && let ChatMessage::Assistant { tool_calls, .. } = &mut messages[idx]
                {
                    tool_calls.push(call.clone());
                }
                pending_calls.push(call.clone());
            }
            SessionEvent::ToolCompleted { call_id, outcome } => {
                if let Some(call) = pending_calls.iter().find(|call| call.call_id == *call_id) {
                    messages.push(ChatMessage::Tool {
                        tool_call_id: call.provider_id.clone(),
                        name: call.name.clone(),
                        content: outcome.model_payload.output.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    if let Some(summary) = summary_text {
        let mut with_summary = Vec::with_capacity(messages.len() + 1);
        with_summary.push(ChatMessage::User(format!(
            "（此前会话的压缩摘要，见 CompactionCommitted）\n{summary}"
        )));
        with_summary.extend(messages);
        messages = with_summary;
    }
    messages
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
    let workspace_id = session::workspace_id_for(workspace_root.as_std_path());
    let dir = sessions_root.join(&workspace_id);
    if !dir.exists() {
        return Err("当前 workspace 没有历史 session".into());
    }
    let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|e| e == "jsonl")
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, entry.path()))
        })
        .collect();
    entries.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
    let (_, path) = entries.first().ok_or("当前 workspace 没有历史 session")?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("无效 session 文件名")?;
    parse_session_id(name)
}
