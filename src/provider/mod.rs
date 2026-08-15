//! 模型 provider 接口与共享协议类型。
//!
//! Adapter 的职责是吸收差异：SSE 分帧、tool argument 增量拼接、`reasoning_content`、
//! finish reason 和 usage。Agent machine 不允许出现 provider-specific JSON 字段（§7.1）。
//!
//! 第一版只有 OpenAI-compatible 一个实现 + 测试用 fake provider；
//! 第二个真实 adapter 出现时再从已稳定的输入/事件类型提取边界（§7.1）。

use crate::session::Usage;
use tokio_util::sync::CancellationToken;

pub mod catalog;
pub mod openai_compat;
pub mod request_replay;
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

/// P7 下沉：`ChatMessage` / `ToolCall` / `ToolDef` 是纯数据，定义在 domain 层
/// （`crate::message`）；此处 re-export 保持对外契约不变（`provider::ChatMessage`
/// 等仍可用，golden parity 由测试保证）。
pub use crate::message::{ChatMessage, ToolCall, ToolDef};

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

/// P5-01：normalized model stream handle。
///
/// Provider 的流式输出统一为 [`ProviderEvent`] 序列（迭代句柄），调用方
/// **不传 UI channel**——UI 是 agent 的 LiveEvent（P1-03 解耦）；provider 只
/// 产生语义事件。OpenAI adapter（openai_compat）与 fake 都产出本句柄。
pub struct ProviderStream {
    rx: tokio::sync::mpsc::Receiver<ProviderEvent>,
}

impl ProviderStream {
    pub fn new(rx: tokio::sync::mpsc::Receiver<ProviderEvent>) -> Self {
        Self { rx }
    }

    /// 取下一个事件（None = 流结束）。
    pub async fn next(&mut self) -> Option<ProviderEvent> {
        self.rx.recv().await
    }
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
    /// 传输层失败（连接被拒/超时/未收到任何语义内容前断流）。provider 内部
    /// 已重试 `MAX_ATTEMPTS` 次仍失败；agent 层再做 turn 级重启（§4.3 第四
    /// 阶段，无内容重启不重复任何东西）。
    #[error("connection failed: {0}")]
    Connection(String),
    /// 连接已建立、流中途截断且已收到部分语义内容（§4.3）。不可重发原请求
    /// （会重复已到达的文本/工具调用）；agent 据此决定自动续写/重生成。
    #[error("stream interrupted: {0}")]
    StreamInterrupted(String),
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
