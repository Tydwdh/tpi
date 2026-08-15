//! P8-03：SubagentProvider 契约 + fake（P8 初始规格：只读、隔离上下文、并行调查）。
//!
//! 规格（用户决策 2026-08-14）：默认关闭；depth=1（不递归）；concurrency=1；
//! fresh child session；只读 capability allowlist；parent 只接收 structured
//! report；parent cancellation 必须传播；不允许共享 workspace 写入。
//!
//! 本模块定义契约 + fake；P8-04 in-process child 在其上实现。

pub mod child;
pub mod parallel;

use crate::ids::{SessionId, SpanId, TraceId};

/// 子代理只读能力白名单（P8 初始规格：只读调查）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyCapability {
    Read,
    List,
    Search,
    Glob,
}

/// O8（P8-09）：发起子代理的 parent trace 上下文（link 双向引用的 parent 侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentTraceContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
}

/// 子代理请求（depth=1、concurrency=1、只读）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRequest {
    /// 调查指令。
    pub instruction: String,
    /// fresh child session id（调用方创建）。
    pub child_session: SessionId,
    /// 只读能力白名单。
    pub capabilities: Vec<ReadOnlyCapability>,
    /// O8（P8-09）：发起方（parent）trace 上下文；None = remote_boundary
    /// （独立测试/诊断路径，无 parent 可链）。
    pub parent: Option<ParentTraceContext>,
}

/// 子代理 structured report（parent 只接收此）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentReport {
    pub child_session: SessionId,
    pub summary: String,
    /// 引用的文件/证据（只读调查产出）。
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
    /// 执行一次只读调查；返回 structured report。
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
                    capabilities: vec![ReadOnlyCapability::Read, ReadOnlyCapability::Search],
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

    /// 契约：请求携带只读白名单（depth=1 语义：不递归）。
    #[test]
    fn request_carries_readonly_capabilities() {
        let req = SubagentRequest {
            instruction: "调查".into(),
            child_session: SessionId::new_v7(),
            capabilities: vec![ReadOnlyCapability::Read],
            parent: None,
        };
        assert_eq!(req.capabilities, vec![ReadOnlyCapability::Read]);
        // 类型层面保证只读：ReadOnlyCapability 没有写能力变体（读/列表/搜索/glob
        // 全部无副作用）——编译期即拒绝写工具进入白名单。
        for cap in req.capabilities {
            assert!(
                matches!(
                    cap,
                    ReadOnlyCapability::Read
                        | ReadOnlyCapability::List
                        | ReadOnlyCapability::Search
                        | ReadOnlyCapability::Glob
                ),
                "白名单只含只读能力"
            );
        }
    }
}
