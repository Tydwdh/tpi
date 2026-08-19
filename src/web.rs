//! `tpi serve`：局域网网页接口（粗糙版，供手机在局域网内发送/接收消息）。
//!
//! 设计约束（用户诉求：可以稍微粗糙一点）：
//! - **零新依赖**：手写 HTTP/1.1 最小实现（tokio `TcpListener` + 逐行解析），
//!   不引入 axum/hyper（保持 P0-06 依赖卫生）；每次连接 `Connection: close`。
//! - 无 TLS：只监听局域网（0.0.0.0），可选 `--token` 做简单访问控制。
//! - 串行执行：同一时刻只跑一个 agent run（busy 时新消息返回 409）；
//!   手机端轮询 `/api/status` 拿结果（无 SSE/WebSocket）。
//! - 会话延续：启动时恢复当前 workspace 最近的 session（与 `--continue`
//!   同一事实源），手机端对话与电脑端 TUI 可互见。
//!
//! 路由：
//! - `GET  /`                → 单文件 HTML 页面（中文，fetch 轮询）
//! - `GET  /api/history`     → 会话历史 JSON `[{role, text}]`
//! - `POST /api/send`        → `{"content":"..."}`；busy 时 409
//! - `GET  /api/status`      → `{"busy":bool,"result":{text,error}|null}`
//!
//! 访问控制：设置了 `--token T` 时，`/api/*` 需要 `X-TPI-Token: T` 头或
//! `?token=T` 查询参数；`GET /` 页面本身无敏感信息不校验。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use camino::Utf8PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::provider::ChatMessage;
use crate::provider::openai_compat::OpenAiCompatClient;
use crate::session::conversation::Conversation;
use crate::session::replay_messages;

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// 读请求的整体超时（防慢客户端/半开连接挂住连接与 task）。
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// 请求行 / 单个 header 行上限。
const MAX_LINE_BYTES: usize = 8 * 1024;
/// 全部 header 总大小上限。
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// 并发连接上限（防局域网恶意/失控客户端用大量连接耗尽内存；每个连接
/// 最多持有一个解析缓冲 + 15s `READ_TIMEOUT` 窗口）。
const MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// 一次 run 的结果（供 /api/status 轮询）。
#[derive(Debug, Clone)]
pub struct RunResult {
    pub text: String,
    pub error: Option<String>,
    pub finished_ms: u64,
}

struct ServerState {
    config: Arc<Config>,
    conversation: Mutex<Conversation>,
    provider: Mutex<OpenAiCompatClient>,
    busy: AtomicBool,
    /// 最近一次完成的结果，关联其 `run_id（供轮询方区分“是不是我的消息`”）。
    last: std::sync::Mutex<Option<(u64, RunResult)>>,
    token: Option<String>,
    sessions_root: std::path::PathBuf,
    workspace_root: Utf8PathBuf,
    /// 当前会话的 durable log 文件路径；run 期间由 `history_json` 直接
    /// replay 文件，避免被 run 持有的 conversation 锁阻塞。
    log_path: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// 单调递增的 run 序号（/api/send 响应带出，轮询按它认领结果）。
    next_run: AtomicU64,
    /// P4 gate：composition root 注入的工具注册表。
    registry: std::sync::Arc<std::sync::Mutex<crate::tool::registry::ToolRegistry>>,
    /// Session 级共享托管进程 / PTY 注册表（跨 run 存活）。
    processes: std::sync::Arc<std::sync::Mutex<crate::process::managed::ProcessRegistry>>,
    terminals: std::sync::Arc<std::sync::Mutex<crate::terminal::TerminalRegistry>>,
    agents: std::sync::Arc<std::sync::Mutex<tpi_agent::agent::manager::AgentManager>>,
}

/// 启动局域网网页服务（阻塞直到监听失败或 Ctrl-C）。
pub async fn serve(config: Arc<Config>, port: u16, token: Option<String>) -> Result<(), String> {
    let workspace_root = config.workspace_root.clone();
    let sessions_root = config.sessions_root.clone();

    // 恢复最近会话（与 `--continue` 同一事实源）；无历史或锁被占用（另一
    // TPI 实例在用）时降级为新会话——serve 是独立前台，不与已有实例抢锁。
    let conversation = match crate::app::latest_session_id(&sessions_root, &workspace_root) {
        Ok(session_id) => match Conversation::resume(&sessions_root, &workspace_root, session_id) {
            Ok(conversation) => conversation,
            Err(e) => {
                eprintln!("警告: 恢复最近 session 失败（{e}），改用新会话");
                Conversation::new()
            }
        },
        Err(_) => Conversation::new(),
    };

    let api_key = crate::config::read_api_key(&config)?;
    let provider = OpenAiCompatClient::new(
        config.model.base_url.clone(),
        config.model.name.clone(),
        api_key,
        config.model.reasoning.clone(),
        config.model.max_output_tokens,
        config.model.context_window,
    );

    // 恢复的会话已有 log 时，history 可直接 replay 该文件（无需等锁）。
    let log_path = conversation.log().map(|log| log.path().to_path_buf());

    let state = Arc::new(ServerState {
        config,
        conversation: Mutex::new(conversation),
        provider: Mutex::new(provider),
        busy: AtomicBool::new(false),
        last: std::sync::Mutex::new(None),
        token,
        sessions_root,
        workspace_root,
        log_path: std::sync::Mutex::new(log_path),
        next_run: AtomicU64::new(1),
        registry: std::sync::Arc::new(std::sync::Mutex::new(
            crate::tool::registry::builtin_registry(),
        )),
        processes: std::sync::Arc::new(std::sync::Mutex::new(
            crate::process::managed::ProcessRegistry::new(),
        )),
        terminals: std::sync::Arc::new(std::sync::Mutex::new(
            crate::terminal::TerminalRegistry::default(),
        )),
        agents: std::sync::Arc::new(std::sync::Mutex::new(
            tpi_agent::agent::manager::AgentManager::new(),
        )),
    });

    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| format!("监听 0.0.0.0:{port} 失败: {e}"))?;
    println!("TPI 网页接口已启动：http://0.0.0.0:{port}");
    println!("手机与本机在同一局域网时访问 http://<本机局域网IP>:{port}");
    println!("（ipconfig 查看本机局域网 IP；Ctrl-C 停止）");

    // K-1：并发连接上限——accept 无限 spawn + 每连接最大 15s 解析窗口，
    // 局域网内大量连接可各自吃数百 MiB（read_line 先整行读入再检查）。
    let connection_permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    loop {
        let (mut stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // K-5：瞬时 accept 错误（如 EMFILE 句柄耗尽）不应终止整个服务。
            Err(e) => {
                tracing::warn!(error = %e, "web accept 失败（继续监听）");
                continue;
            }
        };
        // 超过并发上限：立即拒绝（不排队），保持服务可用。
        let Ok(permit) = connection_permits.clone().try_acquire_owned() else {
            tracing::warn!("web 并发连接超限（{MAX_CONCURRENT_CONNECTIONS}），拒绝连接");
            let _ = write_response(
                &mut stream,
                "503 Service Unavailable",
                "text/plain; charset=utf-8",
                "server busy".into(),
            )
            .await;
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit; // 连接处理完（含 15s 超时）后自动释放
            if let Err(e) = handle_connection(stream, state).await {
                tracing::warn!(error = %e, "web connection error");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 请求解析与路由
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    query: std::collections::HashMap<String, String>,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    state: Arc<ServerState>,
) -> Result<(), String> {
    let request = match tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(e)) => {
            // 解析失败也要给客户端一个 HTTP 响应，而不是直接断连
            // （否则 fetch 只能报“网络错误”，无法区分原因）。
            write_response(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                format!("bad request: {e}"),
            )
            .await?;
            return Ok(());
        }
        Err(_) => {
            write_response(
                &mut stream,
                "408 Request Timeout",
                "text/plain; charset=utf-8",
                "request timeout".to_string(),
            )
            .await?;
            return Ok(());
        }
    };
    let (status, content_type, body) = route(request, &state).await;
    write_response(&mut stream, &status, &content_type, body).await
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: String,
) -> Result<(), String> {
    let body_bytes = body.into_bytes();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body_bytes.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&body_bytes)
        .await
        .map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// 有界逐行读取（K-1）：先整行读入再检查长度会让内存无上限——
/// `read_line` 会先把任意长的行读进 String。这里用 `fill_buf` 增量检查：
/// 累积超过 `max` 字节立即拒绝，绝不为超长行分配无限内存。
/// 返回 `Ok(None)` = EOF（无任何内容）。行内容含换行符（与 `read_line` 一致）。
async fn read_line_bounded(
    reader: &mut BufReader<&mut tokio::net::TcpStream>,
    max: usize,
) -> Result<Option<String>, String> {
    let mut line = String::new();
    loop {
        let buf = reader
            .fill_buf()
            .await
            .map_err(|e| format!("读行失败: {e}"))?;
        if buf.is_empty() {
            // EOF：无内容 → None；有部分内容 → 返回（未换行的最后一段）。
            return Ok(if line.is_empty() { None } else { Some(line) });
        }
        if let Some(index) = buf.iter().position(|b| *b == b'\n') {
            let take = index + 1;
            if line.len() + take > max {
                return Err("行过长".into());
            }
            line.push_str(&String::from_utf8_lossy(&buf[..take]));
            let consumed = take;
            reader.consume(consumed);
            return Ok(Some(line));
        } else {
            if line.len() + buf.len() > max {
                return Err("行过长".into());
            }
            line.push_str(&String::from_utf8_lossy(buf));
            let consumed = buf.len();
            reader.consume(consumed);
        }
    }
}

/// 逐行读取请求：请求行 + headers + Content-Length body（上限 `MAX_BODY_BYTES`）。
async fn read_request(stream: &mut tokio::net::TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(stream);
    let line = read_line_bounded(&mut reader, MAX_LINE_BYTES)
        .await
        .map_err(|_| "请求行过长".to_string())?
        .ok_or_else(|| "空请求".to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err("空请求行".into());
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, std::collections::HashMap::new()),
    };

    let mut headers = std::collections::HashMap::new();
    let mut content_length: usize = 0;
    let mut header_bytes = 0usize;
    loop {
        let Some(line) = read_line_bounded(&mut reader, MAX_LINE_BYTES)
            .await
            .map_err(|_| "header 行过长".to_string())?
        else {
            break;
        };
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err("header 总大小超限".into());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // 空行 = headers 结束
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if key == "content-length" {
                content_length = value.parse::<usize>().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }

    let mut body = Vec::new();
    if content_length > 0 {
        if content_length > MAX_BODY_BYTES {
            return Err("请求体过大".into());
        }
        // 精确读 Content-Length 字节即返回——不能用 take(n).read_to_end()：
        // read_to_end 要读到 EOF 或 limit 耗尽才结束，而 TCP 连接无 EOF，
        // 读满 body 后 limit 未到 0 会永久阻塞。
        body.resize(content_length, 0);
        reader
            .read_exact(&mut body)
            .await
            .map_err(|e| format!("读 body 失败: {e}"))?;
    }

    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            // 粗糙解码：仅处理 %20 空格与 +。
            let decode = |s: &str| s.replace('+', " ").replace("%20", " ");
            out.insert(decode(k).clone(), decode(v).clone());
        }
    }
    out
}

fn token_ok(state: &ServerState, req: &Request) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    let from_query = req.query.get("token").map(String::as_str);
    let from_header = req
        .headers
        .get("x-tpi-token")
        .map(std::string::String::as_str);
    from_query == Some(expected.as_str()) || from_header == Some(expected.as_str())
}

async fn route(request: Request, state: &Arc<ServerState>) -> (String, String, String) {
    // 静态页面不校验 token（无敏感信息）。
    if request.method == "GET" && request.path == "/" {
        return (
            "200 OK".to_string(),
            "text/html; charset=utf-8".to_string(),
            INDEX_HTML.to_string(),
        );
    }
    // /api/* 一律校验 token。
    if !token_ok(state, &request) {
        return (
            "401 Unauthorized".to_string(),
            "application/json; charset=utf-8".to_string(),
            r#"{"error":"token 无效"}"#.to_string(),
        );
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/history") => match history_json(state).await {
            Ok(json) => (
                "200 OK".to_string(),
                "application/json; charset=utf-8".to_string(),
                json,
            ),
            Err(e) => (
                "500 Internal Server Error".to_string(),
                "application/json; charset=utf-8".to_string(),
                json_error(&e),
            ),
        },
        ("GET", "/api/status") => status_json(state),
        ("POST", "/api/send") => match send_message(state, &request).await {
            Ok((status, json)) => (status, "application/json; charset=utf-8".to_string(), json),
            Err((code, json)) => (code, "application/json; charset=utf-8".to_string(), json),
        },
        _ => (
            "404 Not Found".to_string(),
            "text/plain; charset=utf-8".to_string(),
            "not found".to_string(),
        ),
    }
}

fn json_error(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

async fn history_json(state: &Arc<ServerState>) -> Result<String, String> {
    // 直接从 durable log 文件 replay，不经过 conversation 锁：run 期间
    // conversation 锁被 agent::run 持有（&mut SessionLog 借用），若这里等锁
    // 手机端在 agent 处理中刷新页面会挂起数分钟。replay 与 run 结束后的
    // refresh_from_log 同一语义；并发写入的未完成尾行会被 read_envelopes 丢弃。
    let path = state
        .log_path
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let messages: Vec<ChatMessage> = match &path {
        Some(path) => replay_messages(path).map_err(|e| format!("重建历史失败: {e}"))?,
        None => Vec::new(),
    };
    let messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|message| {
            let (role, text) = match message {
                ChatMessage::System(text) => ("system", text.clone()),
                ChatMessage::User(text) => ("user", text.clone()),
                ChatMessage::Assistant { content, .. } => ("assistant", content.clone()),
                ChatMessage::Tool { name, content, .. } => ("tool", format!("[{name}] {content}")),
            };
            serde_json::json!({ "role": role, "text": text })
        })
        .collect();
    Ok(serde_json::json!({ "messages": messages }).to_string())
}

fn status_json(state: &Arc<ServerState>) -> (String, String, String) {
    let busy = state.busy.load(Ordering::SeqCst);
    let last = state
        .last
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let (run_id, result) = match last {
        Some((run_id, result)) => (
            Some(run_id),
            Some(serde_json::json!({
                "text": result.text,
                "error": result.error,
                "finished_ms": result.finished_ms,
            })),
        ),
        None => (None, None),
    };
    (
        "200 OK".to_string(),
        "application/json; charset=utf-8".to_string(),
        serde_json::json!({ "busy": busy, "run_id": run_id, "result": result }).to_string(),
    )
}

async fn send_message(
    state: &Arc<ServerState>,
    request: &Request,
) -> Result<(String, String), (String, String)> {
    let content: serde_json::Value = serde_json::from_slice(&request.body)
        .map_err(|_| ("400 Bad Request".into(), json_error("body 不是合法 JSON")))?;
    let content = content
        .get("content")
        .and_then(|v| v.as_str())
        .map_or("", str::trim)
        .to_string();
    if content.is_empty() {
        return Err((
            "400 Bad Request".to_string(),
            json_error("content 不能为空"),
        ));
    }
    if content.len() > MAX_MESSAGE_BYTES {
        return Err((
            "400 Bad Request".to_string(),
            json_error("消息超过 16 KiB 上限"),
        ));
    }

    // 串行执行：已有 run 在跑则拒绝（手机端轮询等待）。
    if state
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err((
            "409 Conflict".to_string(),
            json_error("已有消息在处理中，请稍候"),
        ));
    }

    let run_id = state.next_run.fetch_add(1, Ordering::SeqCst);
    let state = state.clone();
    tokio::spawn(async move {
        // RunGuard 保证任何路径（含 panic/取消）都复位 busy 并写入结果，
        // 否则一次异常会让服务永久 409、所有后续轮询看到旧结果。
        let mut guard = RunGuard::new(&state.busy, &state.last);
        let result = run_agent(&state, content).await;
        *state
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((run_id, result));
        guard.finish();
    });

    Ok((
        "200 OK".to_string(),
        serde_json::json!({ "ok": true, "run_id": run_id }).to_string(),
    ))
}

/// busy 生命周期守卫：drop（含 unwind）时必复位 busy；若 run 意外中断
/// （panic）则写入错误结果，避免 status 把上一次成功结果当成这次完成。
struct RunGuard<'a> {
    busy: &'a AtomicBool,
    last: &'a std::sync::Mutex<Option<(u64, RunResult)>>,
    finished: bool,
}

impl<'a> RunGuard<'a> {
    fn new(busy: &'a AtomicBool, last: &'a std::sync::Mutex<Option<(u64, RunResult)>>) -> Self {
        Self {
            busy,
            last,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let mut last = self
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // ISSUE-035：run_id 从 1 开始（next_run 初值 1），错误占位用 0——
            // 两者不再冲突，轮询端不会把 panic 错误误认成自己那条消息的结果。
            *last = Some((
                0,
                RunResult {
                    text: String::new(),
                    error: Some("内部错误：处理意外中断（服务端）".to_string()),
                    finished_ms: 0,
                },
            ));
        }
        self.busy.store(false, Ordering::SeqCst);
    }
}

/// 执行一次完整 agent run（串行；borrow 全部在函数内释放后更新状态）。
async fn run_agent(state: &Arc<ServerState>, content: String) -> RunResult {
    let started = std::time::Instant::now();
    let mut conversation = state.conversation.lock().await;
    let ensure = conversation.ensure_started(&state.sessions_root, &state.workspace_root);
    // 记录 durable log 路径：run 期间 history 从文件 replay，不依赖这把锁。
    if let Some(log) = conversation.log() {
        *state
            .log_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(log.path().to_path_buf());
    }
    let run = async {
        let (session, history) = conversation
            .parts_for_run()
            .map_err(crate::agent::RunFailure::Session)?;
        let mut provider = state.provider.lock().await;
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel(64);
        // headless：消费 LiveEvent 防 channel 满挂死（P1-03 语义事件，不投影 TUI）。
        let drain = tokio::spawn(async move { while ui_rx.recv().await.is_some() {} });
        let outcome = crate::agent::run(
            &mut *provider,
            session,
            &state.config,
            crate::agent::RunInput {
                history,
                user_message: content,
                ui: ui_tx,
                cancel: CancellationToken::new(),
                interactive: false,
                force_compaction: false,
                workspace: None,
                registry: state.registry.clone(),
                processes: state.processes.clone(),
                terminals: state.terminals.clone(),
                agents: state.agents.clone(),
            },
        )
        .await;
        drain.abort();
        outcome
    }
    .await;

    // 任何路径都从 durable log 重建 history（与 -p 模式一致）。
    let refresh = conversation.refresh_from_log();
    drop(conversation);

    if let Err(e) = ensure {
        return RunResult {
            text: String::new(),
            error: Some(format!("创建 session 失败: {e}")),
            finished_ms: started.elapsed().as_millis() as u64,
        };
    }
    if let Err(e) = refresh {
        return RunResult {
            text: String::new(),
            error: Some(format!("重建历史失败: {e}")),
            finished_ms: started.elapsed().as_millis() as u64,
        };
    }
    match run {
        Ok(outcome) => RunResult {
            text: outcome.assistant_text,
            error: None,
            finished_ms: started.elapsed().as_millis() as u64,
        },
        Err(failure) => RunResult {
            text: String::new(),
            error: Some(failure.to_string()),
            finished_ms: started.elapsed().as_millis() as u64,
        },
    }
}

// ---------------------------------------------------------------------------
// 单文件页面（内嵌，粗糙但可用：历史 + 输入 + 轮询）
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>TPI · 手机端</title>
<style>
  body { font-family: system-ui, sans-serif; margin: 0; background: #1e1e2e; color: #cdd6f4; }
  header { padding: 12px 16px; background: #11111b; display: flex; justify-content: space-between; align-items: center; }
  h1 { font-size: 18px; margin: 0; }
  #status { font-size: 13px; color: #a6adc8; }
  #history { padding: 12px 16px; }
  .msg { margin: 8px 0; padding: 10px 12px; border-radius: 10px; white-space: pre-wrap; word-break: break-word; font-size: 15px; }
  .user { background: #313244; align-self: flex-end; }
  .assistant { background: #45475a; }
  .tool { background: #1e1e2e; color: #a6adc8; font-size: 12px; border-left: 3px solid #f38ba8; }
  .system { background: #11111b; color: #a6adc8; font-size: 12px; font-style: italic; }
  .label { font-size: 11px; color: #89b4fa; margin-bottom: 2px; }
  #inputbar { position: fixed; bottom: 0; left: 0; right: 0; display: flex; gap: 8px; padding: 10px 16px; background: #11111b; }
  #input { flex: 1; padding: 10px; border-radius: 8px; border: 1px solid #45475a; background: #313244; color: #cdd6f4; font-size: 15px; }
  #send { padding: 10px 18px; border-radius: 8px; border: none; background: #89b4fa; color: #11111b; font-size: 15px; font-weight: bold; }
  body { padding-bottom: 70px; }
  .spin { display: inline-block; animation: spin 1s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
</style>
</head>
<body>
<header><h1>TPI</h1><div id="status"></div></header>
<div id="history"></div>
<div id="inputbar">
  <input id="input" placeholder="给 Agent 发送消息…" autocomplete="off">
  <button id="send">发送</button>
</div>
<script>
const token = new URLSearchParams(location.search).get('token')
  || localStorage.getItem('tpi_token') || '';
if (token) localStorage.setItem('tpi_token', token);
function api(path, options) {
  options = options || {};
  options.headers = Object.assign({}, options.headers);
  if (token) options.headers['X-TPI-Token'] = token;
  return fetch(path, options).then(async r => {
    if (!r.ok) {
      const body = await r.json().catch(() => ({}));
      throw new Error(body.error || ('HTTP ' + r.status));
    }
    return r.json();
  });
}
function esc(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }
function render(msgs) {
  const h = document.getElementById('history');
  h.innerHTML = '';
  for (const m of msgs) {
    const div = document.createElement('div');
    div.className = 'msg ' + m.role;
    if (m.role !== 'tool') {
      const label = document.createElement('div');
      label.className = 'label';
      label.textContent = ({user:'你', assistant:'Agent', system:'系统', tool:'工具'}[m.role] || m.role);
      div.appendChild(label);
    }
    const body = document.createElement('div');
    body.textContent = m.text;
    div.appendChild(body);
    h.appendChild(div);
  }
  h.scrollTop = h.scrollHeight;
}
function setStatus(text) {
  document.getElementById('status').textContent = text;
}
async function loadHistory() {
  try { const d = await api('/api/history'); render(d.messages); }
  catch (e) { setStatus('历史加载失败: ' + e.message); }
}
async function send() {
  const input = document.getElementById('input');
  const text = input.value.trim();
  if (!text) return;
  input.value = '';
  setStatus('已发送，处理中… ⏳');
  try {
    const d = await api('/api/send', { method: 'POST', body: JSON.stringify({ content: text }) });
    poll(d.run_id);
  } catch (e) {
    input.value = text; // 失败（409 忙/网络错误）时恢复输入，避免用户丢字重打
    setStatus('发送失败: ' + e.message);
    loadHistory();
  }
}
let polling = false;
async function poll(myRunId) {
  if (polling) return;
  polling = true;
  try {
    while (true) {
      const d = await api('/api/status');
      if (!d.busy) {
        if (d.run_id === myRunId) {
          setStatus(d.result && d.result.error ? ('失败: ' + d.result.error) : '完成 ✓');
        } else {
          // 串行执行下多手机并发：自己的结果可能已被后续消息覆盖，
          // 不把别人的结果当成自己的，直接看对话确认。
          setStatus('该消息已完成（结果已被后续消息覆盖），见下方对话');
        }
        await loadHistory();
        return;
      }
      setStatus('处理中… ⏳');
      await new Promise(r => setTimeout(r, 1500));
    }
  } catch (e) {
    setStatus('状态查询失败: ' + e.message);
  } finally {
    polling = false;
  }
}
document.getElementById('send').addEventListener('click', send);
document.getElementById('input').addEventListener('keydown', e => { if (e.key === 'Enter') send(); });
loadHistory();
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn parse_query_decodes_plus_and_space() {
        let q = parse_query("token=a+b%20c&x=1");
        assert_eq!(q.get("token").map(String::as_str), Some("a b c"));
        assert_eq!(q.get("x").map(String::as_str), Some("1"));
    }

    #[test]
    fn run_guard_unwound_path_resets_busy_and_writes_error() {
        let busy = AtomicBool::new(true);
        let last = std::sync::Mutex::new(None);
        {
            // 模拟 panic：不调用 finish() 直接 drop（unwind 时 guard drop 等价路径）。
            let _guard = RunGuard::new(&busy, &last);
        }
        assert!(!busy.load(Ordering::SeqCst), "busy 必须被复位");
        let last = last.lock().unwrap();
        let (run_id, result) = last.as_ref().expect("unwind 路径应写入错误结果");
        assert_eq!(*run_id, 0);
        assert!(result.error.is_some());
    }

    #[test]
    fn run_guard_finished_path_keeps_last() {
        let busy = AtomicBool::new(true);
        let last = std::sync::Mutex::new(None);
        {
            let mut guard = RunGuard::new(&busy, &last);
            *last.lock().unwrap() = Some((
                7,
                RunResult {
                    text: "ok".into(),
                    error: None,
                    finished_ms: 1,
                },
            ));
            guard.finish();
        }
        assert!(!busy.load(Ordering::SeqCst));
        let last = last.lock().unwrap();
        let (run_id, result) = last.as_ref().unwrap();
        assert_eq!(*run_id, 7, "正常完成的结果不应被 guard 覆盖");
        assert_eq!(result.text, "ok");
    }

    #[tokio::test]
    async fn read_request_parses_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(
                b"POST /api/send HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\n\r\n{\"a\":1}",
            )
            .await
            .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let req = read_request(&mut stream).await.unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/send");
        assert_eq!(req.headers.get("host").map(String::as_str), Some("x"));
        assert_eq!(req.body, b"{\"a\":1}");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn read_request_rejects_oversized_header_line() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            let req = format!(
                "POST / HTTP/1.1\r\nX-Pad: {}\r\n\r\n",
                "a".repeat(MAX_LINE_BYTES)
            );
            s.write_all(req.as_bytes()).await.unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let err = read_request(&mut stream).await.unwrap_err();
        assert!(err.contains("header 行过长"), "实际错误: {err}");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn read_request_rejects_oversized_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"POST /api/send HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n")
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let err = read_request(&mut stream).await.unwrap_err();
        assert!(err.contains("请求体过大"), "实际错误: {err}");
        writer.await.unwrap();
    }
}
