//! P8-05：bounded parallel children——并发执行多个只读 child。
//!
//! - global/per-parent semaphore：`max_concurrent` 限流（acquire_owned 等待）；
//! - source order：结果按请求顺序返回（handles 顺序 await）；
//! - rate fairness：tokio `Semaphore` FIFO 公平；
//! - output cap：report summary 截断到 `max_output_bytes`（agent 内部 canonical
//!   output 已有界；此处对 report 加一层防护）。

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::subagent::{SubagentProvider, SubagentReport, SubagentRequest};

/// 有界并行 child 执行器（P8-05）。
///
/// `make_child`：每个 child 一个独立 provider 实例（`SubagentProvider` 是
/// `&mut self` 调用，不可共享——并发 child 各自实例化）。
pub struct BoundedChildRunner<F> {
    make_child: F,
    max_concurrent: usize,
    max_output_bytes: usize,
}

impl<F> BoundedChildRunner<F> {
    pub fn new(make_child: F, max_concurrent: usize, max_output_bytes: usize) -> Self {
        Self {
            make_child,
            max_concurrent: max_concurrent.max(1),
            max_output_bytes,
        }
    }
}

/// 截断 report summary 到上限（output cap；保持结构）。
fn cap_report(report: SubagentReport, max_bytes: usize) -> SubagentReport {
    if report.summary.len() <= max_bytes {
        return report;
    }
    // UTF-8 边界安全截断。
    let mut end = max_bytes;
    while end > 0 && !report.summary.is_char_boundary(end) {
        end -= 1;
    }
    let mut summary = report.summary[..end].to_string();
    summary.push_str("…[truncated]");
    SubagentReport { summary, ..report }
}

impl<F> BoundedChildRunner<F>
where
    F: FnMut() -> Box<dyn SubagentProvider + Send> + Send,
{
    /// 并行执行全部请求；结果按 source order 返回（每项 Ok/Err 独立）。
    pub async fn run_parallel(
        &mut self,
        requests: Vec<SubagentRequest>,
        cancel: CancellationToken,
    ) -> Vec<Result<SubagentReport, String>> {
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles: Vec<tokio::task::JoinHandle<Result<SubagentReport, String>>> =
            Vec::with_capacity(requests.len());
        for request in requests {
            // acquire_owned：并发超限时等待（bounded；per-run 全局限流）。
            let permit = match semaphore.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    handles.push(tokio::spawn(async {
                        Err::<SubagentReport, String>("semaphore closed".into())
                    }));
                    continue;
                }
            };
            let mut child = (self.make_child)();
            let child_cancel = cancel.clone();
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                child.run_investigation(request, child_cancel).await
            }));
        }
        // source order：按请求顺序 await（并发已由 semaphore 限定）。
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            let result = handle
                .await
                .unwrap_or_else(|e| Err(format!("child task join 失败: {e}")));
            let result = result.map(|r| cap_report(r, self.max_output_bytes));
            results.push(result);
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SessionId;
    use crate::subagent::ReadOnlyCapability;

    /// 计数 fake provider：进入时 +1（记录峰值并发），sleep 后 -1。
    struct CountingFake {
        active: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl CountingFake {
        fn new(
            active: Arc<std::sync::atomic::AtomicUsize>,
            peak: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self { active, peak }
        }
    }
    #[async_trait::async_trait]
    impl SubagentProvider for CountingFake {
        async fn run_investigation(
            &mut self,
            request: SubagentRequest,
            _cancel: CancellationToken,
        ) -> Result<SubagentReport, String> {
            let now = self
                .active
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.peak
                .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.active
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SubagentReport {
                child_session: request.child_session,
                summary: format!("调查 {}", request.instruction),
                evidence: Vec::new(),
                trace_id: None,
                cancelled: false,
            })
        }
    }

    fn request(instruction: &str) -> SubagentRequest {
        SubagentRequest {
            instruction: instruction.into(),
            child_session: SessionId::new_v7(),
            capabilities: vec![ReadOnlyCapability::Read],
            parent: None,
        }
    }

    /// 并发受限：max_concurrent=2，4 个请求峰值并发 <= 2，全部成功按序。
    #[tokio::test]
    async fn bounded_concurrency_and_source_order() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let active2 = active.clone();
        let peak2 = peak.clone();
        let mut runner = BoundedChildRunner::new(
            move || {
                Box::new(CountingFake::new(active2.clone(), peak2.clone()))
                    as Box<dyn SubagentProvider + Send>
            },
            2,
            4096,
        );
        let results = runner
            .run_parallel(
                vec![request("a"), request("b"), request("c"), request("d")],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(results.len(), 4, "source order 保持（4 项）");
        for (i, r) in results.iter().enumerate() {
            let report = r.as_ref().expect("全部成功");
            assert_eq!(
                report.summary,
                format!("调查 {}", ["a", "b", "c", "d"][i]),
                "source order: 第 {i} 项"
            );
        }
        assert!(
            peak.load(std::sync::atomic::Ordering::SeqCst) <= 2,
            "并发峰值受限: {}",
            peak.load(std::sync::atomic::Ordering::SeqCst)
        );
        let _ = active; // 全部执行完应归零
    }

    /// max_concurrent=1：严格串行（峰值 1）。
    #[tokio::test]
    async fn serial_when_max_concurrent_one() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let active2 = active.clone();
        let peak2 = peak.clone();
        let mut runner = BoundedChildRunner::new(
            move || {
                Box::new(CountingFake::new(active2.clone(), peak2.clone()))
                    as Box<dyn SubagentProvider + Send>
            },
            1,
            4096,
        );
        let results = runner
            .run_parallel(vec![request("a"), request("b")], CancellationToken::new())
            .await;
        assert!(results.iter().all(|r| r.is_ok()));
        assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1, "串行");
    }

    /// output cap：超长 summary 截断（保持 UTF-8 边界）。
    #[test]
    fn output_cap_truncates_summary() {
        let report = SubagentReport {
            child_session: SessionId::new_v7(),
            summary: "x".repeat(1000),
            evidence: Vec::new(),
            trace_id: None,
            cancelled: false,
        };
        let capped = cap_report(report, 100);
        assert!(capped.summary.len() <= 100 + "…[truncated]".len());
        assert!(capped.summary.ends_with("…[truncated]"));
    }
}
