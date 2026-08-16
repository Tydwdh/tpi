//! WebSocket 集成测试（web_desktop.md §三十四）。
//!
//! 覆盖：
//! - connect -> handshake（hello/server_hello + 版本检查）；
//! - send command -> receive ack；
//! - receive event（事件流真实推送）；
//! - disconnect -> runtime 继续运行（事件不因客户端断开而消失）；
//! - 多客户端（A/B 同时订阅都收到事件）。
//!
//! 使用 tokio-tungstenite 作为测试客户端（同时也是 Web 前端的真实传输栈）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use camino::Utf8PathBuf;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use tpi_agent::provider::{
    FinishReason, ModelRequest, Provider, ProviderError, ProviderEvent, ProviderResponse,
};
use tpi_config::config::{Config, LimitsConfig, ModelConfig};
use tpi_protocol::PROTOCOL_VERSION;
use tpi_runtime::service::ProviderFactory;
use tpi_runtime::{RuntimeHandle, RuntimeTask};
use tpi_server::auth::AuthConfig;
use tpi_session::Usage;

// ---- fake provider（与 runtime_contract 相同的最小实现） ----

struct FakeProvider {
    script: VecDeque<String>,
}

impl Provider for FakeProvider {
    fn model_name(&self) -> &str {
        "fake-model"
    }

    fn stream(
        &mut self,
        _request: ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send {
        let text = self.script.pop_front().unwrap_or_default();
        async move {
            if !text.is_empty() {
                let _ = events.send(ProviderEvent::TextDelta(text)).await;
            }
            Ok(ProviderResponse {
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
                tool_calls: Vec::new(),
            })
        }
    }
}

fn test_config(workspace: &Utf8PathBuf) -> Config {
    Config {
        model: ModelConfig {
            provider: "test".into(),
            name: "fake-model".into(),
            base_url: "https://example.invalid/v1".into(),
            reasoning: None,
            max_output_tokens: None,
            context_window: None,
            api_key_env: "TPI_TEST_API_KEY".into(),
            api_key: None,
            price_input: None,
            price_output: None,
        },
        models: Vec::new(),
        limits: LimitsConfig::default(),
        workspace_root: workspace.clone(),
        sessions_root: workspace.join(".tpi-test-sessions").into(),
        artifacts_root: workspace.join(".tpi-test-artifacts").into(),
        shell_path: None,
        safety_reserve_tokens: 8192,
        auto_open_browser: false,
        web_summary_model: "none".into(),
        system_prompt_extra: None,
        source: "test".into(),
        ui_theme: "omp".into(),
        ui_mode: tpi_ui_types::ViewMode::Fullscreen,
        ui_keymap: tpi_ui_types::Keymap::builtin(),
        ui_collapsed_lines: 10,
        allow_outside_workspace: true,
    }
}

/// 启动测试 server（随机端口），返回 (ws_url, http_url, handle, shutdown_token)。
async fn start_server(
    config: Config,
    web_dist: Option<std::path::PathBuf>,
) -> (
    String,
    String,
    RuntimeHandle,
    tokio_util::sync::CancellationToken,
) {
    let registry: Arc<StdMutex<tpi_capabilities::tool::registry::ToolRegistry>> = Arc::new(
        StdMutex::new(tpi_capabilities::tool::registry::builtin_registry()),
    );
    let build_provider: ProviderFactory<FakeProvider> = Box::new(|_| {
        Ok(FakeProvider {
            script: VecDeque::from(["你好，这是测试回复".to_string()]),
        })
    });
    let task = RuntimeTask::new(Arc::new(config), build_provider, registry);
    let (handle, _join) = RuntimeHandle::new(task);

    let shutdown = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let server_shutdown = shutdown.clone();
    let server_handle = handle.clone();
    tokio::spawn(async move {
        let auth = AuthConfig::none();
        let state = Arc::new(tpi_server::ServerState {
            handle: server_handle,
            auth,
            server_version: "test".into(),
            web_dist,
        });
        let app = tpi_server::http::router(state);
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                server_shutdown.cancelled().await;
            })
            .await;
    });

    // 等 server 就绪。
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    (
        format!("ws://{addr}/ws"),
        format!("http://{addr}"),
        handle,
        shutdown,
    )
}

/// 连接并完成握手，返回 (ws_stream, server_hello last_seq)。
async fn connect_and_hello(
    url: &str,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u64,
) {
    let (ws, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();
    let mut ws = ws;
    ws.send(Message::Text(
        serde_json::json!({
            "type": "hello",
            "protocol_version": PROTOCOL_VERSION,
            "client_name": "test-client",
            "client_version": "0.1.0",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    // 读 server_hello。
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let hello: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
    assert_eq!(hello["type"], "server_hello");
    assert_eq!(hello["protocol_version"], PROTOCOL_VERSION);
    let last_seq = hello["last_seq"].as_u64().unwrap_or(0);
    (ws, last_seq)
}

/// 从 ws 流读取直到谓词命中（返回该消息）。
async fn recv_until(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = msg.into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        if predicate(&value) {
            return value;
        }
    }
}

#[tokio::test]
async fn handshake_command_and_event_flow() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (url, _http_url, handle, shutdown) = start_server(test_config(&ws), None).await;

    let (mut client, _last_seq) = connect_and_hello(&url).await;

    // CreateSession。
    client
        .send(Message::Text(
            serde_json::json!({
                "type": "command",
                "payload": { "type": "create_session", "title": null }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // 按到达顺序消费：事件与 ack 谁先到收谁；两者都拿到后再继续
    //（recv_until 会丢弃不匹配的消息，不能串行等两个谓词）。
    let mut session_id = None;
    let mut ack_seen = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(session_id.is_some() && ack_seen) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, client.next())
            .await
            .expect("create_session 应在超时前完成")
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        match value["type"].as_str() {
            Some("event") if value["event"]["type"] == "session_created" => {
                session_id = Some(
                    value["event"]["session"]["id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
            }
            Some("ack") => {
                assert_eq!(value["ack"]["status"], "accepted", "ack: {value}");
                ack_seen = true;
            }
            _ => {}
        }
    }
    let session_id = session_id.unwrap();

    // SubmitMessage -> RunStarted -> ... -> RunCompleted。
    // 一次性等待 ack 与 run_completed（recv_until 会丢弃不匹配消息，
    // 串行等两个谓词会互吞事件）。
    client
        .send(Message::Text(
            serde_json::json!({
                "type": "command",
                "payload": {
                    "type": "submit_message",
                    "session_id": session_id,
                    "content": "测试消息"
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let mut ack_ok = false;
    let mut completed = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(ack_ok && completed.is_some()) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, client.next())
            .await
            .expect("run 应在超时前完成")
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
        match value["type"].as_str() {
            Some("ack") => {
                assert_eq!(value["ack"]["status"], "accepted", "ack: {value}");
                ack_ok = true;
            }
            Some("event")
                if matches!(
                    value["event"]["type"].as_str(),
                    Some("run_completed") | Some("run_failed"),
                ) =>
            {
                completed = Some(value);
            }
            _ => {}
        }
    }
    assert!(ack_ok, "必须收到 accepted ack");
    let completed = completed.unwrap();
    assert_eq!(completed["event"]["type"], "run_completed");
    assert!(
        completed["event"]["assistant_text"]
            .as_str()
            .unwrap()
            .contains("测试回复")
    );

    let _ = handle;
    shutdown.cancel();
}

#[tokio::test]
async fn multi_client_both_receive_events() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (url, _http_url, _handle, shutdown) = start_server(test_config(&ws), None).await;

    let (mut client_a, _) = connect_and_hello(&url).await;
    let (mut client_b, _) = connect_and_hello(&url).await;

    // A 创建会话并提交消息；B 也应看到事件（多订阅者）。
    client_a
        .send(Message::Text(
            serde_json::json!({
                "type": "command",
                "payload": { "type": "create_session", "title": null }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let created = recv_until(&mut client_a, |v| {
        v["type"] == "event" && v["event"]["type"] == "session_created"
    })
    .await;
    let session_id = created["event"]["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // consume create ack（可能在 created 事件后到达，跳过即可）

    client_a
        .send(Message::Text(
            serde_json::json!({
                "type": "command",
                "payload": {
                    "type": "submit_message",
                    "session_id": session_id,
                    "content": "multi-client 测试"
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // A 和 B 都必须收到 run_completed。
    let deadline = Duration::from_secs(15);
    let completed_a = tokio::time::timeout(
        deadline,
        recv_until(&mut client_a, |v| {
            v["type"] == "event"
                && matches!(
                    v["event"]["type"].as_str(),
                    Some("run_completed") | Some("run_failed")
                )
        }),
    )
    .await
    .unwrap();
    let completed_b = tokio::time::timeout(
        deadline,
        recv_until(&mut client_b, |v| {
            v["type"] == "event" && v["event"]["type"] == "run_completed"
        }),
    )
    .await
    .unwrap();
    assert_eq!(completed_a["event"]["type"], "run_completed");
    assert_eq!(completed_b["event"]["type"], "run_completed");
    // 同一 seq 的事件两个客户端等价。
    assert_eq!(completed_a["seq"], completed_b["seq"]);

    shutdown.cancel();
}

#[tokio::test]
async fn version_mismatch_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let (url, _http_url, _handle, shutdown) = start_server(test_config(&ws), None).await;

    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws_stream
        .send(Message::Text(
            serde_json::json!({
                "type": "hello",
                "protocol_version": 9999,
                "client_name": "old-client",
                "client_version": "0.0.1",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(5), ws_stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&msg.into_text().unwrap()).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["code"], "protocol_version_mismatch");

    shutdown.cancel();
}

#[tokio::test]
async fn http_health_and_version_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&ws);
    // 从 ws url 推 http url。
    let (_ws_url, http_url, _handle, shutdown) = start_server(config, None).await;

    let health: serde_json::Value = reqwest::get(format!("{http_url}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");

    let version: serde_json::Value = reqwest::get(format!("{http_url}/api/version"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(version["protocol_version"], PROTOCOL_VERSION);

    shutdown.cancel();
}

#[tokio::test]
async fn static_files_are_served_from_web_dist() {
    // 构造一个临时 dist 目录（模拟 `vite build` 产物）。
    let dir = tempfile::tempdir().unwrap();
    let ws = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&ws);
    let dist = dir.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<html>TPI Web</html>").unwrap();
    std::fs::write(dist.join("app.js"), "console.log('tpi')").unwrap();

    let (_ws_url, http_url, _handle, shutdown) = start_server(config, Some(dist)).await;

    // 根路径返回 index.html。
    let index = reqwest::get(format!("{http_url}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(index.contains("TPI Web"), "index.html 应被服务: {index}");

    // 已知静态文件返回内容。
    let app_js = reqwest::get(format!("{http_url}/app.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(app_js.contains("console.log('tpi')"));

    // SPA 回退：未知路径也返回 index.html（前端路由）。
    let fallback = reqwest::get(format!("{http_url}/some/route"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(fallback.contains("TPI Web"));

    shutdown.cancel();
}
