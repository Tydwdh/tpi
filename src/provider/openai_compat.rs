//! OpenAI-compatible Chat Completions adapter（文档 §7.1/§7.3）。
//!
//! 职责：SSE 分帧、tool argument 增量拼接、`reasoning_content`、finish reason、
//! usage 归一化；重试只发生在流开始之前，流中出现 delta 后绝不自动重放（§7.3）。

use std::time::Duration;

use crate::ids::ToolCallId;
use crate::provider::trace;
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

/// OpenAI-compatible adapter（P0-4：只保存连接级状态）。
///
/// 请求级配置（model / reasoning / max_output_tokens / context_window）
/// 全部来自 [`ModelRequest`]——client 不复制任何请求级字段，避免双事实源
/// （此前 compaction 的 `max_output_tokens=1024` 被 client 自身字段覆盖）。
pub struct OpenAiCompatClient {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiCompatClient {
    /// `model/reasoning/max_output_tokens/context_window` 是请求级配置，
    /// 由每次 [`ModelRequest`] 携带；此处仅保留连接级参数（签名保留以便
    /// 调用方最小改动，多余参数被忽略）。
    pub fn new(
        base_url: String,
        _model: String,
        api_key: String,
        _reasoning: Option<String>,
        _max_output_tokens: Option<u32>,
        _context_window: Option<u64>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
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
        if let Some(reasoning) = &request.reasoning {
            body.insert("reasoning".into(), json!(reasoning));
        }
        if let Some(max_tokens) = request.max_output_tokens {
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
            let mut message = json!({ "role": "assistant", "content": content });
            // 真实 provider 边界（§9）：opencode-go 拒绝 `tool_calls: []`
            // （要求省略或最小长度 1）。空数组必须省略。
            if !calls.is_empty() {
                message["tool_calls"] = json!(calls);
            }
            message
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
        // P0-4：client 不保存模型（请求级配置）；模型名由 UI 从 config 读取。
        "openai-compat"
    }

    async fn stream(
        &mut self,
        request: ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_body(&request);

        // §8：provider trace（本地调试；TPI_TRACE_PROVIDER=1）。
        if trace::enabled() {
            let mut fields = serde_json::Map::new();
            fields.insert("url".into(), json!(url));
            fields.insert("model".into(), json!(request.model));
            fields.insert("max_output_tokens".into(), json!(request.max_output_tokens));
            fields.insert("reasoning".into(), json!(request.reasoning));
            fields.insert("tool_count".into(), json!(request.tools.len()));
            fields.insert("message_count".into(), json!(request.messages.len()));
            if trace::include_body() {
                fields.insert("request_body".into(), body.clone());
            }
            trace::log("request_start", fields);
        }

        // §7.3：只在尚未收到任何 response event 前重试。请求发送阶段与流读取阶段
        // （consume_stream）的传输错误在未收到任何事件时同样可安全重试（网络抖动/
        // 连接中途断开），收到事件后失败则不可重试（避免事件重复/乱序）。
        for attempt in 0..=MAX_RETRIES {
            let response = match self.send_once(&url, &body, &cancel).await {
                Ok(response) => response,
                Err(error) => {
                    // 用户取消：不是输输失败，直接以 Cancelled 返回，
                    // 不得记录“重试”告警也不重发请求。
                    if error == "cancelled" {
                        return Err(ProviderError::Cancelled);
                    }
                    // 传输层失败（连接被拒/中断）：未收到任何事件，可安全重试。
                    if attempt == MAX_RETRIES {
                        return Err(classify_error(error, attempt));
                    }
                    tracing::warn!(
                        attempt,
                        error = %error,
                        "provider: 请求发送失败，重试",
                    );
                    let delay = Duration::from_secs(1);
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            };
            match response {
                SendResult::Ok(response) => {
                    let status = response.status();
                    if trace::enabled() {
                        let mut fields = serde_json::Map::new();
                        fields.insert("status".into(), json!(status.as_u16()));
                        trace::log("response_status", fields);
                    }
                    match consume_stream(response, events.clone(), cancel.clone()).await {
                        ConsumeResult::Ok(response) => return Ok(response),
                        ConsumeResult::Failed { error, retryable } => {
                            if !retryable || matches!(error, ProviderError::Cancelled) {
                                return Err(error);
                            }
                            if attempt == MAX_RETRIES {
                                return Err(error);
                            }
                            // 未收到任何事件：传输层失败，短暂等待后重发请求。
                            tracing::warn!(
                                attempt,
                                error = %error,
                                "provider: 响应流读取失败且未收到事件，重试",
                            );
                            let delay = Duration::from_secs(1);
                            tokio::select! {
                                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                                _ = tokio::time::sleep(delay) => {}
                            }
                        }
                    }
                }
                SendResult::Retryable { retry_after } => {
                    if trace::enabled() {
                        let mut fields = serde_json::Map::new();
                        fields.insert(
                            "retry_after_secs".into(),
                            json!(retry_after.map(|d| d.as_secs())),
                        );
                        trace::log("retryable", fields);
                    }
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

/// 流消费结果：区分可安全重试（未收到任何事件）与不可重试的失败。
enum ConsumeResult {
    Ok(ProviderResponse),
    Failed {
        error: ProviderError,
        /// 未收到任何 response 事件（§7.3：可安全重发请求）。
        retryable: bool,
    },
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
            _ => {
                // §27：4xx/5xx 错误带 provider body（诊断；截断避免刷屏）。
                // 此前只报 status code，真实 400 的具体原因完全不可见。
                let body = response.text().await.unwrap_or_default();
                let snippet: String = body.chars().take(300).collect();
                format!("http {status_text}: {snippet}")
            }
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

/// §7.3/§15：SSE 传输错误是否可安全重试。
/// 收到任何事件后重试会重复内容（重复 assistant 文本/工具调用），必须不可重试。
fn sse_transport_error_retryable(received_any: bool) -> bool {
    !received_any
}
async fn consume_stream(
    response: reqwest::Response,
    events: tokio::sync::mpsc::Sender<ProviderEvent>,
    cancel: CancellationToken,
) -> ConsumeResult {
    let mut stream = response.bytes_stream().eventsource();
    let mut pending: Vec<PendingToolCall> = Vec::new();
    let mut finish_reason: Option<FinishReason> = None;
    let mut usage = Usage::default();
    let mut received_any = false;
    // 正常 OpenAI SSE 流以 `[DONE]` 结束；EOF 前未见 [DONE] = 流被截断
    // （此前静默丢失尾部内容——事件中间中断被当成正常结束）。
    let mut saw_done = false;

    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                return ConsumeResult::Failed {
                    error: ProviderError::Cancelled,
                    retryable: false,
                }
            }
            next = stream.next() => next,
        };
        let Some(event) = next else { break };
        // 传输层错误（连接中断/响应体解码失败等）在未收到事件时可安全重试。
        let event = match event {
            Ok(event) => event,
            Err(e) => {
                return ConsumeResult::Failed {
                    error: ProviderError::Protocol(e.to_string()),
                    // §7.3/§15：只有未收到任何事件时才能重试——否则重发请求会重复
                    // 已到达的文本/工具调用（此前恒为 true，SSE 中途断开会重复内容）。
                    retryable: sse_transport_error_retryable(received_any),
                };
            }
        };
        if trace::enabled() {
            let mut fields = serde_json::Map::new();
            fields.insert("event_type".into(), json!(event.event));
            fields.insert("data_len".into(), json!(event.data.len()));
            trace::log("sse_event", fields);
        }
        if event.event == "error" {
            return ConsumeResult::Failed {
                error: ProviderError::Protocol(event.data),
                retryable: false,
            };
        }
        if event.event == "ping" || event.data.trim().is_empty() {
            continue;
        }
        // `[DONE]` 标记。
        if event.data.trim() == "[DONE]" {
            saw_done = true;
            break;
        }
        let chunk: SseChunk = match serde_json::from_str(&event.data) {
            Ok(chunk) => chunk,
            Err(e) => {
                return ConsumeResult::Failed {
                    error: ProviderError::Protocol(format!("invalid chunk: {e}")),
                    retryable: false,
                };
            }
        };
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
                    if trace::enabled() {
                        let mut fields = serde_json::Map::new();
                        fields.insert("index".into(), json!(index));
                        fields.insert("name".into(), json!(name));
                        fields.insert("provider_id".into(), json!(slot.provider_id));
                        trace::log("tool_call_started", fields);
                    }
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
                    if trace::enabled() {
                        let mut fields = serde_json::Map::new();
                        fields.insert("index".into(), json!(index));
                        fields.insert("arguments_len".into(), json!(arguments.len()));
                        trace::log("tool_arguments_delta", fields);
                    }
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

    if !saw_done {
        // EOF 前未见 [DONE]。部分 provider 不发 [DONE] 但发 finish_reason——
        // 视为正常结束；两者都无 = 流被截断（服务器中断/网络丢包）。
        if finish_reason.is_some() {
            // 正常结束（宽松 provider 路径）。
        } else if received_any {
            // 已发出事件：不可重试（避免事件重复/乱序）。
            return ConsumeResult::Failed {
                error: ProviderError::Protocol(
                    "stream ended before [DONE] and no finish_reason".into(),
                ),
                retryable: false,
            };
        } else {
            return ConsumeResult::Failed {
                error: ProviderError::Protocol("empty stream".into()),
                retryable: true,
            };
        }
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
        .collect::<Result<Vec<_>, ProviderError>>()
        .map_err(|error| ConsumeResult::Failed {
            error,
            retryable: false,
        });
    let tool_calls = match tool_calls {
        Ok(tool_calls) => tool_calls,
        Err(failed) => return failed,
    };

    // §30 第 11 条：finish=tool_calls 但没有任何完整 call → 明确协议错误
    // （此前静默返回 Ok，agent 侧看到空 calls 无法推进）。
    let finish_reason = finish_reason.unwrap_or(FinishReason::Error);
    if finish_reason == FinishReason::ToolCalls && tool_calls.is_empty() {
        return ConsumeResult::Failed {
            error: ProviderError::Protocol(
                "finish_reason=tool_calls but no tool calls received".into(),
            ),
            retryable: false,
        };
    }

    if trace::enabled() {
        let mut fields = serde_json::Map::new();
        fields.insert("finish_reason".into(), json!(format!("{finish_reason:?}")));
        fields.insert("input_tokens".into(), json!(usage.input_tokens));
        fields.insert("output_tokens".into(), json!(usage.output_tokens));
        fields.insert("tool_call_count".into(), json!(tool_calls.len()));
        trace::log("finish", fields);
    }
    ConsumeResult::Ok(ProviderResponse {
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

    /// 真实 provider 边界（§9）：assistant 无 tool_calls 时不得输出空数组
    /// （opencode-go 拒绝 `tool_calls: []`）。
    #[test]
    fn assistant_without_tool_calls_omits_the_field() {
        let message = ChatMessage::Assistant {
            content: "done".into(),
            tool_calls: Vec::new(),
        };
        let value = message_to_json(&message);
        assert_eq!(value["role"], "assistant");
        assert!(
            value.get("tool_calls").is_none(),
            "空 tool_calls 必须省略: {value}"
        );

        let with_calls = ChatMessage::Assistant {
            content: "reading".into(),
            tool_calls: vec![crate::provider::ToolCall {
                call_id: crate::ids::ToolCallId::new_v7(),
                provider_id: "call_x".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
        };
        let value = message_to_json(&with_calls);
        assert_eq!(value["tool_calls"][0]["id"], "call_x");
    }

    #[test]
    fn body_contains_model_and_tools() {
        let client = OpenAiCompatClient::new(
            "https://example.invalid/v1".into(),
            "ignored-model".into(),
            "key".into(),
            None,
            None,
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
            max_output_tokens: Some(1024),
            reasoning: None,
            context_window: None,
        };
        let body = client.build_body(&request);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        // P0-4：max_tokens 只来自 request（client 不再持有请求级配置）。
        assert_eq!(body["max_tokens"], 1024);
    }

    /// P0-4：client 构造参数（model/reasoning/max_output_tokens）不再影响 body。
    #[test]
    fn client_config_never_leaks_into_body() {
        let client = OpenAiCompatClient::new(
            "https://example.invalid/v1".into(),
            "client-model".into(),
            "key".into(),
            Some("client-reasoning".into()),
            Some(999),
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
        let body = client.build_body(&request);
        assert_eq!(body["model"], "request-model");
        assert!(body.get("max_tokens").is_none(), "{body}");
        assert!(body.get("reasoning").is_none(), "{body}");
    }
}

/// §7.3/§15：SSE 传输错误只有未收到事件时才可重试（收到后重试会重复内容）。
#[test]
fn sse_transport_error_retryable_only_before_any_event() {
    assert!(sse_transport_error_retryable(false), "未收到事件：可重试");
    assert!(
        !sse_transport_error_retryable(true),
        "已收到事件：不可重试（避免重复文本/工具调用）"
    );
}
