//! 子代理 provider 契约、异步 agent 工具和 in-process worker。
//!
//! 每个 agent 都使用同一套工具目录；隔离的是 session/context，副作用由
//! 全局 effect scheduler、workspace CAS 和 agent graph 生命周期统一协调。

pub mod async_tool;
pub mod child;
pub mod tool;

use tpi_core::ids::{SessionId, SpanId, TraceId};

/// O8（P8-09）：发起子代理的 parent trace 上下文（link 双向引用的 parent 侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentTraceContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
}

/// 子代理请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRequest {
    /// Agent 的首条 user message。
    pub instruction: String,
    /// 独立 session id（调用方创建）。
    pub child_session: SessionId,
    /// O8（P8-09）：发起方（parent）trace 上下文；None = remote_boundary
    /// （独立测试/诊断路径，无 parent 可链）。
    pub parent: Option<ParentTraceContext>,
}

/// 子代理 structured report（parent 只接收此）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentReport {
    pub child_session: SessionId,
    pub summary: String,
    /// 引用的文件/验证证据。
    pub evidence: Vec<String>,
    /// O8（P8-09）：child run 的 trace id（parent 可据它查询完整 child trace；
    /// 与 parent 侧 link 记录双向引用）。
    pub trace_id: Option<TraceId>,
    /// O8（P8-09）：parent cancel 是否传播到了 child（Cancelled 终态转 Err 前
    /// 记录因果链）。
    pub cancelled: bool,
}

/// SubagentProvider 契约。
#[async_trait::async_trait]
pub trait SubagentProvider: Send {
    /// 执行一次 agent；返回 structured report。
    async fn run_investigation(
        &mut self,
        request: SubagentRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<SubagentReport, String>;
}

/// Fake：立即返回报告（conformance 测试用；不执行真实子代理）。
pub struct FakeSubagentProvider;

#[async_trait::async_trait]
impl SubagentProvider for FakeSubagentProvider {
    async fn run_investigation(
        &mut self,
        request: SubagentRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<SubagentReport, String> {
        Ok(SubagentReport {
            child_session: request.child_session,
            summary: format!("调查完成: {}", request.instruction),
            evidence: vec!["fake-evidence".into()],
            trace_id: None,
            cancelled: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// conformance：fake provider 返回 structured report（child session 匹配）。
    #[tokio::test]
    async fn fake_provider_returns_report() {
        let mut provider = FakeSubagentProvider;
        let child = SessionId::new_v7();
        let report = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "检查 src/main.rs".into(),
                    child_session: child,
                    parent: None,
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("fake 不失败");
        assert_eq!(report.child_session, child, "child session 匹配");
        assert!(!report.summary.is_empty());
        assert!(!report.evidence.is_empty(), "structured report 有证据");
    }

    /// 契约：请求携带独立 session 和可选 parent trace。
    #[test]
    fn request_carries_session_identity() {
        let req = SubagentRequest {
            instruction: "调查".into(),
            child_session: SessionId::new_v7(),
            parent: None,
        };
        assert!(!req.child_session.to_string().is_empty());
    }
}
