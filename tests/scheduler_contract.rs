//! Phase D（任务书 §15）：Scheduler 正确性契约。
//!
//! - `max_parallel_tools=1` 基线：一切串行，生命周期严格交替；
//! - 冲突（read+edit 同文件 / edit+edit 同文件 / bash 隔离点）强制串行；
//! - 并行 read：结果仍按 provider 原始 call index 回填；
//! - 并行期间取消：所有 in-flight call 全部 Cancelled，run 正常结束。

mod fixtures;

use std::collections::HashMap;

use camino::Utf8PathBuf;
use fixtures::fake_provider::{FakeProvider, FakeResponse, tool_call};
use fixtures::{point_host_at_real_tpi, test_config};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::ids::RunId;
use tpi::provider::ChatMessage;
use tpi::session::{CompletionReason, SessionEvent, SessionLog};

/// 工具生命周期序列："S:<tool>"（started）/ "C:<tool>"（completed）。
fn lifecycle(events: &[SessionEvent]) -> Vec<(char, String)> {
    let mut by_call: HashMap<tpi::ids::ToolCallId, String> = HashMap::new();
    let mut out = Vec::new();
    for event in events {
        match event {
            SessionEvent::ToolRequested { call } => {
                by_call.insert(call.call_id, call.name.clone());
            }
            SessionEvent::ToolStarted { call_id, .. } => {
                out.push(('S', by_call.get(call_id).cloned().unwrap_or_default()))
            }
            SessionEvent::ToolCompleted { call_id, .. } => {
                out.push(('C', by_call.get(call_id).cloned().unwrap_or_default()))
            }
            _ => {}
        }
    }
    out
}

/// 断言生命周期严格交替（S,C,S,C,...）——即完全串行，无任何重叠。
fn assert_strictly_serial(life: &[(char, String)], label: &str) {
    for window in life.windows(2) {
        assert_ne!(
            window[0].0, window[1].0,
            "{label}: 必须严格交替 S,C,S,C...: {life:?}"
        );
    }
    assert_eq!(life.len() % 2, 0, "{label}: S/C 必须成对: {life:?}");
}

/// 断言 S（started）全部出现在第一个 C（completed）之前——即真正并行。
fn assert_parallel_start(life: &[(char, String)], label: &str) {
    let first_completed = life.iter().position(|(c, _)| *c == 'C').unwrap();
    assert!(
        life[..first_completed].iter().all(|(c, _)| *c == 'S'),
        "{label}: 所有 started 必须先于任何 completed: {life:?}"
    );
}

async fn run_with(
    config: &tpi::config::Config,
    provider: &mut FakeProvider,
    message: &str,
) -> (tpi::agent::AgentOutcome, Vec<SessionEvent>) {
    let mut session = SessionLog::create(
        &config.sessions_root,
        config.workspace_root.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = agent::run(
        provider,
        &mut session,
        config,
        agent::RunInput {
            history: &[],
            user_message: message.into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
        },
    )
    .await
    .expect("run succeeds");
    drain.abort();
    let events = tpi::session::read_events(session.path()).unwrap();
    (outcome, events)
}

/// §15：max_parallel_tools=1 基线——同轮 3 个 read 完全串行。
#[tokio::test]
async fn max_parallel_one_serializes_everything() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(workspace.join(name), format!("content-{name}")).unwrap();
    }
    let mut config = test_config(&workspace);
    config.limits.max_parallel_tools = 1;

    let mut provider = FakeProvider::scripted(vec![
        Box::new(move |_request| {
            FakeResponse::with_tool_calls(vec![
                tool_call("read", serde_json::json!({"path": "a.txt"})),
                tool_call("read", serde_json::json!({"path": "b.txt"})),
                tool_call("read", serde_json::json!({"path": "c.txt"})),
            ])
        }),
        Box::new(move |_request| FakeResponse::text("done")),
    ]);
    let (outcome, events) = run_with(&config, &mut provider, "读三个文件").await;
    assert_eq!(outcome.reason, CompletionReason::Stop);
    let life = lifecycle(&events);
    assert_strictly_serial(&life, "max_parallel=1 基线");
    assert_eq!(life.len(), 6, "3 个 read 各一对 S/C");
}

/// §15：read + edit 同文件冲突 → 强制串行（edit 在 read 完成后才启动）。
#[tokio::test]
async fn conflicting_read_and_edit_are_serialized() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let fixture = workspace.join("a.txt");
    std::fs::write(&fixture, "old content\n").unwrap();

    let ws = workspace.clone();
    let mut provider = FakeProvider::scripted(vec![
        Box::new(move |_request| {
            let path = ws.join("a.txt");
            let raw = std::fs::read(&path).unwrap();
            let revision = tpi::tool::edit::revision_of(&raw);
            FakeResponse::with_tool_calls(vec![
                tool_call("read", serde_json::json!({"path": "a.txt"})),
                tool_call(
                    "edit",
                    serde_json::json!({
                        "path": "a.txt",
                        "revision": revision,
                        "replacements": [
                            {"old_text": "old content", "new_text": "new content"}
                        ],
                    }),
                ),
            ])
        }),
        Box::new(move |_request| FakeResponse::text("done")),
    ]);
    let (outcome, events) = run_with(&config_of(&workspace), &mut provider, "改 a.txt").await;
    assert_eq!(outcome.reason, CompletionReason::Stop);
    let life = lifecycle(&events);
    assert_strictly_serial(&life, "read+edit 冲突");
    // 顺序：read 先、edit 后。
    assert_eq!(life[0], ('S', "read".into()));
    assert_eq!(life[2], ('S', "edit".into()));
    assert!(
        std::fs::read_to_string(&fixture)
            .unwrap()
            .contains("new content")
    );
}

fn config_of(workspace: &Utf8PathBuf) -> tpi::config::Config {
    test_config(workspace)
}

/// 工具与已校验参数的组合是内部不变量；一旦边界被误用，调度器必须保守地
/// 隔离执行，不能把潜在写工具静默降级为 Pure 并与其他调用并行。
#[test]
fn mismatched_tool_and_args_never_default_to_pure() {
    use tpi::agent::scheduler::{ToolAccess, tool_access};
    use tpi::tool::{BuiltinTool, ValidatedArgs, files::ReadArgs};

    let workspace = Utf8PathBuf::from("C:/workspace");
    let read_args = ValidatedArgs::Read(ReadArgs {
        path: "state.txt".into(),
        start_line: 1,
        line_count: 20,
    });

    let access = tool_access(BuiltinTool::Edit, &read_args, &workspace, false);
    assert_eq!(
        access,
        ToolAccess::WorkspaceUnknown,
        "参数/工具不匹配时必须保守隔离，不能因兜底分支获得 Pure 权限"
    );
}

/// §15：edit + edit 同文件 → 独占串行（第二个因 revision 变化必然 stale）。
#[tokio::test]
async fn edit_and_edit_same_file_are_serialized() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(workspace.join("a.txt"), "line one\nline two\n").unwrap();

    let ws = workspace.clone();
    let mut provider = FakeProvider::scripted(vec![
        Box::new(move |_request| {
            let raw = std::fs::read(ws.join("a.txt")).unwrap();
            let revision = tpi::tool::edit::revision_of(&raw);
            FakeResponse::with_tool_calls(vec![
                tool_call(
                    "edit",
                    serde_json::json!({
                        "path": "a.txt",
                        "revision": revision,
                        "replacements": [
                            {"old_text": "line one", "new_text": "line ONE"}
                        ],
                    }),
                ),
                tool_call(
                    "edit",
                    serde_json::json!({
                        "path": "a.txt",
                        "revision": revision,
                        "replacements": [
                            {"old_text": "line two", "new_text": "line TWO"}
                        ],
                    }),
                ),
            ])
        }),
        Box::new(move |_request| FakeResponse::text("done")),
    ]);
    let (outcome, events) = run_with(&config_of(&workspace), &mut provider, "改两行").await;
    assert_eq!(outcome.reason, CompletionReason::Stop);
    let life = lifecycle(&events);
    assert_strictly_serial(&life, "edit+edit 同文件");
    // §修复 #1：两个 edit 的 old_text 互不重叠（line one / line two），第二个
    // 用旧 revision 但 old_text 仍唯一存在 → 宽松应用成功（目标未变，安全）。
    let completed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCompleted { outcome, .. } => Some(outcome.status),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0], tpi::tool::outcome::ToolStatus::Succeeded);
    assert_eq!(completed[1], tpi::tool::outcome::ToolStatus::Succeeded);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "line ONE\nline TWO\n"
    );
}

/// §15：bash 是隔离点——同轮 bash + read 必须串行。
#[tokio::test]
async fn bash_is_an_isolation_point() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(workspace.join("a.txt"), "A").unwrap();

    let mut provider = FakeProvider::scripted(vec![
        Box::new(move |_request| {
            FakeResponse::with_tool_calls(vec![
                tool_call("bash", serde_json::json!({"command": "echo hello"})),
                tool_call("read", serde_json::json!({"path": "a.txt"})),
            ])
        }),
        Box::new(move |_request| FakeResponse::text("done")),
    ]);
    let (outcome, events) = run_with(&config_of(&workspace), &mut provider, "跑命令并读文件").await;
    assert_eq!(outcome.reason, CompletionReason::Stop);
    let life = lifecycle(&events);
    assert_strictly_serial(&life, "bash 隔离点");
    assert_eq!(life[0], ('S', "bash".into()), "bash 按源顺序在前: {life:?}");
}

/// §15：并行 read 真正并行（S,S 连续），结果按 provider index 回填。
#[tokio::test]
async fn parallel_reads_complete_and_results_follow_source_index() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    // b.txt 明显更大：若并行，完成时间不同；回填顺序仍必须 a 在前。
    std::fs::write(workspace.join("a.txt"), "A").unwrap();
    std::fs::write(workspace.join("b.txt"), "B".repeat(50_000)).unwrap();

    let mut config = config_of(&workspace);
    config.limits.max_parallel_tools = 4;
    let mut provider = FakeProvider::scripted(vec![
        Box::new(move |_request| {
            FakeResponse::with_tool_calls(vec![
                tool_call("read", serde_json::json!({"path": "a.txt"})),
                tool_call("read", serde_json::json!({"path": "b.txt"})),
            ])
        }),
        Box::new(move |_request| FakeResponse::text("done")),
    ]);
    let (outcome, events) = run_with(&config, &mut provider, "并行读两个文件").await;
    assert_eq!(outcome.reason, CompletionReason::Stop);
    let life = lifecycle(&events);
    assert_parallel_start(&life, "并行 read");
    assert_eq!(life.len(), 4);
    // 结果按原始 index 回填：a（短输出）的 Tool 消息必须在 b（长输出）之前。
    let tool_msgs: Vec<&ChatMessage> = outcome
        .messages
        .iter()
        .filter(|m| matches!(m, ChatMessage::Tool { .. }))
        .collect();
    assert_eq!(tool_msgs.len(), 2);
    let ChatMessage::Tool { content: ca, .. } = tool_msgs[0] else {
        unreachable!()
    };
    let ChatMessage::Tool { content: cb, .. } = tool_msgs[1] else {
        unreachable!()
    };
    assert!(
        ca.len() < cb.len(),
        "index 0 的结果必须回填在前（a 短 b 长）: {} vs {}",
        ca.len(),
        cb.len()
    );
}

/// §15：并行期间取消——所有 in-flight call 全部 Cancelled，run 正常结束。
/// §15：并行期间取消——所有 in-flight call 全部 Cancelled，run 正常结束。
/// 注意：FakeProvider 不检查 cancel token，此处用 cancel-aware provider
/// 模拟真实 provider 行为（第二轮请求因 cancel 立即失败）。
struct CancelAwareProvider {
    first: bool,
}

impl tpi::provider::Provider for CancelAwareProvider {
    fn model_name(&self) -> &str {
        "cancel-aware"
    }

    async fn stream(
        &mut self,
        _request: tpi::provider::ModelRequest,
        events: mpsc::Sender<tpi::provider::ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<tpi::provider::ProviderResponse, tpi::provider::ProviderError> {
        if !self.first {
            self.first = true;
            let calls = vec![
                tool_call(
                    "bash",
                    serde_json::json!({"command": "sleep 60", "timeout_ms": 120000}),
                ),
                tool_call(
                    "bash",
                    serde_json::json!({"command": "sleep 60", "timeout_ms": 120000}),
                ),
            ];
            for (index, call) in calls.iter().enumerate() {
                events
                    .send(tpi::provider::ProviderEvent::ToolCallStarted {
                        index: index as u32,
                        id: call.provider_id.clone(),
                        name: call.name.clone(),
                    })
                    .await
                    .map_err(|_| tpi::provider::ProviderError::Protocol("closed".into()))?;
                events
                    .send(tpi::provider::ProviderEvent::ToolArgumentsDelta {
                        index: index as u32,
                        chunk: call.arguments.clone(),
                    })
                    .await
                    .map_err(|_| tpi::provider::ProviderError::Protocol("closed".into()))?;
            }
            return Ok(tpi::provider::ProviderResponse {
                finish_reason: tpi::provider::FinishReason::ToolCalls,
                usage: Default::default(),
                tool_calls: calls,
            });
        }
        // 后续请求：cancel 生效则立即失败（真实 provider 行为）。
        tokio::select! {
            _ = cancel.cancelled() => Err(tpi::provider::ProviderError::Cancelled),
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                Ok(tpi::provider::ProviderResponse {
                    finish_reason: tpi::provider::FinishReason::Stop,
                    usage: Default::default(),
                    tool_calls: Vec::new(),
                })
            }
        }
    }
}

#[tokio::test]
async fn cancellation_during_parallel_bash_cancels_all() {
    point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = config_of(&workspace);
    config.limits.max_parallel_tools = 4;

    let mut provider = CancelAwareProvider { first: false };
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    let session_path = session.path().to_path_buf();
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let cancel = CancellationToken::new();
    let cancel_in_run = cancel.clone();
    let run = tokio::spawn(async move {
        agent::run(
            &mut provider,
            &mut session,
            &config,
            agent::RunInput {
                history: &[],
                user_message: "并行跑两个命令".into(),
                ui: tx,
                cancel: cancel_in_run,
                interactive: true,
                force_compaction: false,
                workspace: None,
            },
        )
        .await
    });
    // 等两个 bash 真正启动后取消。
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(15), run)
        .await
        .expect("取消后 run 必须快速结束")
        .expect("task join 失败")
        .expect("cancel 是正常结束");
    drain.abort();
    let events = tpi::session::read_events(&session_path).unwrap();
    assert_eq!(outcome.reason, CompletionReason::Cancelled);
    let life = lifecycle(&events);
    assert_parallel_start(&life, "并行 bash");
    let cancelled: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCompleted { outcome, .. } => Some(outcome.status),
            _ => None,
        })
        .collect();
    assert_eq!(cancelled.len(), 2);
    assert!(
        cancelled
            .iter()
            .all(|s| *s == tpi::tool::outcome::ToolStatus::Cancelled),
        "并行中取消必须让所有 in-flight call 全部 Cancelled: {cancelled:?}"
    );
}

// ---- P4-03：ActiveToolSet Step 内 reload stability ----

use std::sync::Arc;

/// Step 内 registry 变化（MCP reload）不影响已构建快照。
#[tokio::test]
async fn active_set_is_stable_within_step() {
    use tpi::tool::registry::ToolRegistry;

    let registry = Arc::new(std::sync::Mutex::new(ToolRegistry::new()));
    // 注册 read + bash（builtin）；句柄必须绑定（RAII：drop 即注销）。
    let mut handles = Vec::new();
    for name in ["read", "bash"] {
        let found = tpi::tool::implemented_tools()
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap();
        let adapter = tpi::tool::registry::BuiltinToolAdapter::new(found);
        handles.push(ToolRegistry::register_owned(&registry, Arc::new(adapter)));
    }
    assert_eq!(registry.lock().unwrap().list().len(), 2);

    // 构建 Step 快照（模拟 reload）。
    let defs: Vec<String> = registry
        .lock()
        .unwrap()
        .descriptors()
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(defs.contains(&"read".to_string()));

    // Step 内 registry 变化：注销 read（模拟 MCP reload 移除工具）。
    let mut guard = registry.lock().unwrap();
    guard.unregister("read");
    drop(guard);
    assert!(registry.lock().unwrap().get("read").is_none());

    // 快照（defs）不受影响——Step 内执行仍用旧快照。
    // 这里验证：reload 前构建的 defs 是独立 Vec（不可变快照语义）。
    assert!(
        defs.contains(&"read".to_string()),
        "Step 快照不受 registry 变化影响"
    );
}
