//! Phase C：Provider Contract（TPI_STABILIZATION_TASK §6/§7/§8/§30）。
//!
//! - P0-4：ModelRequest 是请求配置的单一事实源——`max_output_tokens`/
//!   `reasoning`/`model` 必须来自 request（compaction 的 1024 output budget
//!   必须真正进入 HTTP body），client 只保留连接级状态。
//! - Level 3（§29）：recorded SSE fixtures（tests/fixtures/provider/*.sse）
//!   → 本地 mock server → parser → ProviderEvent → ProviderResponse 确定性测试。
//! - §30：finish=tool_calls 但 calls 为空 → 明确 protocol error；
//!   malformed streamed JSON args → 明确 protocol error。
//! - invalid tool args：agent 调度前 schema 校验产生 observation，不破坏 session。

use std::time::Duration;

mod fixtures;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::provider::openai_compat::OpenAiCompatClient;
use tpi::provider::{
    ChatMessage, FinishReason, ModelRequest, Provider, ProviderError, ProviderEvent,
};

/// 本地 mock SSE server：捕获请求 body，返回固定 SSE 响应。
async fn mock_sse_server(
    sse_body: &'static str,
) -> (String, tokio::task::JoinHandle<serde_json::Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // 读请求头直到 \r\n\r\n。
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                panic!("connection closed before headers");
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]);
        let content_length: usize = head
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        // 读完整 body。
        while buf.len() < header_end + 4 + content_length {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let body_start = header_end + 4;
        let request_body = buf[body_start..body_start + content_length].to_vec();
        // 返回 SSE 响应。
        let response =
            format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{sse_body}");
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        serde_json::from_slice(&request_body).expect("request body must be JSON")
    });
    (format!("http://{addr}/v1"), handle)
}

/// 第一个连接接受后立即断开（模拟传输层中断/请求发送失败），
/// 第二个连接正常返回 SSE——验证请求发送阶段重试。
async fn mock_sse_server_fail_once(sse_body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // 第一个请求：接受后立即断开（不写任何响应）。
        let (first, _) = listener.accept().await.unwrap();
        drop(first);
        // 第二个请求：正常返回 SSE。
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                panic!("connection closed before headers");
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]);
        let content_length: usize = head
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        while buf.len() < header_end + 4 + content_length {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let response =
            format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{sse_body}");
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });
    format!("http://{addr}/v1")
}

/// 通过 mock server 跑一次完整 stream，返回 (response, 捕获的 request body)。
async fn run_via_mock(
    sse_body: &'static str,
    client_max_tokens: Option<u32>,
    request_max_tokens: Option<u32>,
) -> (
    tpi::provider::ProviderResponse,
    serde_json::Value,
    Vec<ProviderEvent>,
) {
    let (base_url, body_handle) = mock_sse_server(sse_body).await;
    let mut client = OpenAiCompatClient::new(
        base_url,
        "client-model".into(),
        "test-key".into(),
        Some("client-reasoning".into()),
        client_max_tokens,
        None,
    );
    let request = ModelRequest {
        model: "request-model".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: request_max_tokens,
        reasoning: Some("request-reasoning".into()),
        context_window: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let response = client
        .stream(request, tx.clone(), CancellationToken::new())
        .await
        .expect("stream succeeds");
    drop(tx);
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let body = body_handle.await.unwrap();
    (response, body, events)
}

/// P0-4：request 级配置是单一事实源——client 构造时的
/// max_output_tokens/reasoning/model 不得覆盖 request 的值
/// （compaction 的 1024 output budget 必须真正生效）。
#[tokio::test]
async fn p0_4_request_config_is_authoritative_over_client() {
    let fixture = include_str!("fixtures/provider/text_only.sse");
    let (response, body, _events) = run_via_mock(fixture, Some(999), Some(1024)).await;
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        body["max_tokens"], 1024,
        "request.max_output_tokens 必须进入 HTTP body（compaction 预算）: {body}"
    );
    assert_eq!(
        body["model"], "request-model",
        "model 必须以 request 为准: {body}"
    );
    assert_eq!(
        body["reasoning"], "request-reasoning",
        "reasoning 必须以 request 为准: {body}"
    );
}

/// Level 3：text_only.sse → 文本 delta 流式到达，finish=stop。
#[tokio::test]
async fn recorded_text_only_streams_text() {
    let fixture = include_str!("fixtures/provider/text_only.sse");
    let (response, body, events) = run_via_mock(fixture, None, None).await;
    assert_eq!(body["stream"], true);
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert!(response.tool_calls.is_empty());
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello world", "recorded SSE 文本必须完整: {events:?}");
}

/// 传输层中断重试：第一个连接未写任何响应即断开（≈ error decoding
/// response body），consume_stream 阶段未收到事件时必须重发请求并成功。
#[tokio::test]
async fn stream_transport_failure_retries_before_any_event() {
    let fixture = include_str!("fixtures/provider/text_only.sse");
    let base_url = mock_sse_server_fail_once(fixture).await;
    let mut client = OpenAiCompatClient::new(
        base_url,
        "client-model".into(),
        "test-key".into(),
        None,
        None,
        None,
    );
    let request = ModelRequest {
        model: "request-model".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: None,
        context_window: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let response = client
        .stream(request, tx.clone(), CancellationToken::new())
        .await
        .expect("传输中断后必须重试成功");
    drop(tx);
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(response.finish_reason, FinishReason::Stop);
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "hello world",
        "重试后的流必须完整且事件不重复: {events:?}"
    );
}

/// 第一个连接返回 200 + SSE 响应头 + 部分 body 后中断（≈ "error decoding
/// response body"：响应体在流读取阶段被截断），第二个连接正常返回——
/// 验证 consume_stream 阶段未收到事件时重试。
#[tokio::test]
async fn stream_body_truncation_retries_before_any_event() {
    let fixture = include_str!("fixtures/provider/text_only.sse");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // 第一个请求：写响应头 + 半个 SSE 事件后立即断开。
        let (mut first, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = first.read(&mut buf).await;
        let partial = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"hel";
        let _ = first.write_all(partial.as_bytes()).await;
        let _ = first.flush().await;
        drop(first); // 截断：不写完整个 SSE body。
        // 第二个请求：正常返回 SSE。
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf2: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                panic!("connection closed before headers");
            }
            buf2.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf2.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
        };
        let head = String::from_utf8_lossy(&buf2[..header_end]);
        let content_length: usize = head
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        while buf2.len() < header_end + 4 + content_length {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf2.extend_from_slice(&tmp[..n]);
        }
        let response =
            format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{fixture}");
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });
    let base_url = format!("http://{addr}/v1");
    let mut client = OpenAiCompatClient::new(
        base_url,
        "client-model".into(),
        "test-key".into(),
        None,
        None,
        None,
    );
    let request = ModelRequest {
        model: "request-model".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: None,
        context_window: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let response = client
        .stream(request, tx.clone(), CancellationToken::new())
        .await
        .expect("响应体截断后必须重试成功");
    drop(tx);
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(response.finish_reason, FinishReason::Stop);
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "hello world",
        "重试后的流必须完整且事件不重复: {events:?}"
    );
}

/// 已收到事件后流中断：不可重试（避免事件重复/乱序），直接返回协议错误；
/// 已到达的部分文本必须仍经事件通道可见。
#[tokio::test]
async fn stream_failure_after_events_does_not_retry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // 唯一连接：写完整响应头 + 一个完整 SSE 事件 + 截断（不再写更多）。
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let partial = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n\
data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"wor";
        let _ = socket.write_all(partial.as_bytes()).await;
        let _ = socket.flush().await;
        drop(socket); // 截断。
    });
    let base_url = format!("http://{addr}/v1");
    let mut client = OpenAiCompatClient::new(
        base_url,
        "client-model".into(),
        "test-key".into(),
        None,
        None,
        None,
    );
    let request = ModelRequest {
        model: "request-model".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: None,
        context_window: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let result = client
        .stream(request, tx.clone(), CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(result.is_err(), "已收到事件后中断必须报错（不可重试）");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello ", "已到达的部分文本必须保留: {events:?}");
}

/// Level 3：one_tool_call.sse → tool call 增量拼接完整、
/// provider id 原样保留（§30 第 7 条）。
#[tokio::test]
async fn recorded_one_tool_call_assembles_arguments() {
    let fixture = include_str!("fixtures/provider/one_tool_call.sse");
    let (response, _body, events) = run_via_mock(fixture, None, None).await;
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 1);
    let call = &response.tool_calls[0];
    assert_eq!(
        call.provider_id, "call_abc123",
        "provider id 原样 round-trip"
    );
    assert_eq!(call.name, "read");
    assert_eq!(call.arguments, r#"{"path":"probe.txt"}"#);
    // 事件序列：ToolCallStarted → ToolArgumentsDelta。
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ProviderEvent::ToolCallStarted { name, .. } if name == "read")),
        "ToolCallStarted 必须发出: {events:?}"
    );
    let arg_chunks: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolArgumentsDelta { chunk, .. } => Some(chunk.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(arg_chunks.join(""), r#"{"path":"probe.txt"}"#);
}

/// Level 3：parallel_tool_calls.sse → 同轮两个 tool call，
/// 顺序按 provider 原始 index 保留。
#[tokio::test]
async fn recorded_parallel_tool_calls_preserve_index_order() {
    let fixture = include_str!("fixtures/provider/parallel_tool_calls.sse");
    let (response, _body, _events) = run_via_mock(fixture, None, None).await;
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 2);
    assert_eq!(response.tool_calls[0].provider_id, "call_a");
    assert_eq!(response.tool_calls[0].name, "read");
    assert_eq!(response.tool_calls[1].provider_id, "call_b");
    assert_eq!(response.tool_calls[1].name, "list");
    assert_eq!(response.tool_calls[1].arguments, r#"{"path":"."}"#);
}

/// Level 3：reasoning_and_tool.sse → reasoning 走 ephemeral 事件，
/// 不进入 durable response（§28）。
#[tokio::test]
async fn recorded_reasoning_stays_ephemeral() {
    let fixture = include_str!("fixtures/provider/reasoning_and_tool.sse");
    let (response, _body, events) = run_via_mock(fixture, None, None).await;
    let reasoning: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ReasoningDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning.join(""), "先看一下文件结构");
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].provider_id, "call_xyz");
}

/// §30 第 11 条：finish=tool_calls 但 calls 为空 → 明确协议错误（不能静默 Ok）。
#[tokio::test]
async fn finish_tool_calls_with_empty_calls_is_protocol_error() {
    let fixture = include_str!("fixtures/provider/empty_tool_calls.sse");
    let (base_url, _body_handle) = mock_sse_server(fixture).await;
    let mut client = OpenAiCompatClient::new(base_url, "m".into(), "k".into(), None, None, None);
    let request = ModelRequest {
        model: "m".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: None,
        context_window: None,
    };
    let (tx, _rx) = mpsc::channel(16);
    let error = client
        .stream(request, tx, CancellationToken::new())
        .await
        .expect_err("finish=tool_calls 但无 calls 必须是协议错误");
    assert!(
        matches!(error, ProviderError::Protocol(_)),
        "必须是 Protocol 错误: {error:?}"
    );
}

/// §30 第 12 条：malformed streamed JSON args → 明确 protocol error（不猜补 JSON）。
#[tokio::test]
async fn malformed_streamed_arguments_is_protocol_error() {
    let fixture = include_str!("fixtures/provider/malformed_arguments.sse");
    let (base_url, _body_handle) = mock_sse_server(fixture).await;
    let mut client = OpenAiCompatClient::new(base_url, "m".into(), "k".into(), None, None, None);
    let request = ModelRequest {
        model: "m".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: None,
        context_window: None,
    };
    let (tx, _rx) = mpsc::channel(16);
    let error = client
        .stream(request, tx, CancellationToken::new())
        .await
        .expect_err("未闭合 JSON arguments 必须是协议错误");
    assert!(
        matches!(error, ProviderError::Protocol(_)),
        "必须是 Protocol 错误: {error:?}"
    );
}

/// 测试 client 不带超时重试（本地 mock 失败时快速失败）。
#[tokio::test]
async fn retry_respects_cancel_during_backoff() {
    // 500 响应 → Retryable → 等待期间取消 → Cancelled。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            // 读到完整请求头后返回 500。
            let received = String::from_utf8_lossy(&buf[..n]);
            if received.contains("\r\n\r\n") {
                let response = "HTTP/1.1 500 Internal Server Error\r\nretry-after: 30\r\ncontent-length: 0\r\n\r\n";
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
                break;
            }
        }
    });
    let mut client = OpenAiCompatClient::new(
        format!("http://{addr}/v1"),
        "m".into(),
        "k".into(),
        None,
        None,
        None,
    );
    let request = ModelRequest {
        model: "m".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: None,
        context_window: None,
    };
    let cancel = CancellationToken::new();
    let cancel_in_task = cancel.clone();
    let (tx, _rx) = mpsc::channel(16);
    let client_task = tokio::spawn(async move { client.stream(request, tx, cancel_in_task).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), client_task)
        .await
        .expect("cancel 必须在 backoff 中生效");
    assert!(matches!(result, Ok(Err(ProviderError::Cancelled))));
}

/// §30 第 9 条：invalid tool args（schema 校验失败）产生 observation，
/// 模型可见失败原因，session 不被破坏、run 正常继续。
#[tokio::test]
async fn invalid_tool_args_produce_observation_without_breaking_session() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = fixtures::test_config(&workspace);
    // 第 1 次请求返回非法 args 的 read（缺必填 path）；
    // 第 2 次请求模型根据 observation 修正（这次给合法 args）。
    let mut provider = fixtures::fake_provider::FakeProvider::scripted(vec![
        Box::new(move |_request| {
            fixtures::fake_provider::FakeResponse::with_tool_calls(vec![
                fixtures::fake_provider::tool_call("read", serde_json::json!({})),
            ])
        }),
        Box::new(move |_request| {
            fixtures::fake_provider::FakeResponse::with_tool_calls(vec![
                fixtures::fake_provider::tool_call(
                    "read",
                    serde_json::json!({"path": "probe.txt"}),
                ),
            ])
        }),
        Box::new(move |_request| fixtures::fake_provider::FakeResponse::text("done")),
    ]);
    std::fs::write(workspace.join("probe.txt"), "probe").unwrap();
    let mut session = tpi::session::SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        tpi::ids::RunId::new_v7(),
    )
    .unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = tpi::agent::run(
        &mut provider,
        &mut session,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "读 probe.txt".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
        },
    )
    .await
    .expect("run 必须继续（invalid args 是 expected failure，不是 run failure）");
    drain.abort();
    assert_eq!(outcome.reason, tpi::session::CompletionReason::Stop);
    // 3 次请求：invalid → observation → valid → observation → done。
    assert_eq!(provider.request_count, 3);
    // session 完整：两条 Tool 消息（一条 rejected/校验失败、一条成功）。
    let events = tpi::session::read_events(session.path()).unwrap();
    let completed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            tpi::session::SessionEvent::ToolCompleted { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 2, "两次工具执行都必须持久化");
    assert!(
        completed[0].model_payload.output.contains("invalid"),
        "第一次的 observation 必须说明校验失败: {}",
        completed[0].model_payload.output
    );
    assert_eq!(
        completed[1].status,
        tpi::tool::outcome::ToolStatus::Succeeded
    );
    // P0-3：rejected observation 持久化后，restart 重建必须与 runtime 一致。
    let resumed = tpi::session::replay_messages(session.path()).unwrap();
    assert_eq!(
        outcome.messages, resumed,
        "rejected observation 也必须能 replay（P0-3）"
    );
}

/// §8：TPI_TRACE_PROVIDER=1 时写入 ~/.tpi/logs/provider-*.jsonl，
/// 记录 request_start/finish 等元数据；不记录 Authorization。
#[tokio::test]
async fn provider_trace_writes_metadata_without_auth() {
    // 先设环境变量（trace 模式是进程级 OnceLock，首次检查前必须设置）。
    let dir = tempfile::tempdir().unwrap();
    let tpi_home = dir.path().to_str().unwrap().to_string();
    unsafe {
        std::env::set_var("TPI_TRACE_PROVIDER", "1");
        std::env::set_var("TPI_HOME", &tpi_home);
    }

    let fixture = include_str!("fixtures/provider/text_only.sse");
    let (response, _body, _events) = run_via_mock(fixture, None, None).await;
    assert_eq!(response.finish_reason, FinishReason::Stop);

    // 找到 trace 文件并断言内容。
    let logs = std::path::Path::new(&tpi_home).join("logs");
    let files: Vec<_> = std::fs::read_dir(&logs)
        .expect("trace 目录必须创建")
        .filter_map(|e| e.ok())
        .collect();
    assert!(!files.is_empty(), "trace 文件必须生成");
    let content = std::fs::read_to_string(files[0].path()).unwrap();
    assert!(content.contains("\"kind\":\"request_start\""), "{content}");
    assert!(
        content.contains("\"kind\":\"response_status\""),
        "{content}"
    );
    assert!(content.contains("\"kind\":\"finish\""), "{content}");
    // 禁止记录 Authorization / api key。
    assert!(!content.contains("authorization"), "{content}");
    assert!(!content.contains("api_key"), "{content}");
    assert!(!content.contains("test-key"), "API key 不得落盘: {content}");
}

/// Phase G（任务书 §26/§37）：provider 失败不得破坏 session——
/// run_prompt_once 返回 Err，但 session 已持久化 UserSubmitted +
/// RunCompleted(Error)，可恢复继续。
#[tokio::test]
async fn provider_failure_keeps_session_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = fixtures::test_config(&workspace);

    struct FailingProvider;
    impl tpi::provider::Provider for FailingProvider {
        fn model_name(&self) -> &str {
            "failing"
        }
        async fn stream(
            &mut self,
            _request: tpi::provider::ModelRequest,
            _events: mpsc::Sender<tpi::provider::ProviderEvent>,
            _cancel: CancellationToken,
        ) -> Result<tpi::provider::ProviderResponse, tpi::provider::ProviderError> {
            Err(tpi::provider::ProviderError::Connection(
                "simulated outage".into(),
            ))
        }
    }

    let mut provider = FailingProvider;
    let mut session = tpi::session::SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        tpi::ids::RunId::new_v7(),
    )
    .unwrap();
    let current_cancel: std::sync::Arc<std::sync::Mutex<Option<CancellationToken>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    let result = tpi::app::run_prompt_once(
        &mut provider,
        &mut session,
        &config,
        &[],
        "hello".into(),
        current_cancel.clone(),
    )
    .await;
    assert!(result.is_err(), "provider 失败必须传播为 Err");

    // session 保留：UserSubmitted 已持久化 + RunCompleted(ProviderUnavailable)，可恢复。
    let events = tpi::session::read_events(session.path()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.type_name()).collect();
    assert!(
        kinds.contains(&"user_submitted"),
        "用户消息必须已持久化: {kinds:?}"
    );
    assert!(
        kinds.contains(&"run_completed"),
        "失败原因必须已持久化: {kinds:?}"
    );
    let reason = events
        .iter()
        .find_map(|e| match e {
            tpi::session::SessionEvent::RunCompleted { reason, .. } => Some(*reason),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        reason,
        tpi::session::CompletionReason::ProviderUnavailable,
        "连接失败（未收到任何事件）reason 必须是 ProviderUnavailable（§4.3，此前归为 Error）"
    );
}

#[allow(deprecated)]
// SO_LINGER 是触发 RST 的标准手段；tokio 标记 deprecated（drop 可能阻塞线程）。
/// 模拟 SSE 中途断开：返回头部 + 一个事件后以 RST（SO_LINGER=0）断开。
/// 最多接受 4 个连接（若客户端错误地重试，计数会 >1；修复后应恒为 1）。
async fn mock_sse_server_rst_after_partial(
    partial: &'static str,
) -> (String, tokio::task::JoinHandle<usize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut conns = 0usize;
        for _ in 0..4 {
            let accept = tokio::time::timeout(Duration::from_secs(3), listener.accept()).await;
            let Ok(Ok((mut socket, _))) = accept else {
                break;
            };
            conns += 1;
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = socket.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            })
            .await;
            let response =
                format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{partial}");
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = socket.set_linger(Some(Duration::from_secs(0)));
            drop(socket);
        }
        conns
    });
    (format!("http://{addr}/v1"), handle)
}

/// §7.3/§15：SSE 中途断开且已收到事件 → 必须返回错误且不得重试
/// （否则重发请求会重复已到达的文本/工具调用）。
#[tokio::test]
async fn sse_midstream_error_after_events_does_not_retry() {
    let partial = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"部分文本\"},\"finish_reason\":null}]}\n\n";
    let (base_url, conns) = mock_sse_server_rst_after_partial(partial).await;
    let mut client = OpenAiCompatClient::new(base_url, "m".into(), "k".into(), None, None, None);
    let request = ModelRequest {
        model: "m".into(),
        messages: vec![ChatMessage::User("hi".into())],
        tools: Vec::new(),
        max_output_tokens: None,
        reasoning: None,
        context_window: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = client
        .stream(request, tx.clone(), CancellationToken::new())
        .await;
    drop(tx);
    drain.abort();
    assert!(result.is_err(), "SSE 中途断开（已收到事件）必须是错误");
    assert_eq!(
        conns.await.unwrap(),
        1,
        "收到事件后不得重试（否则重复文本/工具调用）"
    );
}
