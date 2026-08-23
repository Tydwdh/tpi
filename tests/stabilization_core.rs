//! `TPI_STABILIZATION_TASK` Phase 1：Core Conversation Model。
//!
//! - P0-1：interactive 层每轮 `history.extend(outcome.messages)`，而
//!   `AgentOutcome.messages` 是 agent 从调用方 history 复制构造的**完整 context**
//!   → 历史每轮整体重复（token 膨胀、模型看到重复消息）。
//!   修复契约：outcome.messages = 完整 context，由 `Conversation` 整体接纳，
//!   app 不再分别持有 `SessionLog` 与 history。
//!
//! 本文件同时承载 Phase 1 后续的 replay / runtime-resume 等价测试。

mod fixtures;

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use fixtures::fake_provider::{FakeProvider, FakeResponse};
use fixtures::test_config;
use tokio_util::sync::CancellationToken;
use tpi::app;
use tpi::ids::RunId;
use tpi::provider::ChatMessage;
use tpi::session::SessionLog;

/// P0-1：3 轮 run（U1→A1, U2→A2, U3→A3）后，调用方 history 必须精确等于
/// U1 A1 U2 A2 U3 A3，不得重复。
#[tokio::test]
async fn p0_1_history_never_duplicates_across_turns() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    let mut provider = FakeProvider::new(vec![
        FakeResponse::text("A1"),
        FakeResponse::text("A2"),
        FakeResponse::text("A3"),
    ]);
    let current_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let mut conversation = tpi::session::conversation::Conversation::new();
    conversation
        .ensure_started(&config.sessions_root, &workspace)
        .expect("create session");

    for i in 1..=3 {
        let outcome = {
            let (session, history) = conversation.parts_for_run().unwrap();
            app::run_prompt_once(
                &mut provider,
                session,
                &config,
                history,
                format!("U{i}"),
                current_cancel.clone(),
                std::sync::Arc::new(std::sync::Mutex::new(
                    tpi::tool::registry::builtin_registry(),
                )),
            )
            .await
            .expect("run should succeed")
        };
        conversation.accept_context(outcome.messages);
    }

    let expected: Vec<ChatMessage> = vec![
        ChatMessage::User("U1".into()),
        ChatMessage::Assistant {
            content: "A1".into(),
            tool_calls: Vec::new(),
        },
        ChatMessage::User("U2".into()),
        ChatMessage::Assistant {
            content: "A2".into(),
            tool_calls: Vec::new(),
        },
        ChatMessage::User("U3".into()),
        ChatMessage::Assistant {
            content: "A3".into(),
            tool_calls: Vec::new(),
        },
    ];
    assert_eq!(
        conversation.history(),
        expected,
        "history 必须精确为 U1A1U2A2U3A3，不得重复"
    );
}

/// 构造一个最小脚本 provider：request 1 返回 `tool_calls（可带文本），后续请求返回文本`。
/// 返回 (provider, request 引用计数)。
fn tool_loop_provider(_workspace: &Utf8PathBuf, first_text: &'static str) -> FakeProvider {
    FakeProvider::scripted(vec![
        // 1. 读取 probe.txt（纯 tool-call 轮或 text+tool 轮）。
        Box::new(move |_request| {
            FakeResponse::with_tool_calls(vec![tpi::provider::ToolCall {
                call_id: tpi::ids::ToolCallId::new_v7(),
                provider_id: "call-read-probe".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "probe.txt"}).to_string(),
            }])
            .with_text(first_text)
        }),
        // 2. 模型基于 tool result 给出最终回答。
        Box::new(move |_request| FakeResponse::text("读取完成")),
    ])
}

/// P0-2：assistant content 为空（纯 tool-call 轮）时，session 必须仍然持久化
/// assistant turn（含 `tool_calls）——resume` 重建出的消息序列必须合法：
/// User → `Assistant(tool_calls)` → Tool → Assistant。
#[tokio::test]
async fn p0_2_empty_text_tool_call_replays_legal_protocol() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(workspace.join("probe.txt"), "TPI_LIVE_CANARY_7F31").unwrap();
    let config = test_config(&workspace);
    let mut provider = tool_loop_provider(&workspace, "");
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = tokio::sync::mpsc::channel(128);

    let _outcome = tpi::agent::run(
        &mut provider,
        &mut session,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "读取 probe.txt".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
            resources: std::sync::Arc::new(tpi::resource::ResourceManager::new()),

            agents: std::sync::Arc::new(std::sync::Mutex::new(
                tpi_agent::agent::manager::AgentManager::new(),
            )),
        },
    )
    .await
    .expect("run succeeds");

    // 模拟进程重启：关闭后重新 open，重建 projection。
    let events = tpi::session::read_events(session.path()).expect("read events");
    let resumed = app::session_to_messages(&events);

    assert_eq!(
        resumed.len(),
        4,
        "重建序列必须为 User/Assistant(tool_calls)/Tool/Assistant: {resumed:?}"
    );
    let ChatMessage::Assistant {
        content,
        tool_calls,
    } = &resumed[1]
    else {
        panic!("resumed[1] 必须是 Assistant: {:?}", resumed[1]);
    };
    assert!(content.is_empty(), "纯 tool-call 轮 content 为空");
    assert_eq!(tool_calls.len(), 1, "assistant turn 必须携带 tool_calls");
    assert_eq!(tool_calls[0].name, "read");
    assert_eq!(tool_calls[0].provider_id, "call-read-probe");
    let ChatMessage::Tool { name, .. } = &resumed[2] else {
        panic!("resumed[2] 必须是 Tool: {:?}", resumed[2]);
    };
    assert_eq!(name, "read");
    let ChatMessage::Assistant { content, .. } = &resumed[3] else {
        panic!("resumed[3] 必须是 Assistant: {:?}", resumed[3]);
    };
    assert_eq!(content, "读取完成");
}

/// P0-3：runtime context 与 resume projection 必须语义等价
/// （role 顺序、assistant content、tool call `name/provider_id/args、tool` result）。
#[tokio::test]
async fn p0_3_runtime_projection_matches_resume_projection() {
    fixtures::point_host_at_real_tpi();
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(workspace.join("probe.txt"), "TPI_LIVE_CANARY_7F31").unwrap();
    let config = test_config(&workspace);
    let mut provider = tool_loop_provider(&workspace, "我先读取这个文件。");
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("create session");
    let (tx, _rx) = tokio::sync::mpsc::channel(128);

    let outcome = tpi::agent::run(
        &mut provider,
        &mut session,
        &config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "读取 probe.txt".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
            resources: std::sync::Arc::new(tpi::resource::ResourceManager::new()),

            agents: std::sync::Arc::new(std::sync::Mutex::new(
                tpi_agent::agent::manager::AgentManager::new(),
            )),
        },
    )
    .await
    .expect("run succeeds");

    let events = tpi::session::read_events(session.path()).expect("read events");
    let resumed = app::session_to_messages(&events);

    assert_eq!(
        outcome.messages, resumed,
        "runtime context 必须与 restart 后重建的 context 完全一致"
    );
    // 显式核验关键语义（即使未来字段增加也不漏检）。
    assert_eq!(resumed.len(), 4);
    assert_eq!(resumed[0], ChatMessage::User("读取 probe.txt".into()));
    let ChatMessage::Assistant {
        content,
        tool_calls,
    } = &resumed[1]
    else {
        panic!("resumed[1] 必须是 Assistant");
    };
    assert_eq!(
        content, "我先读取这个文件。",
        "text+tool 轮 content 必须保留"
    );
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].name, "read");
    let ChatMessage::Tool { tool_call_id, .. } = &resumed[2] else {
        panic!("resumed[2] 必须是 Tool: {:?}", resumed[2]);
    };
    assert_eq!(tool_call_id, "call-read-probe");
}

// ── §21 Session Replay Contract ────────────────────────────────────────────
// 统一流程：FakeProvider 跑真实 agent loop → 得到 runtime context；
// 关闭 SessionLog → 从 JSONL 重新 open → session_to_messages() 重建；
// 两者必须语义等价（任务书 §21 / §30 最后一条）。

struct ReplayHarness {
    config: tpi::config::Config,
    session: SessionLog,
}

impl ReplayHarness {
    fn new(workspace: &Utf8PathBuf) -> Self {
        let config = test_config(workspace);
        let session = SessionLog::create(
            &config.sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
        )
        .expect("create session");
        Self { config, session }
    }

    async fn run(&mut self, provider: &mut FakeProvider, user_message: &str) -> Vec<ChatMessage> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let outcome = tpi::agent::run(
            provider,
            &mut self.session,
            &self.config,
            tpi::agent::RunInput {
                history: &[],
                user_message: user_message.into(),
                ui: tx,
                cancel: CancellationToken::new(),
                interactive: true,
                force_compaction: false,
                workspace: None,
                registry: std::sync::Arc::new(std::sync::Mutex::new(
                    tpi::tool::registry::builtin_registry(),
                )),
                resources: std::sync::Arc::new(tpi::resource::ResourceManager::new()),

                agents: std::sync::Arc::new(std::sync::Mutex::new(
                    tpi_agent::agent::manager::AgentManager::new(),
                )),
            },
        )
        .await
        .expect("run succeeds");
        drain.abort();
        outcome.messages
    }

    /// 重新 open（模拟进程重启）并重建 projection。
    fn replay(&self) -> Vec<ChatMessage> {
        let events = tpi::session::read_events(self.session.path()).expect("read events");
        app::session_to_messages(&events)
    }
}

/// §21 场景 1-5 + write：runtime projection == resume projection。
#[tokio::test]
async fn replay_text_only_one_tool_multi_tool_text_plus_tool_failed_and_write() {
    fixtures::point_host_at_real_tpi();

    // 场景 1：纯文本。
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut h = ReplayHarness::new(&workspace);
        let mut provider = FakeProvider::new(vec![FakeResponse::text("hello")]);
        let runtime = h.run(&mut provider, "hi").await;
        let resumed = h.replay();
        assert_eq!(runtime, resumed, "text only replay");
        assert_eq!(resumed.len(), 2);
        assert_eq!(
            resumed[1],
            ChatMessage::Assistant {
                content: "hello".into(),
                tool_calls: Vec::new(),
            }
        );
    }

    // 场景 2：单工具调用。
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(workspace.join("probe.txt"), "c2").unwrap();
        let mut h = ReplayHarness::new(&workspace);
        let mut provider = tool_loop_provider(&workspace, "");
        let runtime = h.run(&mut provider, "读取 probe.txt").await;
        let resumed = h.replay();
        assert_eq!(runtime, resumed, "one tool call replay");
        assert_eq!(resumed.len(), 4);
    }

    // 场景 3：多工具调用（同轮两个 read）。
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(workspace.join("a.txt"), "A").unwrap();
        std::fs::write(workspace.join("b.txt"), "B").unwrap();
        let mut h = ReplayHarness::new(&workspace);
        let mut provider = FakeProvider::scripted(vec![
            Box::new(move |_request| {
                FakeResponse::with_tool_calls(vec![
                    tpi::provider::ToolCall {
                        call_id: tpi::ids::ToolCallId::new_v7(),
                        provider_id: "call-a".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "a.txt"}).to_string(),
                    },
                    tpi::provider::ToolCall {
                        call_id: tpi::ids::ToolCallId::new_v7(),
                        provider_id: "call-b".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "b.txt"}).to_string(),
                    },
                ])
            }),
            Box::new(move |_request| FakeResponse::text("两个都读了")),
        ]);
        let runtime = h.run(&mut provider, "读 a 和 b").await;
        let resumed = h.replay();
        assert_eq!(runtime, resumed, "multiple tool calls replay");
        // User + Assistant(2 calls) + Tool + Tool + Assistant
        assert_eq!(resumed.len(), 5);
        let ChatMessage::Assistant { tool_calls, .. } = &resumed[1] else {
            panic!("resumed[1] 必须是 Assistant");
        };
        assert_eq!(tool_calls.len(), 2, "同轮两个 tool call 必须都保留");
        assert_eq!(tool_calls[0].provider_id, "call-a");
        assert_eq!(tool_calls[1].provider_id, "call-b");
        let ChatMessage::Tool {
            tool_call_id: id0, ..
        } = &resumed[2]
        else {
            panic!("resumed[2] 必须是 Tool");
        };
        let ChatMessage::Tool {
            tool_call_id: id1, ..
        } = &resumed[3]
        else {
            panic!("resumed[3] 必须是 Tool");
        };
        assert_eq!(id0, "call-a");
        assert_eq!(id1, "call-b");
    }

    // 场景 4：text + tool call 同轮。
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(workspace.join("probe.txt"), "c4").unwrap();
        let mut h = ReplayHarness::new(&workspace);
        let mut provider = tool_loop_provider(&workspace, "我先读取。");
        let runtime = h.run(&mut provider, "读取 probe.txt").await;
        let resumed = h.replay();
        assert_eq!(runtime, resumed, "text + tool replay");
    }

    // 场景 5：failed tool（读不存在的文件）。
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut h = ReplayHarness::new(&workspace);
        let mut provider = FakeProvider::scripted(vec![
            Box::new(move |_request| {
                FakeResponse::with_tool_calls(vec![tpi::provider::ToolCall {
                    call_id: tpi::ids::ToolCallId::new_v7(),
                    provider_id: "call-missing".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "missing.txt"}).to_string(),
                }])
            }),
            Box::new(move |_request| FakeResponse::text("文件不存在，我会新建")),
        ]);
        let runtime = h.run(&mut provider, "读 missing.txt").await;
        let resumed = h.replay();
        assert_eq!(runtime, resumed, "failed tool replay");
        assert_eq!(resumed.len(), 4);
        let ChatMessage::Tool { name, content, .. } = &resumed[2] else {
            panic!("resumed[2] 必须是 Tool");
        };
        assert_eq!(name, "read");
        assert!(
            !content.is_empty(),
            "failed tool 的 observation 必须保留: {content}"
        );
    }

    // 场景 6：write 工具成功（副作用工具参与）。
    {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut h = ReplayHarness::new(&workspace);
        let mut provider = FakeProvider::scripted(vec![
            Box::new(move |_request| {
                FakeResponse::with_tool_calls(vec![tpi::provider::ToolCall {
                    call_id: tpi::ids::ToolCallId::new_v7(),
                    provider_id: "call-write".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({
                        "path": "new.txt",
                        "content": "hello new"
                    })
                    .to_string(),
                }])
            }),
            Box::new(move |_request| FakeResponse::text("已创建")),
        ]);
        let runtime = h.run(&mut provider, "创建 new.txt").await;
        let resumed = h.replay();
        assert_eq!(runtime, resumed, "write tool replay");
        assert_eq!(resumed.len(), 4);
        assert!(workspace.join("new.txt").exists(), "write 副作用已落盘");
    }
}

/// 取消 provider：发 2 个 delta 后以 Cancelled 结束（§11.5）。
struct CancelAfterDeltaProvider;

impl tpi::provider::Provider for CancelAfterDeltaProvider {
    fn model_name(&self) -> &'static str {
        "cancel-after-delta"
    }

    async fn stream(
        &mut self,
        _request: tpi::provider::ModelRequest,
        events: tokio::sync::mpsc::Sender<tpi::provider::ProviderEvent>,
        _cancel: CancellationToken,
    ) -> Result<tpi::provider::ProviderResponse, tpi::provider::ProviderError> {
        for i in 0..2 {
            events
                .send(tpi::provider::ProviderEvent::TextDelta(format!("t{i} ")))
                .await
                .map_err(|_| tpi::provider::ProviderError::Protocol("closed".into()))?;
        }
        Err(tpi::provider::ProviderError::Cancelled)
    }
}

/// §21 场景 7：cancelled run 的 runtime/replay 等价。
#[tokio::test]
async fn replay_cancelled_run_matches_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut h = ReplayHarness::new(&workspace);
    let mut provider = CancelAfterDeltaProvider;
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = tpi::agent::run(
        &mut provider,
        &mut h.session,
        &h.config,
        tpi::agent::RunInput {
            history: &[],
            user_message: "hi".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
            resources: std::sync::Arc::new(tpi::resource::ResourceManager::new()),

            agents: std::sync::Arc::new(std::sync::Mutex::new(
                tpi_agent::agent::manager::AgentManager::new(),
            )),
        },
    )
    .await
    .expect("cancel 是正常结束");
    drain.abort();
    assert_eq!(outcome.reason, tpi::session::CompletionReason::Cancelled);
    // 已到达的文本提交进 session（P1-1），runtime 与 replay 必须一致。
    let resumed = h.replay();
    assert_eq!(outcome.messages, resumed, "cancelled run replay");
    assert_eq!(resumed.len(), 2);
    let ChatMessage::Assistant { content, .. } = &resumed[1] else {
        panic!("resumed[1] 必须是 Assistant");
    };
    assert_eq!(content, "t0 t1 ", "已到达的 delta 必须保留");
}

/// §21 场景 9：崩溃留下的未完成尾部不破坏 replay 等价。
#[tokio::test]
async fn replay_survives_corrupt_trailing_line() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut h = ReplayHarness::new(&workspace);
    let mut provider = FakeProvider::new(vec![FakeResponse::text("hello")]);
    let runtime = h.run(&mut provider, "hi").await;

    // 模拟崩溃中断写入留下的半行垃圾。
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(h.session.path())
            .unwrap();
        write!(f, "{{not json at all").unwrap();
    }

    let resumed = h.replay();
    assert_eq!(runtime, resumed, "未完成尾部必须被跳过");
    assert_eq!(resumed.len(), 2);
}

/// §30 第 14 条：5~10 个连续 user turns，context 线性增长（无重复膨胀）。
/// P0-1 修复后：每轮 history 精确 +2 `条消息（U_i` + `A_i`）。
#[tokio::test]
async fn ten_turns_context_grows_linearly() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config = test_config(&workspace);
    // 10 轮，每轮回复 Ai。
    let mut provider = FakeProvider::new(
        (1..=10)
            .map(|i| FakeResponse::text(&format!("A{i}")))
            .collect(),
    );
    let current_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let mut conversation = tpi::session::conversation::Conversation::new();
    conversation
        .ensure_started(&config.sessions_root, &workspace)
        .expect("create session");

    for i in 1..=10 {
        let outcome = {
            let (session, history) = conversation.parts_for_run().unwrap();
            app::run_prompt_once(
                &mut provider,
                session,
                &config,
                history,
                format!("U{i}"),
                current_cancel.clone(),
                std::sync::Arc::new(std::sync::Mutex::new(
                    tpi::tool::registry::builtin_registry(),
                )),
            )
            .await
            .expect("run should succeed")
        };
        conversation.accept_context(outcome.messages);
        // 线性增长：第 i 轮后恰好 2i 条消息，无重复、无丢失。
        assert_eq!(
            conversation.history().len(),
            2 * i,
            "第 {i} 轮后 history 必须精确 2{i} 条: {:?}",
            conversation.history()
        );
    }
    // 最终序列精确：U1 A1 U2 A2 ... U10 A10。
    let history = conversation.history();
    for i in 1..=10 {
        let user = &history[2 * (i - 1)];
        let assistant = &history[2 * (i - 1) + 1];
        assert_eq!(*user, ChatMessage::User(format!("U{i}")));
        assert_eq!(
            *assistant,
            ChatMessage::Assistant {
                content: format!("A{i}"),
                tool_calls: Vec::new(),
            }
        );
    }
}
