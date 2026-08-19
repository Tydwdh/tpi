//! O0 trace integrity：异步 span 父子关系测试（audit Medium-7 / P0-09）。
//!
//! 问题：`agent.run` 曾用 `let _enter = span.enter()`（同步 thread-local
//! enter/exit scope）覆盖整个 async 函数体。future yield 后 guard 仍持有，
//! 同一 executor 线程上的其他任务会被错误归入该 span，造成并发 run 的
//! parent/child 关系交叉。
//!
//! 验收（P0-09）：并发两个带不同 run_id 的 future，subscriber 记录的
//! event ancestry 不能交叉；任意时刻 `agent.run` 的 enter 深度 ≤ 1。
//! 官方 contract：<https://docs.rs/tracing/latest/tracing/span/struct.EnteredSpan.html>
//! 明确警告跨 await 持有 EnteredSpan 会导致不可预测的父子关系。

mod fixtures;

use std::sync::{Arc, Mutex};

use tracing::span;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::{LookupSpan, Registry};

use tpi::agent;
use tpi::ids::RunId;
use tpi::provider::{FinishReason, Provider, ProviderError, ProviderEvent, ProviderResponse};
use tpi::session::{SessionLog, Usage};

/// 每次 poll 都让出（yield_now）的 fake provider：保证并发 run 在单线程
/// runtime 上真正交替执行，从而暴露"同步 enter guard 跨 await 泄漏"。
struct YieldingProvider;

impl Provider for YieldingProvider {
    fn model_name(&self) -> &str {
        "yield"
    }

    async fn stream(
        &mut self,
        _request: tpi::provider::ModelRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ProviderResponse, ProviderError> {
        tokio::task::yield_now().await;
        events
            .send(ProviderEvent::TextDelta("answer".into()))
            .await
            .map_err(|_| ProviderError::Protocol("closed".into()))?;
        tokio::task::yield_now().await;
        Ok(ProviderResponse {
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            tool_calls: Vec::new(),
        })
    }
}

/// 捕获 span 的 enter/exit 序列，统计 `agent.run` 的最大并发 enter 深度。
#[derive(Default)]
struct Capture {
    stack: Vec<String>,
    max_run_depth: usize,
}

/// registry-based capture layer：on_new_span 把 span 名存入 extensions，
/// on_enter/on_exit 维护 enter 栈（enter/exit 严格嵌套配对）。
struct CaptureLayer(Arc<Mutex<Capture>>);

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let name = attrs.metadata().name().to_string();
        if let Some(s) = ctx.span(id) {
            s.extensions_mut().insert(name);
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        let mut cap = self.0.lock().unwrap();
        let name = ctx
            .span(id)
            .and_then(|s| s.extensions().get::<String>().cloned())
            .unwrap_or_default();
        cap.stack.push(name.clone());
        if name == "agent.run" {
            let depth = cap.stack.iter().filter(|n| *n == "agent.run").count();
            cap.max_run_depth = cap.max_run_depth.max(depth);
        }
    }

    fn on_exit(&self, _id: &span::Id, _ctx: Context<'_, S>) {
        let mut cap = self.0.lock().unwrap();
        if !cap.stack.is_empty() {
            cap.stack.pop();
        }
    }
}

/// 并发两个 run：任意时刻 `agent.run` 的 enter 深度必须 ≤ 1
/// （修复前同步 enter guard 跨 await 泄漏 → 深度 ≥ 2，红灯）。
#[test]
fn concurrent_runs_do_not_nest_agent_run_spans() {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let layer = CaptureLayer(capture.clone());
    let subscriber = Registry::default().with(layer);

    tracing::subscriber::with_default(subscriber, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (a, b) = tokio::join!(run_fake("run-a"), run_fake("run-b"));
            a.expect("run a ok");
            b.expect("run b ok");
        });
    });

    let cap = capture.lock().unwrap();
    assert!(
        cap.max_run_depth <= 1,
        "agent.run span 并发 enter 深度 = {}（应 ≤ 1）：同步 enter guard 跨 await 泄漏，\
         同一线程其他 run 被错误归入当前 span（Medium-7）。应改用 Future::instrument",
        cap.max_run_depth
    );
}

/// 构造并执行一次完整 run（fake provider + tempdir session）。
async fn run_fake(label: &str) -> Result<agent::AgentOutcome, String> {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = fixtures::test_config(&workspace);
    let mut provider = YieldingProvider;
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: label.into(),
            ui: tx,
            cancel: tokio_util::sync::CancellationToken::new(),
            interactive: false,
            force_compaction: false,
            workspace: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::process::managed::ProcessRegistry::new(),
            )),
            terminals: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::terminal::TerminalRegistry::default(),
            )),

            agents: std::sync::Arc::new(std::sync::Mutex::new(
                tpi_agent::agent::manager::AgentManager::new(),
            )),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    drain.abort();
    Ok(outcome)
}
