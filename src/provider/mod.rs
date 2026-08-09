//! Provider 层（文档 §7）。
//!
//! Adapter 的职责是吸收差异：SSE 分帧、tool argument 增量拼接、`reasoning_content`、
//! finish reason 和 usage。Agent machine 不允许出现 provider-specific JSON 字段（§7.1）。
//!
//! 第一版只有 OpenAI-compatible 一个实现 + 测试用 fake provider；
//! 第二个真实 adapter 出现时再从已稳定的输入/事件类型提取边界（§7.1）。

use crate::ids::ToolCallId;
use crate::session::Usage;
use tokio_util::sync::CancellationToken;

pub mod openai_compat;
pub mod trace;

/// Provider 流归一化后的事件（ephemeral，不逐 token 写盘，见 §4.3/§7.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallStarted {
        index: u32,
        id: String,
        name: String,
    },
    ToolArgumentsDelta {
        index: u32,
        chunk: String,
    },
    Usage(Usage),
}

/// Provider 流的结束原因（§6.2：明确的完成/失败分类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// 正常结束且无未决 tool call。
    Stop,
    /// 模型选择调用工具（tool call 在 [`ProviderResponse::tool_calls`] 中返回）。
    ToolCalls,
    /// 输出达到长度限制。
    Length,
    /// 内容被过滤器拦截。
    ContentFilter,
    /// 协议错误或 provider 内部错误。
    Error,
}

/// 模型发出的工具调用请求（tool argument 增量已在 adapter 内拼接完成）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// TPI 内部分配的 call id（§14.2 恢复关联用）。
    pub call_id: ToolCallId,
    /// provider 原始 tool call id（回填 tool result 时必须原样返回）。
    pub provider_id: String,
    pub name: String,
    /// 完整 JSON 参数字符串；schema 校验发生在调度前（§8.2 `PreparedToolCall`）。
    pub arguments: String,
}

/// 发给模型的消息（OpenAI-compatible 最小形态；provider 差异在 adapter 内吸收）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMessage {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    /// 工具结果回填。
    Tool {
        tool_call_id: String,
        name: String,
        content: String,
    },
}

/// 工具定义（schema 由参数类型生成，§5.2 schemars）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// 一次模型请求。
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    pub max_output_tokens: Option<u32>,
    /// 透传 reasoning level（§18.1 `[model.primary] reasoning`）。
    pub reasoning: Option<String>,
    /// 请求级预算：超过该 token 数视为上下文溢出（§15 由 context builder 使用）。
    pub context_window: Option<u64>,
}

/// 一次请求的完整响应（流事件经 `mpsc::Sender<ProviderEvent>` 发送）。
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub finish_reason: FinishReason,
    pub usage: Usage,
    pub tool_calls: Vec<ToolCall>,
}

/// Provider 请求失败（§7.3 重试后仍失败，或流中途协议错误）。
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("connection failed: {0}")]
    Connection(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("cancelled")]
    Cancelled,
}

/// 模型请求的流式通道容量。
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Provider 抽象。
///
/// 真实实现是 [`OpenAiCompatClient`]；测试用 fake provider 也是真实消费者
/// （§20.1：fake provider → Agent loop 的 integration 测试是 M1 验收项），
/// 因此这里不是为假想 provider 预留的抽象层（§7.1）。
///
pub trait Provider: Send {
    /// 当前 primary model 名称（session 记录与 UI 展示，§3.2 不变量 12）。
    fn model_name(&self) -> &str;

    /// 发起一次流式请求。
    ///
    /// 文本/reasoning/tool argument 增量通过 `events` 发送；
    /// 返回的 [`ProviderResponse`] 携带 finish reason、usage 与组装完成的 tool calls。
    fn stream(
        &mut self,
        request: ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send;
}
