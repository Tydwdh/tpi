//! OpenAI-compatible Chat Completions 流式适配器。
//!
//! 职责：SSE 分帧、tool argument 增量拼接、`reasoning_content`、finish reason、
//! usage 归一化；重试只发生在流开始之前，流中出现 delta 后绝不自动重放（§7.3）。

use std::time::Duration;

use crate::provider::trace;
use crate::provider::{
    ChatMessage, FinishReason, ModelRequest, Provider, ProviderError, ProviderEvent,
    ProviderResponse, RetryBudget,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tpi_core::ids::ToolCallId;
use tpi_session::Usage;

/// 最大尝试次数（含首次请求；§7.3：指数退避，最多 10 次 = 9 次重试）。
const MAX_ATTEMPTS: u32 = 10;
/// 重试等待总预算（§7.3：只计「重试等待」的累计，不计单次请求耗时）。
/// 值取“9 次重试全按翻倍退避 + jitter 上限 1.4x”的累计上界（约 358s），
/// 保证本地退避下 10 次尝试都能跑满；同时防止服务端超长 Retry-After
/// （如 1 小时）导致单次等待无限长。
const MAX_RETRY_WAIT: Duration = Duration::from_secs(360);
/// 首次退避基准（第 0 次重试）；每次尝试翻倍：500ms → 1s → 2s → …
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
/// 单个响应允许的 tool call 槽位上限，防止稀疏/恶意 index 触发巨量分配。
const MAX_STREAM_TOOL_CALLS: usize = 256;
const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STREAM_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROVIDER_CALL_ID_BYTES: usize = 1024;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

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
                        "parameters": normalize_parameters(&tool.parameters),
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
            // OpenAI-compatible gateways (including Ark Coding Plan) expose
            // this as `reasoning_effort`, not `reasoning`.
            // Keep the existing user-facing `max` spelling working by mapping
            // it to Ark's highest supported level.
            let effort = if reasoning.eq_ignore_ascii_case("max") {
                "high"
            } else {
                reasoning.as_str()
            };
            body.insert("reasoning_effort".into(), json!(effort));
        }
        if let Some(max_tokens) = request.max_output_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }
        body.insert("stream".into(), json!(true));
        // §PointerHit 9：请求流式 usage——很多 OpenAI 兼容服务需显式
        // stream_options.include_usage 才返回 usage，否则 token/成本/评测为 0。
        body.insert("stream_options".into(), json!({ "include_usage": true }));
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

/// 协议 invariant 兜底（§provider 边界）：`type: object` 的 tool parameters
/// **必须始终带 `properties`**（min：空 `{}`）。
///
/// schemars 对空结构体只生成 `{"type":"object"}`（见 capabilities 层
/// [`normalize_tool_parameters`]），本处作为发往 provider 前的最终防线：
/// 即便某些工具来源（第三方/MCP/手写 adapter）返回了缺 `properties` 的
/// object schema，也不会触发 LM Studio 0.4.x 等严格 validator 的 HTTP 400
/// （`function.parameters.properties` required）。非 object 参数原样透传。
fn normalize_parameters(parameters: &serde_json::Value) -> serde_json::Value {
    let mut value = match parameters {
        serde_json::Value::Object(map) => map.clone(),
        other => return other.clone(),
    };
    if value.get("type").and_then(|t| t.as_str()) == Some("object") {
        value
            .entry("properties")
            .or_insert_with(|| serde_json::json!({}));
    }
    serde_json::Value::Object(value)
}

/// 容忍数组字段为 `null`（视为空数组）：`#[serde(default)]` 只处理字段缺失，
/// 不处理显式 `null`——真实 OpenAI 兼容服务（DeepSeek/网关等）在纯 usage
/// chunk 或推理帧会发 `"choices": null` / `"delta.tool_calls": null`，
/// 直接反序列化会报 `invalid type: null, expected a sequence`。
/// 空数组语义与缺失一致（调用方已有 `is_empty()` 处理），null → 空是安全降级。
fn deserialize_null_vec<'de, T, D>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    choices: Vec<SseChoice>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Deserialize)]
struct SseChoice {
    #[serde(default)]
    index: Option<u32>,
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
    #[serde(default, deserialize_with = "deserialize_null_vec")]
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

/// Some OpenAI-compatible gateways encode omitted fields in a streamed tool
/// delta as `""` instead of leaving the field out.
fn non_empty_delta(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
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
    announced: bool,
}

/// 在第三方 SSE parser 之前执行的原始帧预算。parser 会跨网络 chunk 累积到
/// 空行；若只在 parser 产出 Event 后检查，永不结束的超长帧仍可耗尽内存。
#[derive(Default)]
struct SseFrameGuard {
    frame_bytes: usize,
    recent: [u8; 4],
    recent_len: usize,
}

impl SseFrameGuard {
    fn inspect(&mut self, chunk: &[u8]) -> Result<(), BoundedSseStreamError> {
        self.inspect_with_limit(chunk, MAX_SSE_EVENT_BYTES)
    }

    fn inspect_with_limit(
        &mut self,
        chunk: &[u8],
        max_frame_bytes: usize,
    ) -> Result<(), BoundedSseStreamError> {
        for byte in chunk {
            self.frame_bytes = self
                .frame_bytes
                .checked_add(1)
                .ok_or(BoundedSseStreamError::FrameTooLarge)?;
            if self.recent_len < self.recent.len() {
                self.recent[self.recent_len] = *byte;
                self.recent_len += 1;
            } else {
                self.recent.copy_within(1.., 0);
                self.recent[3] = *byte;
            }

            let recent = &self.recent[..self.recent_len];
            let delimiter_len = if recent.ends_with(b"\r\n\r\n") {
                Some(4)
            } else if recent.ends_with(b"\n\n") || recent.ends_with(b"\r\r") {
                Some(2)
            } else {
                None
            };
            if let Some(delimiter_len) = delimiter_len {
                if self.frame_bytes.saturating_sub(delimiter_len) > max_frame_bytes {
                    return Err(BoundedSseStreamError::FrameTooLarge);
                }
                self.frame_bytes = 0;
                self.recent_len = 0;
            } else if self.frame_bytes > max_frame_bytes.saturating_add(3) {
                return Err(BoundedSseStreamError::FrameTooLarge);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
enum BoundedSseStreamError {
    Transport(reqwest::Error),
    FrameTooLarge,
}

impl std::fmt::Display for BoundedSseStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(formatter),
            Self::FrameTooLarge => {
                write!(
                    formatter,
                    "SSE frame exceeds {MAX_SSE_EVENT_BYTES} byte limit"
                )
            }
        }
    }
}

impl std::error::Error for BoundedSseStreamError {}

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
        // 预算属于本次逻辑请求，而不是某个 HTTP attempt。请求 body 在循环外
        // 构建且绝不修改，确保 retry 不会偷偷 compact、改 schema 或改参数。
        let mut budget = RetryBudget::new(MAX_ATTEMPTS, MAX_RETRY_WAIT);
        loop {
            if !budget.begin_attempt() {
                return Err(ProviderError::Connection(format!(
                    "retry budget exhausted after {} attempts / {}ms",
                    budget.attempts_started(),
                    budget.elapsed().as_millis()
                )));
            }
            let attempt = budget.attempts_started() - 1;
            if trace::enabled() {
                let mut fields = serde_json::Map::new();
                fields.insert("attempt".into(), json!(budget.attempts_started()));
                fields.insert("waited_ms".into(), json!(budget.waited().as_millis()));
                trace::log("request_attempt", fields);
            }
            let response = match self.send_once(&url, &body, &cancel).await {
                Ok(response) => response,
                Err(error) => {
                    // 用户取消：不是输输失败，直接以 Cancelled 返回，
                    // 不得记录“重试”告警也不重发请求。
                    if error == "cancelled" {
                        return Err(ProviderError::Cancelled);
                    }
                    // §40：确定性非法请求（HTTP 400/422，如 tool schema 校验
                    // 失败）——请求 body 是确定性的，重发相同 JSON 必然再 400，
                    // 直接以 InvalidRequest 终态返回，**不做任何重试/重放**。
                    if error.starts_with("invalid request") {
                        return Err(ProviderError::InvalidRequest(error));
                    }
                    // OpenAI-compatible gateways occasionally surface proxy
                    // failures as 4xx. Give such a result exactly one
                    // defensive replay, then retain its precise error class;
                    // this is deliberately not an unbounded "retry 4xx"
                    // policy. 429 follows the normal bounded retry branch.
                    let defensive_4xx = error.starts_with("auth")
                        || (error.starts_with("http ") && !error.starts_with("http 429"));
                    if defensive_4xx && !defensive_retry_allowed(attempt) {
                        return Err(classify_error(error, attempt));
                    }
                    // 传输层失败（连接被拒/中断）：未收到任何事件，可安全重试。
                    if !budget.permits_retry_after(Duration::ZERO) {
                        return Err(classify_error(error, attempt));
                    }
                    let delay = backoff_delay(attempt, None);
                    if !budget.permits_retry_after(delay) {
                        return Err(classify_error(error, attempt));
                    }
                    tracing::warn!(
                        attempt,
                        error = %error,
                        backoff_ms = delay.as_millis(),
                        "provider: 请求发送失败，重试",
                    );
                    if !wait_or_cancelled(&cancel, delay).await {
                        return Err(ProviderError::Cancelled);
                    }
                    budget.record_wait(delay);
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
                            if trace::enabled() {
                                let mut fields = serde_json::Map::new();
                                fields.insert("error".into(), json!(error.to_string()));
                                fields.insert("retryable".into(), json!(retryable));
                                trace::log("stream_failed", fields);
                            }
                            if !retryable || matches!(error, ProviderError::Cancelled) {
                                return Err(error);
                            }
                            if !budget.permits_retry_after(Duration::ZERO) {
                                return Err(error);
                            }
                            // 未收到任何事件：传输层失败，短暂等待后重发请求。
                            let delay = backoff_delay(attempt, None);
                            if !budget.permits_retry_after(delay) {
                                return Err(error);
                            }
                            tracing::warn!(
                                attempt,
                                error = %error,
                                backoff_ms = delay.as_millis(),
                                "provider: 响应流读取失败且未收到事件，重试",
                            );
                            if !wait_or_cancelled(&cancel, delay).await {
                                return Err(ProviderError::Cancelled);
                            }
                            budget.record_wait(delay);
                        }
                    }
                }
                SendResult::Retryable {
                    retry_after,
                    status,
                } => {
                    if trace::enabled() {
                        let mut fields = serde_json::Map::new();
                        fields.insert(
                            "retry_after_secs".into(),
                            json!(retry_after.map(|d| d.as_secs())),
                        );
                        trace::log("retryable", fields);
                    }
                    if !budget.permits_retry_after(Duration::ZERO) {
                        return Err(retryable_status_error(status, attempt));
                    }
                    let delay = backoff_delay(attempt, retry_after);
                    if !budget.permits_retry_after(delay) {
                        return Err(retryable_status_error(status, attempt));
                    }
                    tracing::warn!(
                        attempt,
                        backoff_ms = delay.as_millis(),
                        "provider: 服务端要求退避，等待后重试",
                    );
                    if !wait_or_cancelled(&cancel, delay).await {
                        return Err(ProviderError::Cancelled);
                    }
                    budget.record_wait(delay);
                }
            }
        }
    }
}

/// A malformed request/auth result gets one replay for imperfect compatible
/// gateways, while subsequent identical 4xx results remain terminal.
fn defensive_retry_allowed(attempt: u32) -> bool {
    attempt == 0
}

/// 计算本次重试的等待时长（§7.3）：
/// - `retry_after`（服务端 `Retry-After`）大于本地退避时尊重服务端；
/// - 否则指数退避 `500ms * 2^attempt` + 随机 jitter（±40%）。
fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    // 每次尝试翻倍且不设上限：500ms * 2^attempt（attempt ≤ MAX_ATTEMPTS-2 = 8，
    // 峰值 2^8 = 128s 基准；u32 溢出不可能）。
    let base = INITIAL_BACKOFF * (1u32 << attempt);
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
    Retryable {
        retry_after: Option<Duration>,
        status: reqwest::StatusCode,
    },
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
            return Ok(SendResult::Retryable {
                retry_after,
                status,
            });
        }
        let status_text = status.as_u16();
        let message = match status {
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                return Err(format!("auth ({status_text})"));
            }
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
                // §40：确定性非法请求（同 JSON 重发必然 400）。读取 provider body
                //（tool schema 校验错误等，诊断用），作为 InvalidRequest 归类，
                // 不进入 defensive_retry / RetryBudget 重试（重试无意义）。
                let snippet = read_error_body(response, cancel).await?;
                return Err(format!("invalid request ({status_text}): {snippet}"));
            }
            _ => {
                // §27：4xx/5xx 错误带 provider body（诊断；截断避免刷屏）。
                // 此前只报 status code，真实 400 的具体原因完全不可见。
                let snippet = read_error_body(response, cancel).await?;
                format!("http {status_text}: {snippet}")
            }
        };
        Err(message)
    }
}

/// 读取错误响应的 provider body（截断；用于诊断消息）。
async fn read_error_body(
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while body.len() < MAX_ERROR_BODY_BYTES {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".into()),
            next = stream.next() => next,
        };
        let Some(chunk) = next else { break };
        let Ok(chunk) = chunk else { break };
        let remaining = MAX_ERROR_BODY_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    let snippet: String = String::from_utf8_lossy(&body).chars().take(300).collect();
    Ok(snippet)
}

fn classify_error(message: String, attempt: u32) -> ProviderError {
    if message == "cancelled" {
        return ProviderError::Cancelled;
    }
    if message.starts_with("auth") {
        return ProviderError::Auth(message);
    }
    // §40：确定性非法请求（HTTP 400/422：tool schema 校验失败等）。同 JSON
    // 重发必然再 400，归为 InvalidRequest，agent 层不得对其自动重启/续写。
    if message.starts_with("invalid request") {
        return ProviderError::InvalidRequest(message);
    }
    if message.starts_with("http 429") {
        return ProviderError::RateLimited(format!("attempt {attempt}: {message}"));
    }
    // ISSUE-009：确定性 4xx（请求格式/内容策略）归为协议错误，agent 层
    // 不把它当瞬时错误做 turn 级重启。
    if message.starts_with("http 4") {
        return ProviderError::Protocol(message);
    }
    ProviderError::Connection(format!("attempt {attempt}: {message}"))
}

fn retryable_status_error(status: reqwest::StatusCode, attempt: u32) -> ProviderError {
    let message = format!("attempt {attempt}: HTTP {status}; retry budget exhausted");
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        ProviderError::RateLimited(message)
    } else {
        ProviderError::Http(message)
    }
}

/// §7.3/§15：SSE 传输错误是否可安全重试。
/// 收到任何事件后重试会重复内容（重复 assistant 文本/工具调用），必须不可重试。
fn sse_transport_error_retryable(received_any: bool) -> bool {
    !received_any
}

/// reqwest 传输错误的类别摘要（诊断用：区分 body 解码失败 / 超时 / 连接重置等）。
fn reqwest_error_kind(error: &reqwest::Error) -> String {
    let mut parts = Vec::new();
    if error.is_timeout() {
        parts.push("timeout");
    }
    if error.is_connect() {
        parts.push("connect");
    }
    if error.is_body() {
        parts.push("body");
    }
    if error.is_decode() {
        parts.push("decode");
    }
    if error.is_redirect() {
        parts.push("redirect");
    }
    if error.is_request() {
        parts.push("request");
    }
    if parts.is_empty() {
        parts.push("unknown");
    }
    parts.join("|")
}
async fn consume_stream(
    response: reqwest::Response,
    events: tokio::sync::mpsc::Sender<ProviderEvent>,
    cancel: CancellationToken,
) -> ConsumeResult {
    let mut frame_guard = SseFrameGuard::default();
    // §诊断：累计已读字节，transport 错误时附上（定位“解码失败是发生在流开头
    // 还是输出中途”，以及是解码/超时/连接重置哪一类）。
    let streamed_bytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let streamed_bytes_for_stream = streamed_bytes.clone();
    let guarded_stream = response.bytes_stream().map(move |result| match result {
        Ok(chunk) => {
            let counter = &streamed_bytes_for_stream;
            counter.fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed);
            frame_guard.inspect(&chunk).map(|()| chunk)
        }
        Err(error) => Err(BoundedSseStreamError::Transport(error)),
    });
    let mut stream = guarded_stream.eventsource();
    let mut pending: Vec<PendingToolCall> = Vec::new();
    let mut finish_reason: Option<FinishReason> = None;
    let mut usage = Usage::default();
    let mut received_any = false;
    // 正常 OpenAI SSE 流以 `[DONE]` 结束；EOF 前未见 [DONE] = 流被截断
    // （此前静默丢失尾部内容——事件中间中断被当成正常结束）。
    let mut saw_done = false;
    let mut streamed_text_bytes = 0usize;
    let mut tool_argument_bytes = 0usize;

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
                // §诊断：消息带错误类别 + 已读字节 + 是否已收到语义事件，
                // 便于区分“流开头就解码失败”（服务端/代理问题）与
                // “输出中途被截断”（网关超时/连接重置）。
                let (kind, detail) = match &e {
                    eventsource_stream::EventStreamError::Transport(inner) => match inner {
                        BoundedSseStreamError::FrameTooLarge => {
                            ("frame_too_large".to_string(), String::new())
                        }
                        BoundedSseStreamError::Transport(err) => {
                            (reqwest_error_kind(err), err.to_string())
                        }
                    },
                    eventsource_stream::EventStreamError::Parser(err) => {
                        ("parse".to_string(), err.to_string())
                    }
                    eventsource_stream::EventStreamError::Utf8(err) => {
                        ("utf8".to_string(), err.to_string())
                    }
                };
                let detail = format!(
                    "stream transport ({kind}): {detail}; events_received={received_any}; bytes_read={}",
                    streamed_bytes.load(std::sync::atomic::Ordering::Relaxed)
                );
                return ConsumeResult::Failed {
                    // §修复：已收到部分语义内容后断流 = StreamInterrupted（agent 可
                    // 自动续写）；未收到任何内容 = Connection（agent 不再续写）。
                    error: if received_any {
                        ProviderError::StreamInterrupted(detail)
                    } else {
                        ProviderError::Connection(detail)
                    },
                    // §7.3/§15：只有未收到任何事件时才能重试——否则重发请求会重复
                    // 已到达的文本/工具调用（此前恒为 true，SSE 中途断开会重复内容）。
                    retryable: sse_transport_error_retryable(received_any),
                };
            }
        };
        if event.data.len() > MAX_SSE_EVENT_BYTES {
            return ConsumeResult::Failed {
                error: ProviderError::Protocol(format!(
                    "SSE event exceeds {MAX_SSE_EVENT_BYTES} byte limit"
                )),
                retryable: false,
            };
        }
        if trace::enabled() {
            let mut fields = serde_json::Map::new();
            fields.insert("event_type".into(), json!(event.event));
            fields.insert("data_len".into(), json!(event.data.len()));
            trace::log("sse_event", fields);
        }
        if event.event == "error" {
            let mut message = event.data;
            tpi_core::util::truncate_to_char_boundary(&mut message, MAX_ERROR_BODY_BYTES);
            return ConsumeResult::Failed {
                error: ProviderError::Protocol(format!("provider error event: {message}")),
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
        if finish_reason.is_some() && !chunk.choices.is_empty() {
            return ConsumeResult::Failed {
                error: ProviderError::Protocol("choice received after finish_reason".into()),
                retryable: false,
            };
        }
        if chunk.choices.len() > 1 {
            return ConsumeResult::Failed {
                error: ProviderError::Protocol(
                    "multiple completion choices are not supported".into(),
                ),
                retryable: false,
            };
        }
        // §7.3/§15：`received_any` 表示「已收到语义内容」（text/reasoning/tool/
        // finish_reason）。只有收到语义内容后断流才不可重试（避免重复内容）；
        // 空 chunk / usage-only chunk（如流开头的 usage 包）不算——它们不会导致
        // 重复，收到后断流仍应可重试（否则长响应在开头被 usage chunk 标记为
        // 不可重试，网络抖动直接 ProviderUnavailable）。
        let mut chunk_had_semantic = false;
        for choice in &chunk.choices {
            if choice.index.unwrap_or(0) != 0 {
                return ConsumeResult::Failed {
                    error: ProviderError::Protocol("unexpected completion choice index".into()),
                    retryable: false,
                };
            }
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                streamed_text_bytes = match streamed_text_bytes.checked_add(content.len()) {
                    Some(total) if total <= MAX_STREAM_TEXT_BYTES => total,
                    _ => {
                        return ConsumeResult::Failed {
                            error: ProviderError::Protocol(format!(
                                "assistant text exceeds {MAX_STREAM_TEXT_BYTES} byte limit"
                            )),
                            retryable: false,
                        };
                    }
                };
                chunk_had_semantic = true;
                if events
                    .send(ProviderEvent::TextDelta(content.clone()))
                    .await
                    .is_err()
                {
                    return ConsumeResult::Failed {
                        error: ProviderError::Cancelled,
                        retryable: false,
                    };
                }
            }
            if let Some(reasoning) = &choice.delta.reasoning_content
                && !reasoning.is_empty()
            {
                streamed_text_bytes = match streamed_text_bytes.checked_add(reasoning.len()) {
                    Some(total) if total <= MAX_STREAM_TEXT_BYTES => total,
                    _ => {
                        return ConsumeResult::Failed {
                            error: ProviderError::Protocol(format!(
                                "stream text exceeds {MAX_STREAM_TEXT_BYTES} byte limit"
                            )),
                            retryable: false,
                        };
                    }
                };
                chunk_had_semantic = true;
                if events
                    .send(ProviderEvent::ReasoningDelta(reasoning.clone()))
                    .await
                    .is_err()
                {
                    return ConsumeResult::Failed {
                        error: ProviderError::Cancelled,
                        retryable: false,
                    };
                }
            }
            for call in &choice.delta.tool_calls {
                chunk_had_semantic = true;
                let index = call.index.unwrap_or(0) as usize;
                if index >= MAX_STREAM_TOOL_CALLS {
                    return ConsumeResult::Failed {
                        error: ProviderError::Protocol(format!(
                            "tool call index {index} exceeds limit {MAX_STREAM_TOOL_CALLS}"
                        )),
                        retryable: false,
                    };
                }
                if pending.len() <= index {
                    pending.resize_with(index + 1, || PendingToolCall {
                        provider_id: None,
                        name: String::new(),
                        arguments: String::new(),
                        announced: false,
                    });
                }
                let slot = &mut pending[index];
                // Ark emits the id/name only in the first tool delta, then
                // represents omitted fields as empty strings in following
                // argument deltas. Treat those placeholders as absent; the
                // completed call below still requires non-empty values.
                if let Some(id) = non_empty_delta(call.id.as_deref()) {
                    if id.len() > MAX_PROVIDER_CALL_ID_BYTES || id.chars().any(char::is_control) {
                        return ConsumeResult::Failed {
                            error: ProviderError::Protocol("invalid tool call id".into()),
                            retryable: false,
                        };
                    }
                    if slot
                        .provider_id
                        .as_ref()
                        .is_some_and(|existing| existing != id)
                    {
                        return ConsumeResult::Failed {
                            error: ProviderError::Protocol(
                                "tool call id changed while streaming".into(),
                            ),
                            retryable: false,
                        };
                    }
                    slot.provider_id.get_or_insert_with(|| id.to_string());
                }
                if let Some(name) = call
                    .function
                    .as_ref()
                    .and_then(|function| function.name.as_deref())
                    .and_then(|name| non_empty_delta(Some(name)))
                {
                    if name.len() > MAX_TOOL_NAME_BYTES || name.chars().any(char::is_control) {
                        return ConsumeResult::Failed {
                            error: ProviderError::Protocol("invalid tool call name".into()),
                            retryable: false,
                        };
                    }
                    if !slot.name.is_empty() && slot.name != name {
                        return ConsumeResult::Failed {
                            error: ProviderError::Protocol(
                                "tool call name changed while streaming".into(),
                            ),
                            retryable: false,
                        };
                    }
                    if slot.name.is_empty() {
                        slot.name = name.to_string();
                    }
                }
                if !slot.announced
                    && !slot.name.is_empty()
                    && let Some(provider_id) = &slot.provider_id
                {
                    slot.announced = true;
                    if trace::enabled() {
                        let mut fields = serde_json::Map::new();
                        fields.insert("index".into(), json!(index));
                        fields.insert("name".into(), json!(slot.name));
                        fields.insert("provider_id".into(), json!(provider_id));
                        trace::log("tool_call_started", fields);
                    }
                    if events
                        .send(ProviderEvent::ToolCallStarted {
                            index: index as u32,
                            id: provider_id.clone(),
                            name: slot.name.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return ConsumeResult::Failed {
                            error: ProviderError::Cancelled,
                            retryable: false,
                        };
                    }
                }
                if let Some(arguments) = call.function.as_ref().and_then(|f| f.arguments.as_ref())
                    && !arguments.is_empty()
                {
                    tool_argument_bytes = match tool_argument_bytes.checked_add(arguments.len()) {
                        Some(total) if total <= MAX_TOOL_ARGUMENT_BYTES => total,
                        _ => {
                            return ConsumeResult::Failed {
                                error: ProviderError::Protocol(format!(
                                    "tool arguments exceed {MAX_TOOL_ARGUMENT_BYTES} byte limit"
                                )),
                                retryable: false,
                            };
                        }
                    };
                    slot.arguments.push_str(arguments);
                    if trace::enabled() {
                        let mut fields = serde_json::Map::new();
                        fields.insert("index".into(), json!(index));
                        fields.insert("arguments_len".into(), json!(arguments.len()));
                        trace::log("tool_arguments_delta", fields);
                    }
                    if events
                        .send(ProviderEvent::ToolArgumentsDelta {
                            index: index as u32,
                            chunk: arguments.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return ConsumeResult::Failed {
                            error: ProviderError::Cancelled,
                            retryable: false,
                        };
                    }
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
            if let Some(input_tokens) = usage_value.prompt_tokens {
                usage.input_tokens = input_tokens;
            }
            if let Some(output_tokens) = usage_value.completion_tokens {
                usage.output_tokens = output_tokens;
            }
            if let Some(cache_read_tokens) = usage_value
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens)
            {
                usage.cache_read_tokens = cache_read_tokens;
            }
        }
    }

    if !saw_done {
        // EOF 前未见 [DONE]。部分 provider 不发 [DONE] 但发 finish_reason——
        // 视为正常结束；两者都无 = 流被截断（服务器中断/网络丢包）。
        if finish_reason.is_some() {
            // 正常结束（宽松 provider 路径）。
        } else if received_any {
            // 已发出语义事件：不可重试（避免事件重复/乱序）；分类为
            // StreamInterrupted——agent 据此自动续写/重生成（§4.3）。
            return ConsumeResult::Failed {
                error: ProviderError::StreamInterrupted(
                    "stream ended before [DONE] and no finish_reason".into(),
                ),
                retryable: false,
            };
        } else {
            // 未收到任何语义内容：可安全重试（provider 内部退避重试）。
            return ConsumeResult::Failed {
                error: ProviderError::Connection("empty stream".into()),
                retryable: true,
            };
        }
    }

    // §7.3：tool arguments 不完整（未闭合 JSON）时返回协议错误，不能猜补 JSON。
    let tool_calls = (|| -> Result<Vec<crate::provider::ToolCall>, ProviderError> {
        let mut complete = Vec::new();
        let mut provider_ids = std::collections::HashSet::new();
        for call in pending {
            let touched =
                call.provider_id.is_some() || !call.name.is_empty() || !call.arguments.is_empty();
            if !touched {
                continue;
            }
            if call.name.is_empty() {
                return Err(ProviderError::Protocol(
                    "tool call is missing function name".into(),
                ));
            }
            let arguments = call.arguments;
            if arguments.trim().is_empty() {
                return Err(ProviderError::Protocol(
                    "tool call is missing JSON arguments".into(),
                ));
            }
            serde_json::from_str::<serde_json::Value>(&arguments)
                .map_err(|_| ProviderError::Protocol("incomplete tool arguments".into()))?;
            let provider_id = call.provider_id.ok_or_else(|| {
                ProviderError::Protocol(format!("tool call {} is missing id", call.name))
            })?;
            if !provider_ids.insert(provider_id.clone()) {
                return Err(ProviderError::Protocol(format!(
                    "duplicate tool call id: {provider_id}"
                )));
            }
            complete.push(crate::provider::ToolCall {
                call_id: ToolCallId::new_v7(),
                provider_id,
                name: call.name,
                arguments,
            });
        }
        Ok(complete)
    })()
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

    /// §bug 修复：`choices` / `delta.tool_calls` 为 `null` 时视为空数组
    ///（此前 serde 报 `invalid type: null, expected a sequence`，整轮 run
    /// 失败——真实 OpenAI 兼容服务在 usage chunk / 推理帧会发 null）。
    #[test]
    fn sse_chunk_tolerates_null_sequence_fields() {
        // choices: null（usage-only chunk）。
        let chunk: SseChunk =
            serde_json::from_str(r#"{"choices": null, "usage": {"prompt_tokens": 1}}"#)
                .expect("choices null 应解析为空数组");
        assert!(chunk.choices.is_empty());
        assert_eq!(chunk.usage.as_ref().unwrap().prompt_tokens, Some(1));

        // delta.tool_calls: null（推理/工具帧）。
        let chunk: SseChunk = serde_json::from_str(
            r#"{"choices": [{"index": 0, "delta": {"content": "hi", "tool_calls": null}}]}"#,
        )
        .expect("delta.tool_calls null 应解析为空数组");
        assert_eq!(chunk.choices.len(), 1);
        assert!(chunk.choices[0].delta.tool_calls.is_empty());
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));

        // 字段完全缺失仍默认空（原有行为不回退）。
        let chunk: SseChunk = serde_json::from_str(r#"{}"#).expect("缺字段默认");
        assert!(chunk.choices.is_empty());
    }

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
    fn empty_streamed_tool_fields_are_treated_as_omitted() {
        // Ark emits id/name in the first tool-call chunk and `""` for those
        // fields in following argument chunks.
        assert_eq!(non_empty_delta(Some("call_123")), Some("call_123"));
        assert_eq!(non_empty_delta(Some("")), None);
        assert_eq!(non_empty_delta(None), None);
    }

    #[test]
    fn raw_sse_frame_guard_handles_split_delimiters_and_rejects_oversize() {
        let mut guard = SseFrameGuard::default();
        guard.inspect_with_limit(b"data: 1\r\n\r", 16).unwrap();
        guard.inspect_with_limit(b"\n", 16).unwrap();
        assert_eq!(
            guard.frame_bytes, 0,
            "split CRLF delimiter must reset budget"
        );
        guard.inspect_with_limit(b"data: 2\n\n", 16).unwrap();
        assert_eq!(guard.frame_bytes, 0, "LF delimiter must reset budget");

        let mut oversized = SseFrameGuard::default();
        assert!(
            oversized
                .inspect_with_limit(b"123456789012\n\n", 8)
                .is_err()
        );
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
                call_id: tpi_core::ids::ToolCallId::new_v7(),
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

    /// 协议 invariant：`type: object` 的 parameters 必须带 `properties`
    /// （无字段工具/第三方来源字段缺失时，兜底补空 `{}` 避免严格 validator 400）。
    #[test]
    fn object_parameters_always_carry_properties() {
        // 无字段工具：源 schema 只有 `{"type":"object"}`。
        let bare = serde_json::json!({ "type": "object" });
        let normalized = normalize_parameters(&bare);
        assert_eq!(normalized["properties"], serde_json::json!({}), "{normalized}");
        assert_eq!(normalized["type"], "object");

        // 带字段的 schema：properties 保留，不覆盖。
        let with_fields = serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } }
        });
        let normalized = normalize_parameters(&with_fields);
        assert_eq!(
            normalized["properties"]["path"]["type"],
            "string",
            "已有 properties 不得被覆盖"
        );

        // 非 object 参数原样透传。
        let array_schema = serde_json::json!({ "type": "array" });
        assert_eq!(normalize_parameters(&array_schema), array_schema);
    }

    /// build_body 兜底：即便 ToolDef 的 parameters 缺 `properties`，发出的
    /// 请求体里仍带 `properties: {}`。
    #[test]
    fn build_body_normalizes_bare_object_schema() {
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
                name: "inspect".into(),
                description: "inspect".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            max_output_tokens: None,
            reasoning: None,
            context_window: None,
        };
        let body = client.build_body(&request);
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"],
            serde_json::json!({}),
            "{body}"
        );
    }

    #[test]
    fn reasoning_uses_openai_compatible_effort_field() {
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
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: Some("max".into()),
            context_window: None,
        };
        let body = client.build_body(&request);
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("reasoning").is_none(), "{body}");
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
        assert!(body.get("reasoning_effort").is_none(), "{body}");
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

/// §7.3：指数退避，测试环境 jitter=1.0（确定性）。每次尝试持续翻倍、
/// 不设上限（§用户诉求：10 次尝试每次等待翻倍）。
#[test]
fn backoff_doubles_per_attempt() {
    let first = backoff_delay(0, None);
    let second = backoff_delay(1, None);
    let third = backoff_delay(2, None);
    let fourth = backoff_delay(3, None);
    let ninth = backoff_delay(8, None);
    assert_eq!(first, INITIAL_BACKOFF, "attempt 0 = 初始退避");
    assert_eq!(second, INITIAL_BACKOFF * 2, "attempt 1 翻倍");
    assert_eq!(third, INITIAL_BACKOFF * 4, "attempt 2 再翻倍");
    assert_eq!(fourth, INITIAL_BACKOFF * 8, "attempt 3 继续翻倍（无上限）");
    assert_eq!(ninth, INITIAL_BACKOFF * 256, "attempt 8 峰值 128s");
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

#[test]
fn retry_after_cannot_exceed_total_wait_budget() {
    let mut budget = RetryBudget::new(4, Duration::from_secs(360));
    assert!(budget.begin_attempt());
    // 61s 在 360s 预算内：允许（区别于旧的 60s 预算）。
    assert!(budget.permits_retry_after(Duration::from_secs(61)));
    budget.record_wait(Duration::from_secs(330));
    // 累计恰好等于预算：允许；再多 1ms 即拒绝。
    assert!(budget.permits_retry_after(Duration::from_secs(30)));
    assert!(!budget.permits_retry_after(Duration::from_secs(31)));
}

/// §40：确定性非法请求（HTTP 400/422：tool schema 校验失败等）必须归为
/// `InvalidRequest`——同类 JSON 重发必然再 400，agent 不得对其自动重试/重启。
#[test]
fn invalid_request_classified_as_terminal_fatal() {
    let err = classify_error("invalid request (400): schema error".into(), 0);
    assert!(
        matches!(err, ProviderError::InvalidRequest(_)),
        "400 must be InvalidRequest: {err:?}"
    );
    let err = classify_error("invalid request (422): bad body".into(), 2);
    assert!(
        matches!(err, ProviderError::InvalidRequest(_)),
        "422 must be InvalidRequest: {err:?}"
    );
}

#[test]
fn exhausted_retryable_status_keeps_error_class() {
    assert!(matches!(
        retryable_status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, 3),
        ProviderError::RateLimited(_)
    ));
    assert!(matches!(
        retryable_status_error(reqwest::StatusCode::BAD_GATEWAY, 3),
        ProviderError::Http(_)
    ));
}

#[test]
fn defensive_4xx_retry_is_limited_to_one_replay() {
    assert!(defensive_retry_allowed(0));
    assert!(!defensive_retry_allowed(1));
    assert!(!defensive_retry_allowed(3));
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
