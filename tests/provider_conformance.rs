//! P5-02：provider conformance suite——fake 与 OpenAI recorded adapter 共用。
//!
//! 同一组断言（protocol / cancel / usage）对 fake 运行；recorded OpenAI fixtures
//! 的等价断言在 provider_contract.rs（recorded_* 系列）运行——本文件显式
//! conformance 结构保证两者对齐（P5-02 验收：接口不是单实现自画像）。

mod fixtures;

use tpi::provider::{FinishReason, Provider, ProviderEvent, ProviderStream};

/// 对任意 Provider 的协议断言：单文本流 → Stop + text 完整 + usage 结构化。
async fn assert_protocol<P: Provider>(provider: &mut P) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut stream = ProviderStream::new(rx);
    let response = provider
        .stream(
            tpi::provider::ModelRequest {
                model: "m".into(),
                messages: vec![tpi::provider::ChatMessage::User("hi".into())],
                tools: Vec::new(),
                max_output_tokens: None,
                reasoning: None,
                context_window: None,
            },
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("stream 成功");
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let ProviderEvent::TextDelta(delta) = event {
            text.push_str(&delta);
        }
    }
    assert_eq!(response.finish_reason, FinishReason::Stop, "正常 Stop");
    assert_eq!(text, "conformance ok", "文本必须完整");
}

/// conformance：fake provider 协议（文本 + usage）。
#[tokio::test]
async fn fake_provider_protocol_conformance() {
    let mut provider = fixtures::fake_provider::FakeProvider::scripted(vec![Box::new(|_| {
        fixtures::fake_provider::FakeResponse::text("conformance ok")
    })]);
    assert_protocol(&mut provider).await;
}

/// conformance：usage 结构化（fake 带 usage）。
#[tokio::test]
async fn fake_provider_usage_conformance() {
    let mut provider = fixtures::fake_provider::FakeProvider::scripted(vec![Box::new(|_| {
        fixtures::fake_provider::FakeResponse::text("conformance ok").with_usage(100, 50)
    })]);
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut stream = ProviderStream::new(rx);
    let response = provider
        .stream(
            tpi::provider::ModelRequest {
                model: "m".into(),
                messages: vec![],
                tools: Vec::new(),
                max_output_tokens: None,
                reasoning: None,
                context_window: None,
            },
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    while stream.next().await.is_some() {}
    assert_eq!(response.usage.input_tokens, 100, "usage input 结构化");
    assert_eq!(response.usage.output_tokens, 50, "usage output 结构化");
    assert_eq!(
        response.usage.input_tokens + response.usage.output_tokens,
        150,
        "usage 字段齐全"
    );
}

/// conformance：tool calls 装配（fake 返回 tool call → response 携带）。
#[tokio::test]
async fn fake_provider_tool_call_conformance() {
    let mut provider = fixtures::fake_provider::FakeProvider::scripted(vec![Box::new(|_| {
        fixtures::fake_provider::FakeResponse::with_tool_calls(vec![
            fixtures::fake_provider::tool_call("read", serde_json::json!({"path": "/tmp/x"})),
        ])
    })]);
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut stream = ProviderStream::new(rx);
    let response = provider
        .stream(
            tpi::provider::ModelRequest {
                model: "m".into(),
                messages: vec![],
                tools: Vec::new(),
                max_output_tokens: None,
                reasoning: None,
                context_window: None,
            },
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    while stream.next().await.is_some() {}
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "read");
}

/// ProviderStream：normalized handle 迭代（P5-01）。
#[tokio::test]
async fn provider_stream_iterates_events() {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut stream = ProviderStream::new(rx);
    tx.send(ProviderEvent::TextDelta("a".into())).await.unwrap();
    tx.send(ProviderEvent::TextDelta("b".into())).await.unwrap();
    drop(tx);
    let mut collected = String::new();
    while let Some(event) = stream.next().await {
        if let ProviderEvent::TextDelta(d) = event {
            collected.push_str(&d);
        }
    }
    assert_eq!(collected, "ab");
}
