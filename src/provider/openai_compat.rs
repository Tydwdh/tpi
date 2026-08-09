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

/// 最大尝试次数（含首次请求；§7.3：指数退避，最多 4 次）。
const MAX_ATTEMPTS: u32 = 4;
/// 重试等待总预算（§7.3：只计「重试等待」的累计，不计单次请求耗时——
/// 防止服务端持续 Retry-After 时无限重试；单次请求本身由 connect_timeout
/// 和 cancel 约束，SSE 流读取无总超时）。
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);
/// 首次退避基准（第 0 次重试）；每次尝试翻倍：500ms → 1s → 2s。
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

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
            // 连接超时（§7.3）：TCP 连接建立上限，防网络 hang 无限阻塞
            // （此前无超时，连接卡住会耗尽重试预算）。
            // 注意：**不设置整体 timeout**——SSE 流可能长时间保持打开
            // （模型长 thinking / tool 间无 token），reqwest 的 `.timeout()`
            // 是整请求总超时，会误杀长流（`error decoding response body`）。
            // 流读取空闲由 consume_stream 的 cancel 处理。
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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
        // §PointerHit 9：请求流式 usage——很多 OpenAI 兼容服务需显式
        // stream_options.include_usage 才返回 usage，否则 token/成本/评测为 0。
        body.insert(
            "stream_options".into(),
            json!({ "include_usage": true }),
        );
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
    /// OpenAI-compatible：`prompt_tokens_details.cached_tokens`（缓存命中）。
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
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
        //
        // 退避策略（§7.3）：指数退避 + jitter；最多 `MAX_ATTEMPTS` 次尝试，
        // 且「重试等待」累计不超过 `MAX_RETRY_WAIT`（防服务端持续 Retry-After
        // 时无限重试）。单次请求耗时不计入预算（由 connect_timeout / cancel
        // 约束；SSE 流读取无总超时）。
        let mut attempt: u32 = 0;
        let mut retry_wait = Duration::ZERO;
        loop {
            let response = match self.send_once(&url, &body, &cancel).await {
                Ok(response) => response,
                Err(error) => {
                    // 用户取消：不是输输失败，直接以 Cancelled 返回，
                    // 不得记录“重试”告警也不重发请求。
                    if error == "cancelled" {
                        return Err(ProviderError::Cancelled);
                    }
                    // 传输层失败（连接被拒/中断）：未收到任何事件，可安全重试。
                    if attempt + 1 >= MAX_ATTEMPTS || retry_wait >= MAX_RETRY_WAIT {
                        return Err(classify_error(error, attempt));
                    }
                    let delay = backoff_delay(attempt, None);
                    tracing::warn!(
                        attempt,
                        error = %error,
                        backoff_ms = delay.as_millis(),
                        "provider: 请求发送失败，重试",
                    );
                    if !wait_or_cancelled(&cancel, delay).await {
                        return Err(ProviderError::Cancelled);
                    }
                    retry_wait += delay;
                    attempt += 1;
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
                            if attempt + 1 >= MAX_ATTEMPTS || retry_wait >= MAX_RETRY_WAIT {
                                return Err(error);
                            }
                            // 未收到任何事件：传输层失败，短暂等待后重发请求。
                            let delay = backoff_delay(attempt, None);
                            tracing::warn!(
                                attempt,
                                error = %error,
                                backoff_ms = delay.as_millis(),
                                "provider: 响应流读取失败且未收到事件，重试",
                            );
                            if !wait_or_cancelled(&cancel, delay).await {
                                return Err(ProviderError::Cancelled);
                            }
                            retry_wait += delay;
                            attempt += 1;
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
                    if attempt + 1 >= MAX_ATTEMPTS || retry_wait >= MAX_RETRY_WAIT {
                        return Err(ProviderError::RateLimited(format!(
                            "attempt {attempt}: retry budget exhausted"
                        )));
                    }
                    let delay = backoff_delay(attempt, retry_after);
                    tracing::warn!(
                        attempt,
                        backoff_ms = delay.as_millis(),
                        "provider: 服务端要求退避，等待后重试",
                    );
                    if !wait_or_cancelled(&cancel, delay).await {
                        return Err(ProviderError::Cancelled);
                    }
                    retry_wait += delay;
                    attempt += 1;
                }
            }
        }
    }
}

/// 计算本次重试的等待时长（§7.3）：
/// - `retry_after`（服务端 `Retry-After`）大于本地退避时尊重服务端；
/// - 否则指数退避 `500ms * 2^attempt` + 随机 jitter（±40%）。
fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let base = INITIAL_BACKOFF * (1u32 << attempt.min(3));
    let jitter = if cfg!(test) {
        1.0
    } else {
        0.6 + random_ratio() * 0.8
    };
    let local = base.mul_f64(jitter);
    match retry_after {
        Some(server) if server > local => server,
        _ => local,
    }
}

/// 0..1 的伪随机比例（jitter 用；不用引全局 RNG 依赖）。
fn random_ratio() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    ((nanos % 10_000) as f64) / 10_000.0
}

/// 等待 `delay`；期间用户取消则返回 false（不重发请求）。
async fn wait_or_cancelled(cancel: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
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
        // §7.3/§15：`received_any` 表示「已收到语义内容」（text/reasoning/tool/
        // finish_reason）。只有收到语义内容后断流才不可重试（避免重复内容）；
        // 空 chunk / usage-only chunk（如流开头的 usage 包）不算——它们不会导致
        // 重复，收到后断流仍应可重试（否则长响应在开头被 usage chunk 标记为
        // 不可重试，网络抖动直接 ProviderUnavailable）。
        let mut chunk_had_semantic = false;
        for choice in &chunk.choices {
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                chunk_had_semantic = true;
                let _ = events.send(ProviderEvent::TextDelta(content.clone())).await;
            }
            if let Some(reasoning) = &choice.delta.reasoning_content
                && !reasoning.is_empty()
            {
                chunk_had_semantic = true;
                let _ = events
                    .send(ProviderEvent::ReasoningDelta(reasoning.clone()))
                    .await;
            }
            for call in &choice.delta.tool_calls {
                chunk_had_semantic = true;
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
                chunk_had_semantic = true;
                finish_reason = Some(parse_finish_reason(reason));
            }
        }
        if chunk_had_semantic {
            received_any = true;
        }
        if let Some(usage_value) = &chunk.usage {
            usage = Usage {
                input_tokens: usage_value.prompt_tokens.unwrap_or(0),
                output_tokens: usage_value.completion_tokens.unwrap_or(0),
                // §16.2：缓存命中 token（OpenAI-compatible prompt_tokens_details）。
                cache_read_tokens: usage_value
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0),
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

/// §7.3：指数退避，测试环境 jitter=1.0（确定性）。
#[test]
fn backoff_doubles_per_attempt() {
    let first = backoff_delay(0, None);
    let second = backoff_delay(1, None);
    let third = backoff_delay(2, None);
    assert_eq!(first, INITIAL_BACKOFF, "attempt 0 = 初始退避");
    assert_eq!(second, INITIAL_BACKOFF * 2, "attempt 1 翻倍");
    assert_eq!(third, INITIAL_BACKOFF * 4, "attempt 2 再翻倍");
}

/// §7.3：服务端 Retry-After 大于本地退避时尊重服务端；否则用本地退避。
#[test]
fn retry_after_respected_when_larger() {
    let local = backoff_delay(0, None);
    let server_longer = Duration::from_secs(30);
    assert_eq!(backoff_delay(0, Some(server_longer)), server_longer);
    let server_shorter = Duration::from_millis(10);
    assert_eq!(backoff_delay(0, Some(server_shorter)), local);
}

/// §16.2：缓存命中 token 解析——prompt_tokens_details.cached_tokens。
#[test]
fn sse_usage_parses_cached_tokens() {
    let raw = r#"{"prompt_tokens": 100, "completion_tokens": 50, "prompt_tokens_details": {"cached_tokens": 40}}"#;
    let usage: SseUsage = serde_json::from_str(raw).unwrap();
    assert_eq!(usage.prompt_tokens, Some(100));
    assert_eq!(usage.completion_tokens, Some(50));
    assert_eq!(
        usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        Some(40),
        "缓存命中 token 必须解析"
    );

    // 无 details 字段（部分 provider）不报错。
    let raw2 = r#"{"prompt_tokens": 10, "completion_tokens": 5}"#;
    let usage2: SseUsage = serde_json::from_str(raw2).unwrap();
    assert_eq!(
        usage2
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        None,
        "缺 details 时为 None"
    );
}
