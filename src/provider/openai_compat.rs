//! OpenAI-compatible Chat Completions adapter（文档 §7.1/§7.3）。
//!
//! 职责：SSE 分帧、tool argument 增量拼接、`reasoning_content`、finish reason、
//! usage 归一化；重试只发生在流开始之前，流中出现 delta 后绝不自动重放（§7.3）。

use std::time::Duration;

use crate::ids::ToolCallId;
use crate::provider::{
    ChatMessage, FinishReason, ModelRequest, Provider, ProviderError, ProviderEvent,
    ProviderResponse,
};
use crate::session::Usage;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// 重试上限（§7.3：最多重试 2 次）。
const MAX_RETRIES: u32 = 2;

pub struct OpenAiCompatClient {
    base_url: String,
    model: String,
    api_key: String,
    reasoning: Option<String>,
    max_output_tokens: Option<u32>,
    client: reqwest::Client,
}

impl OpenAiCompatClient {
    pub fn new(
        base_url: String,
        model: String,
        api_key: String,
        reasoning: Option<String>,
        max_output_tokens: Option<u32>,
        _context_window: Option<u64>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
            reasoning,
            max_output_tokens,
            client: reqwest::Client::new(),
        }
    }

    fn build_body(&self, request: &ModelRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> =
            request.messages.iter().map(message_to_json).collect();
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect();
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("messages".into(), json!(messages));
        if !tools.is_empty() {
            body.insert("tools".into(), json!(tools));
        }
        if let Some(reasoning) = self.reasoning.as_deref().or(request.reasoning.as_deref()) {
            body.insert("reasoning".into(), json!(reasoning));
        }
        if let Some(max_tokens) = self.max_output_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }
        body.insert("stream".into(), json!(true));
        serde_json::Value::Object(body)
    }
}

fn message_to_json(message: &ChatMessage) -> serde_json::Value {
    match message {
        ChatMessage::System(content) => json!({ "role": "system", "content": content }),
        ChatMessage::User(content) => json!({ "role": "user", "content": content }),
        ChatMessage::Assistant {
            content,
            tool_calls,
        } => {
            let calls: Vec<serde_json::Value> = tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.provider_id,
                        "type": "function",
                        "function": { "name": call.name, "arguments": call.arguments },
                    })
                })
                .collect();
            json!({ "role": "assistant", "content": content, "tool_calls": calls })
        }
        ChatMessage::Tool {
            tool_call_id,
            name: _,
            content,
        } => json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        }),
    }
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    choices: Vec<SseChoice>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: SseDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<SseToolCallDelta>,
}

#[derive(Deserialize)]
struct SseToolCallDelta {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<SseFunctionDelta>,
}

#[derive(Deserialize, Default)]
struct SseFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct SseUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

/// 流中 tool call 的组装状态。
struct PendingToolCall {
    provider_id: Option<String>,
    name: String,
    arguments: String,
}

impl Provider for OpenAiCompatClient {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn stream(
        &mut self,
        request: ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_body(&request);

        // §7.3：只在尚未收到任何 response event 前重试。
        for attempt in 0..=MAX_RETRIES {
            let response = self
                .send_once(&url, &body, &cancel)
                .await
                .map_err(|error| classify_error(error, attempt))?;
            match response {
                SendResult::Ok(response) => {
                    return consume_stream(response, events, cancel).await;
                }
                SendResult::Retryable { retry_after } => {
                    let delay = retry_after.unwrap_or(Duration::from_secs(1));
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
        Err(ProviderError::Connection("retry budget exhausted".into()))
    }
}

enum SendResult {
    Ok(reqwest::Response),
    Retryable { retry_after: Option<Duration> },
}

impl OpenAiCompatClient {
    async fn send_once(
        &self,
        url: &str,
        body: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<SendResult, String> {
        let request = self.client.post(url).bearer_auth(&self.api_key).json(body);
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".into()),
            result = request.send() => result.map_err(|e| e.to_string())?,
        };
        let status = response.status();
        if status.is_success() {
            return Ok(SendResult::Ok(response));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Ok(SendResult::Retryable { retry_after });
        }
        let status_text = status.as_u16();
        let message = match status {
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                return Err(format!("auth ({status_text})"));
            }
            _ => format!("http {status_text}"),
        };
        Err(message)
    }
}

fn classify_error(message: String, attempt: u32) -> ProviderError {
    if message == "cancelled" {
        return ProviderError::Cancelled;
    }
    if message.starts_with("auth") {
        return ProviderError::Auth(message);
    }
    if message.starts_with("http 429") {
        return ProviderError::RateLimited(format!("attempt {attempt}: {message}"));
    }
    ProviderError::Connection(format!("attempt {attempt}: {message}"))
}

async fn consume_stream(
    response: reqwest::Response,
    events: tokio::sync::mpsc::Sender<ProviderEvent>,
    cancel: CancellationToken,
) -> Result<ProviderResponse, ProviderError> {
    let mut stream = response.bytes_stream().eventsource();
    let mut pending: Vec<PendingToolCall> = Vec::new();
    let mut finish_reason: Option<FinishReason> = None;
    let mut usage = Usage::default();
    let mut received_any = false;

    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            next = stream.next() => next,
        };
        let Some(event) = next else { break };
        let event = event.map_err(|e| ProviderError::Protocol(e.to_string()))?;
        if event.event == "error" {
            return Err(ProviderError::Protocol(event.data));
        }
        if event.event == "ping" || event.data.trim().is_empty() {
            continue;
        }
        // `[DONE]` 标记。
        if event.data.trim() == "[DONE]" {
            break;
        }
        let chunk: SseChunk = serde_json::from_str(&event.data)
            .map_err(|e| ProviderError::Protocol(format!("invalid chunk: {e}")))?;
        received_any = true;
        for choice in &chunk.choices {
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                let _ = events.send(ProviderEvent::TextDelta(content.clone())).await;
            }
            if let Some(reasoning) = &choice.delta.reasoning_content
                && !reasoning.is_empty()
            {
                let _ = events
                    .send(ProviderEvent::ReasoningDelta(reasoning.clone()))
                    .await;
            }
            for call in &choice.delta.tool_calls {
                let index = call.index.unwrap_or(0) as usize;
                if pending.len() <= index {
                    pending.resize_with(index + 1, || PendingToolCall {
                        provider_id: None,
                        name: String::new(),
                        arguments: String::new(),
                    });
                }
                let slot = &mut pending[index];
                if let Some(id) = &call.id {
                    slot.provider_id = Some(id.clone());
                }
                if let Some(name) = &call.function.as_ref().and_then(|f| f.name.as_ref()) {
                    slot.name = name.to_string();
                    let _ = events
                        .send(ProviderEvent::ToolCallStarted {
                            index: index as u32,
                            id: slot.provider_id.clone().unwrap_or_default(),
                            name: name.to_string(),
                        })
                        .await;
                }
                if let Some(arguments) = call.function.as_ref().and_then(|f| f.arguments.as_ref())
                    && !arguments.is_empty()
                {
                    slot.arguments.push_str(arguments);
                    let _ = events
                        .send(ProviderEvent::ToolArgumentsDelta {
                            index: index as u32,
                            chunk: arguments.clone(),
                        })
                        .await;
                }
            }
            if let Some(reason) = &choice.finish_reason {
                finish_reason = Some(parse_finish_reason(reason));
            }
        }
        if let Some(usage_value) = &chunk.usage {
            usage = Usage {
                input_tokens: usage_value.prompt_tokens.unwrap_or(0),
                output_tokens: usage_value.completion_tokens.unwrap_or(0),
            };
        }
    }

    if !received_any {
        return Err(ProviderError::Protocol("empty stream".into()));
    }

    // §7.3：tool arguments 不完整（未闭合 JSON）时返回协议错误，不能猜补 JSON。
    let tool_calls = pending
        .into_iter()
        .filter(|call| !call.name.is_empty())
        .map(|call| {
            let arguments = call.arguments;
            if !arguments.trim().is_empty() {
                serde_json::from_str::<serde_json::Value>(&arguments)
                    .map_err(|_| ProviderError::Protocol("incomplete tool arguments".into()))?;
            }
            let provider_id = call.provider_id.unwrap_or_default();
            Ok(crate::provider::ToolCall {
                call_id: ToolCallId::new_v7(),
                provider_id,
                name: call.name,
                arguments,
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;

    let finish_reason = finish_reason.unwrap_or(FinishReason::Error);
    Ok(ProviderResponse {
        finish_reason,
        usage,
        tool_calls,
    })
}

fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(parse_finish_reason("stop"), FinishReason::Stop);
        assert_eq!(parse_finish_reason("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(parse_finish_reason("length"), FinishReason::Length);
        assert_eq!(
            parse_finish_reason("content_filter"),
            FinishReason::ContentFilter
        );
        assert_eq!(parse_finish_reason("weird"), FinishReason::Error);
    }

    #[test]
    fn message_serialization_shape() {
        let message = ChatMessage::Tool {
            tool_call_id: "call_1".into(),
            name: "read".into(),
            content: "status: ok".into(),
        };
        let value = message_to_json(&message);
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_call_id"], "call_1");
        assert_eq!(value["content"], "status: ok");
    }

    #[test]
    fn body_contains_model_and_tools() {
        let client = OpenAiCompatClient::new(
            "https://example.invalid/v1".into(),
            "test-model".into(),
            "key".into(),
            None,
            Some(1024),
            None,
        );
        let request = ModelRequest {
            model: "test-model".into(),
            messages: vec![ChatMessage::User("hi".into())],
            tools: vec![crate::provider::ToolDef {
                name: "read".into(),
                description: "read".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_output_tokens: None,
            reasoning: None,
            context_window: None,
        };
        let body = client.build_body(&request);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["max_tokens"], 1024);
    }
}
