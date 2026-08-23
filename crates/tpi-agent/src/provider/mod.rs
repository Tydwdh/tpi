//! 模型 provider 接口与共享协议类型。
//!
//! Adapter 的职责是吸收差异：SSE 分帧、tool argument 增量拼接、`reasoning_content`、
//! finish reason 和 usage。Agent machine 不允许出现 provider-specific JSON 字段（§7.1）。
//!
//! 第一版只有 OpenAI-compatible 一个实现 + 测试用 fake provider；
//! 第二个真实 adapter 出现时再从已稳定的输入/事件类型提取边界（§7.1）。

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tpi_session::Usage;

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
    /// 请求（发送/读流）重试：attempt = 本次逻辑请求内的第几次重试
    /// （1-based），backoff_ms 为将要等待的退避时长。
    /// 由 provider 内部重试循环发出（§用户诉求：重试对用户可见，避免"瞬间报错"错觉）。
    Retrying {
        attempt: u32,
        backoff_ms: u64,
    },
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
/// （`tpi_core::message`）；此处 re-export 保持对外契约不变（`provider::ChatMessage`
/// 等仍可用，golden parity 由测试保证）。
pub use tpi_core::message::{ChatMessage, ToolCall, ToolDef};

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
    /// 确定性非法请求（HTTP 400：tool schema 校验失败 / 请求体格式错误等）。
    ///
    /// 请求本身是**确定性非法**的——重发相同 JSON 必然得到相同 400，因此完全
    /// 不值得重试/重放/自动重启（§用户诉求：禁止对 400 反复 regenerate）。
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("cancelled")]
    Cancelled,
}

/// 模型请求的流式通道容量。
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// 一个逻辑 provider 请求共享的重试预算。
///
/// 预算由 provider adapter 唯一拥有：上层 AgentLoop 不得为同一请求再做
/// 自动重试，避免 HTTP/client/provider/agent 的乘法放大。`ModelRequest` 在
/// budget 的整个生命周期内保持不变，因此每次 attempt 都是同一请求身份的重放。
#[derive(Debug)]
pub struct RetryBudget {
    max_attempts: u32,
    max_elapsed: Duration,
    started_at: Instant,
    attempts_started: u32,
    waited: Duration,
}

impl RetryBudget {
    pub fn new(max_attempts: u32, max_elapsed: Duration) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            max_elapsed,
            started_at: Instant::now(),
            attempts_started: 0,
            waited: Duration::ZERO,
        }
    }

    /// 登记一次实际网络请求；超过 attempt 或 wall-clock 预算时拒绝启动。
    pub fn begin_attempt(&mut self) -> bool {
        if self.attempts_started >= self.max_attempts || self.elapsed() >= self.max_elapsed {
            return false;
        }
        self.attempts_started += 1;
        true
    }

    /// 本轮失败后是否还能等待 `delay` 再重试。
    pub fn permits_retry_after(&self, delay: Duration) -> bool {
        self.attempts_started < self.max_attempts
            && self
                .elapsed()
                // 正常路径 elapsed 已包含真实 sleep；`waited` 让预算在可控
                // 时间/测试环境下仍能精确记账，取较大者避免重复计算。
                .max(self.waited)
                .checked_add(delay)
                .is_some_and(|total| total <= self.max_elapsed)
    }

    pub fn record_wait(&mut self, delay: Duration) {
        self.waited = self.waited.saturating_add(delay);
    }

    pub fn attempts_started(&self) -> u32 {
        self.attempts_started
    }

    pub fn waited(&self) -> Duration {
        self.waited
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

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

#[cfg(test)]
mod retry_budget_tests {
    use super::RetryBudget;
    use std::time::Duration;

    #[test]
    fn shared_budget_prevents_nested_attempt_amplification() {
        let mut budget = RetryBudget::new(2, Duration::from_secs(30));
        assert!(budget.begin_attempt());
        assert!(budget.permits_retry_after(Duration::from_secs(1)));
        budget.record_wait(Duration::from_secs(1));
        assert!(budget.begin_attempt());
        assert!(!budget.permits_retry_after(Duration::ZERO));
        assert!(!budget.begin_attempt());
    }
}
