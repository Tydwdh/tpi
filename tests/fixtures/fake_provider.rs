//! 录制式 fake provider（M0/M1 交付项，§21；对应文档 §4.2 的 tests/fixtures）。
//!
//! 由固定脚本驱动 Agent 状态机，不使用真实网络。
//! 脚本闭包在每次请求时执行，因此可以读取当前文件状态计算真实 revision。

use std::collections::VecDeque;

use tpi::ids::ToolCallId;
use tpi::provider::{
    FinishReason, ModelRequest, Provider, ProviderError, ProviderEvent, ProviderResponse, ToolCall,
};
use tpi::session::Usage;

/// 一次预设响应。
#[derive(Debug, Clone)]
pub struct FakeResponse {
    pub text: String,
    /// 非 None 时按条发送多个 TextDelta（P0-2 死锁回归：事件数超过 channel 容量）。
    pub deltas: Option<Vec<String>>,
    pub tool_calls: Vec<ToolCall>,
    pub finish: FinishReason,
    /// 本次请求返回的 usage（默认 0；usage 累加测试用）。
    pub usage: Usage,
}

impl FakeResponse {
    pub fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            deltas: None,
            tool_calls: Vec::new(),
            finish: FinishReason::Stop,
            usage: Usage::default(),
        }
    }

    /// 逐条发送多个文本增量（每块一条 ProviderEvent，可超过 channel 容量）。
    pub fn text_deltas(deltas: Vec<String>) -> Self {
        Self {
            text: String::new(),
            deltas: Some(deltas),
            tool_calls: Vec::new(),
            finish: FinishReason::Stop,
            usage: Usage::default(),
        }
    }

    pub fn with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            text: String::new(),
            deltas: None,
            tool_calls,
            finish: FinishReason::ToolCalls,
            usage: Usage::default(),
        }
    }

    /// 带 usage 的响应（usage 累加测试用）。
    pub fn with_usage(mut self, input: u64, output: u64) -> Self {
        self.usage = Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
        };
        self
    }

    /// 带文本 + 工具调用的响应（text+tool 轮，P0-3）。
    pub fn with_text(mut self, text: &str) -> Self {
        self.text = text.to_string();
        self
    }
}

/// 构造一个工具调用（TPI 内部 call id 自动分配）。
pub fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new_v7(),
        provider_id: format!("call-{name}"),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

/// 脚本闭包：按请求返回响应（可访问当前状态，如文件 revision）。
type ScriptStep = Box<dyn for<'a> FnMut(&'a ModelRequest) -> FakeResponse + Send>;

/// fake provider：按脚本顺序响应请求。
pub struct FakeProvider {
    script: VecDeque<ScriptStep>,
    /// 循环模式：同一个闭包反复调用（不 pop）。
    loop_mode: bool,
    pub request_count: usize,
    pub requests: Vec<ModelRequest>,
    pub events_log: Vec<ProviderEvent>,
}

impl FakeProvider {
    pub fn new(responses: Vec<FakeResponse>) -> Self {
        Self {
            script: responses
                .into_iter()
                .map(|response| {
                    let boxed: Box<dyn for<'a> FnMut(&'a ModelRequest) -> FakeResponse + Send> =
                        Box::new(move |_request| response.clone());
                    boxed
                })
                .collect(),
            loop_mode: false,
            request_count: 0,
            requests: Vec::new(),
            events_log: Vec::new(),
        }
    }

    /// 脚本化：每次请求 pop 一个闭包（可访问当前状态，如文件 revision）。
    pub fn scripted(script: Vec<ScriptStep>) -> Self {
        Self {
            script: script.into(),
            loop_mode: false,
            request_count: 0,
            requests: Vec::new(),
            events_log: Vec::new(),
        }
    }

    /// 循环模式：同一个状态机闭包反复响应所有请求。
    pub fn scripted_loop(script: ScriptStep) -> Self {
        Self {
            script: VecDeque::from([script]),
            loop_mode: true,
            request_count: 0,
            requests: Vec::new(),
            events_log: Vec::new(),
        }
    }

    pub fn model_name(&self) -> &'static str {
        "fake-model"
    }
}

impl Provider for FakeProvider {
    fn model_name(&self) -> &'static str {
        "fake-model"
    }

    async fn stream(
        &mut self,
        request: ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        self.request_count += 1;
        self.requests.push(request.clone());
        let mut owned_script;
        let response = if self.loop_mode {
            let script = self
                .script
                .front_mut()
                .expect("fake provider loop script present");
            script(&request)
        } else {
            owned_script = self.script.pop_front().expect(
                "fake provider script exhausted: more model requests than scripted responses",
            );
            owned_script(&request)
        };
        if let Some(deltas) = &response.deltas {
            for delta in deltas {
                self.events_log
                    .push(ProviderEvent::TextDelta(delta.clone()));
                events
                    .send(ProviderEvent::TextDelta(delta.clone()))
                    .await
                    .map_err(|_| ProviderError::Protocol("channel closed".into()))?;
            }
        } else if !response.text.is_empty() {
            self.events_log
                .push(ProviderEvent::TextDelta(response.text.clone()));
            let _ = events.send(ProviderEvent::TextDelta(response.text)).await;
        }
        Ok(ProviderResponse {
            finish_reason: response.finish,
            usage: response.usage,
            tool_calls: response.tool_calls,
        })
    }
}
