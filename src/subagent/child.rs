//! P8-04：in-process read-only child——复用进程内 agent run 执行只读调查。
//!
//! - concurrency 1 / depth 1（每次 run_investigation 一个 child，不递归）；
//! - 只读 registry（read/list/search/glob 白名单，P8-03 类型保证）；
//! - 独立 child session（`config.sessions_root/child` 下，隔离 parent）；
//! - parent cancel 传播（同一 CancellationToken 传给 child run）；
//! - parent 只接收 structured report（summary + evidence）。

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::agent::{self, RunInput};
use crate::ids::RunId;
use crate::provider::Provider;
use crate::session::store::SessionLog;
use crate::subagent::{SubagentProvider, SubagentReport, SubagentRequest};
use crate::tool::registry::{ToolRegistry, read_only_registry};
use crate::workspace::ActiveWorkspace;

/// in-process child provider（P8-04）。
///
/// `make_provider`：child 用独立 provider 实例（parent 与 child 不共享 &mut
/// provider——parent 阻塞等 child，child 独占自己的实例；concurrency 1 无竞争）。
pub struct InProcessChildProvider<P, F> {
    make_provider: F,
    config: Arc<crate::config::Config>,
    workspace: ActiveWorkspace,
    _provider: std::marker::PhantomData<P>,
}

impl<P, F> InProcessChildProvider<P, F> {
    pub fn new(
        make_provider: F,
        config: Arc<crate::config::Config>,
        workspace: ActiveWorkspace,
    ) -> Self {
        Self {
            make_provider,
            config,
            workspace,
            _provider: std::marker::PhantomData,
        }
    }
}

/// 从 assistant 文本提取证据引用（`@artifact/...`；只读调查的产出）。
fn evidence_from(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find("@artifact/") {
        let start = idx;
        let tail = &rest[start..];
        let end = tail
            .char_indices()
            .find(|(i, c)| *i > "@artifact/".len() && c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        out.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    out
}

#[async_trait::async_trait]
impl<P: Provider + Send, F: FnMut() -> P + Send> SubagentProvider for InProcessChildProvider<P, F> {
    async fn run_investigation(
        &mut self,
        request: SubagentRequest,
        cancel: CancellationToken,
    ) -> Result<SubagentReport, String> {
        // depth 1：child 不再产生 child（不递归；P9-05 延后）。
        // concurrency 1：调用方每次一个 child（P8-05 信号量之前）。

        // 独立 child provider 实例（不与 parent 争用 &mut provider）。
        let mut child_provider = (self.make_provider)();

        // 独立 child session（隔离目录，不与 parent session 混）。
        let child_root = self.config.sessions_root.join("child");
        std::fs::create_dir_all(&child_root)
            .map_err(|e| format!("创建 child sessions 目录失败: {e}"))?;
        let mut child_session = SessionLog::create_with_id(
            &child_root,
            self.config.workspace_root.as_std_path(),
            RunId::new_v7(),
            request.child_session,
        )
        .map_err(|e| format!("创建 child session 失败: {e}"))?;

        // 只读 registry（白名单之外的工具不存在 → 不可调用）。
        let registry: Arc<Mutex<ToolRegistry>> =
            Arc::new(Mutex::new(read_only_registry(&request.capabilities)));

        // child 的 LiveEvent 不投影 TUI（P8-06 之前 drain 丢弃）。
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel::<crate::agent::LiveEvent>(64);
        let drain = tokio::spawn(async move { while ui_rx.recv().await.is_some() {} });

        let outcome = agent::run(
            &mut child_provider,
            &mut child_session,
            &self.config,
            RunInput {
                history: &[],
                user_message: request.instruction.clone(),
                ui: ui_tx,
                cancel,
                interactive: false,
                force_compaction: false,
                workspace: Some(self.workspace.clone()),
                registry,
            },
        )
        .await;
        drain.abort();

        let outcome = outcome.map_err(|e| format!("child run 失败: {e}"))?;
        // parent cancel 传播：child run 以 Cancelled 正常结束（§11.5 取消是正常
        // 终态，非错误）——此时 parent 不需要 report，按契约返回 Err。
        if outcome.reason == crate::session::CompletionReason::Cancelled {
            return Err("child run cancelled（parent cancel 传播）".into());
        }
        Ok(SubagentReport {
            child_session: request.child_session,
            summary: outcome.assistant_text.clone(),
            evidence: evidence_from(&outcome.assistant_text),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::provider::{Provider, ProviderResponse};
    use crate::subagent::ReadOnlyCapability;
    use crate::workspace::LocalWorkspace;

    /// 脚本化 fake provider（实现 Provider；测试专用）。
    struct ChildFake;
    impl Provider for ChildFake {
        fn model_name(&self) -> &str {
            "child-fake"
        }
        async fn stream(
            &mut self,
            _request: crate::provider::ModelRequest,
            events: tokio::sync::mpsc::Sender<crate::provider::ProviderEvent>,
            cancel: CancellationToken,
        ) -> Result<ProviderResponse, crate::provider::ProviderError> {
            if cancel.is_cancelled() {
                return Err(crate::provider::ProviderError::Cancelled);
            }
            let response = crate::provider::ProviderResponse {
                finish_reason: crate::provider::FinishReason::Stop,
                tool_calls: Vec::new(),
                usage: crate::session::Usage::default(),
            };
            let _ = events
                .send(crate::provider::ProviderEvent::TextDelta(
                    "child 调查完成，见 @artifact/child-1".into(),
                ))
                .await;
            Ok(response)
        }
    }

    fn test_config() -> Arc<Config> {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut config = crate::config::test_config(&root);
        config.workspace_root = root.clone();
        config.sessions_root = root.join("sessions").into();
        config.artifacts_root = root.join("artifacts").into();
        Arc::new(config)
    }

    /// conformance：child 执行返回 structured report（child_session 匹配 + summary）。
    #[tokio::test]
    async fn child_returns_structured_report() {
        let mut provider = InProcessChildProvider::new(
            || ChildFake,
            test_config(),
            ActiveWorkspace::local(LocalWorkspace::new(
                camino::Utf8PathBuf::from("C:/proj"),
                false,
            )),
        );
        let child = crate::ids::SessionId::new_v7();
        let report = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "调查 src/main.rs".into(),
                    child_session: child,
                    capabilities: vec![
                        ReadOnlyCapability::Read,
                        ReadOnlyCapability::List,
                        ReadOnlyCapability::Search,
                    ],
                },
                CancellationToken::new(),
            )
            .await
            .expect("child 执行成功");
        assert_eq!(report.child_session, child, "child session 匹配");
        assert!(
            report.summary.contains("child 调查完成"),
            "summary 携带 child 输出: {}",
            report.summary
        );
        assert!(
            report.evidence.contains(&"@artifact/child-1".to_string()),
            "evidence 提取 artifact 引用: {:?}",
            report.evidence
        );
    }

    /// parent cancel 传播：cancel 后 child 立即 cancelled（不继续执行）。
    #[tokio::test]
    async fn parent_cancel_propagates_to_child() {
        let mut provider = InProcessChildProvider::new(
            || ChildFake,
            test_config(),
            ActiveWorkspace::local(LocalWorkspace::new(
                camino::Utf8PathBuf::from("C:/proj"),
                false,
            )),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = provider
            .run_investigation(
                SubagentRequest {
                    instruction: "调查".into(),
                    child_session: crate::ids::SessionId::new_v7(),
                    capabilities: vec![ReadOnlyCapability::Read],
                },
                cancel,
            )
            .await;
        assert!(
            result.is_err(),
            "parent cancel 后 child 必须失败（不继续执行）"
        );
    }

    /// 只读 registry：白名单之外的工具不可用（无写/进程工具）。
    #[test]
    fn read_only_registry_excludes_writers() {
        let caps = vec![ReadOnlyCapability::Read, ReadOnlyCapability::Search];
        let registry = read_only_registry(&caps);
        assert!(registry.get("read").is_some(), "read 可用");
        assert!(registry.get("search").is_some(), "search 可用");
        assert!(registry.get("bash").is_none(), "bash 不可用");
        assert!(registry.get("edit").is_none(), "edit 不可用");
        assert!(registry.get("write").is_none(), "write 不可用");
        assert!(registry.get("web_fetch").is_none(), "网络工具不可用");
    }
}
