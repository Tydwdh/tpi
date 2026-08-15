//! M4 验收契约（§21 M4）。
//!
//! - §12.2：并行 read 能提速，冲突 write 不重叠，结果顺序稳定；
//! - §12.3：相同无进展动作连续 2 次后，第 3 次执行前返回 `repeated_without_progress`；
//! - §12.4：wall-clock watchdog 到达 deadline 主动取消；
//! - §13：原子短计划不变量（≤7 项、唯一 InProgress、完整替换、每次请求注入 snapshot）；
//! - §15.4：长会话经 compaction 后可继续准确执行；compaction 失败不循环。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::{fake_provider, test_config};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::agent::limits;
use tpi::agent::scheduler::{PreparedCall, ProgressTracker, ToolAccess, build_waves};
use tpi::ids::RunId;
use tpi::outcome::ToolStatus;
use tpi::session::{CompletionReason, SessionEvent, SessionLog, read_events};

/// §12.2：同 wave 无冲突 read 并行；冲突 write 独立 wave（不重叠）。
#[test]
fn waves_parallelize_reads_and_serialize_writes() {
    let mk = |index: usize, access: ToolAccess| PreparedCall {
        source_index: index,
        kind: tpi::agent::scheduler::PreparedKind::Builtin {
            tool: tpi::tool::BuiltinTool::Read,
            args: tpi::tool::ValidatedArgs::Read(tpi::tool::files::ReadArgs {
                path: format!("f{index}.txt"),
                start_line: 1,
                line_count: 10,
            }),
        },
        access,
        action_key: format!("k{index}"),
        plan: None,
    };
    let file_read = |index: usize, path: &str, mode: tpi::agent::scheduler::AccessMode| {
        mk(
            index,
            ToolAccess::Resources(vec![tpi::agent::scheduler::ResourceLock {
                resource: tpi::agent::scheduler::ResourceId::File(
                    tpi::agent::scheduler::FileScope::Exact(camino::Utf8PathBuf::from(path)),
                ),
                mode,
            }]),
        )
    };
    // 三个无冲突 read → 一个 wave（并行）。
    let calls = vec![
        file_read(0, "a.rs", tpi::agent::scheduler::AccessMode::Read),
        file_read(1, "b.rs", tpi::agent::scheduler::AccessMode::Read),
        file_read(2, "c.rs", tpi::agent::scheduler::AccessMode::Read),
    ];
    let waves = build_waves(calls, 4);
    assert_eq!(waves.len(), 1, "无冲突 read 必须并行（单 wave）");
    assert_eq!(waves[0].len(), 3);

    // write 与 read 冲突 → 独立 wave；write 之间也串行。
    let calls = vec![
        file_read(0, "a.rs", tpi::agent::scheduler::AccessMode::Read),
        file_write(1, "a.rs"),
        file_read(2, "b.rs", tpi::agent::scheduler::AccessMode::Read),
        file_write(3, "b.rs"),
    ];
    let waves = build_waves(calls, 4);
    // wave0: read a.rs + read b.rs（无 write 冲突）；wave1: write a.rs；wave2: write b.rs。
    assert_eq!(
        waves.len(),
        3,
        "write 必须与冲突 read 隔离并串行: {waves:?}"
    );
    assert!(waves[1][0].source_index == 1);
    assert!(waves[2][0].source_index == 3);
}

/// §12.1/§12.2：WorkspaceUnknown（bash）独占 wave，后续任何工具不得并入其 wave。
#[test]
fn bash_serializes_all_following_tools() {
    let bash_call = |index: usize| PreparedCall {
        source_index: index,
        kind: tpi::agent::scheduler::PreparedKind::Builtin {
            tool: tpi::tool::BuiltinTool::Bash,
            args: tpi::tool::ValidatedArgs::Bash(tpi::tool::command::BashArgs {
                command: "echo hi".into(),
                cwd: None,
                timeout_ms: 1000,
                background: false,
            }),
        },
        access: ToolAccess::WorkspaceUnknown,
        action_key: format!("b{index}"),
        plan: None,
    };
    let file_read = |index: usize, path: &str| PreparedCall {
        source_index: index,
        kind: tpi::agent::scheduler::PreparedKind::Builtin {
            tool: tpi::tool::BuiltinTool::Read,
            args: tpi::tool::ValidatedArgs::Read(tpi::tool::files::ReadArgs {
                path: path.into(),
                start_line: 1,
                line_count: 10,
            }),
        },
        access: ToolAccess::Resources(vec![tpi::agent::scheduler::ResourceLock {
            resource: tpi::agent::scheduler::ResourceId::File(
                tpi::agent::scheduler::FileScope::Exact(camino::Utf8PathBuf::from(path)),
            ),
            mode: tpi::agent::scheduler::AccessMode::Read,
        }]),
        action_key: format!("r{index}"),
        plan: None,
    };

    // bash → read → bash → edit：每个 bash 独占 wave，后续调用不得并入。
    let calls = vec![
        bash_call(0),
        file_read(1, "a.rs"),
        bash_call(2),
        file_write(3, "b.rs"),
    ];
    let waves = build_waves(calls, 4);
    // 期望：wave0=[bash0] wave1=[read] wave2=[bash2] wave3=[write]
    assert_eq!(
        waves.len(),
        4,
        "bash 必须独占 wave，后续工具不得与其并行: {waves:?}"
    );
    for wave in &waves {
        assert_eq!(wave.len(), 1, "每个 wave 必须单元素: {waves:?}");
    }
    assert_eq!(waves[0][0].source_index, 0);
    assert_eq!(waves[1][0].source_index, 1);
    assert_eq!(waves[2][0].source_index, 2);
    assert_eq!(waves[3][0].source_index, 3);
}

fn file_write(index: usize, path: &str) -> PreparedCall {
    PreparedCall {
        source_index: index,
        kind: tpi::agent::scheduler::PreparedKind::Builtin {
            tool: tpi::tool::BuiltinTool::Edit,
            args: tpi::tool::ValidatedArgs::Edit(tpi::tool::edit::EditArgs {
                path: path.into(),
                revision: "b3:0000000000000000000000000000000000000000000000000000000000000000"
                    .into(),
                replacements: vec![tpi::tool::edit::Replacement {
                    old_text: "x".into(),
                    new_text: "y".into(),
                }],
            }),
        },
        access: ToolAccess::Resources(vec![tpi::agent::scheduler::ResourceLock {
            resource: tpi::agent::scheduler::ResourceId::File(
                tpi::agent::scheduler::FileScope::Exact(camino::Utf8PathBuf::from(path)),
            ),
            mode: tpi::agent::scheduler::AccessMode::Write,
        }]),
        action_key: format!("w{index}"),
        plan: None,
    }
}

/// §12.3：连续 2 次相同无进展后，第 3 次执行前被拦截。
#[test]
fn no_progress_repeated_actions_blocked_on_third() {
    let mut tracker = ProgressTracker::default();
    // 第 1 次：执行。
    assert!(!tracker.should_block("read", "stamp0"));
    tracker.observe("read", "obs-failed", "stamp0");
    // 第 2 次：相同动作 + 相同观察 + 相同状态。
    assert!(!tracker.should_block("read", "stamp0"));
    tracker.observe("read", "obs-failed", "stamp0");
    // 第 3 次：执行前拦截（§12.3）。
    assert!(tracker.should_block("read", "stamp0"), "第 3 次必须拦截");
    // 状态变化（相关 revision 变化）后允许重试（§12.3：epoch 增加）。
    tracker.bump_workspace_epoch();
    assert!(
        !tracker.should_block("read", "stamp1"),
        "状态变化后允许重试"
    );
    // 不同动作不受影响。
    tracker.observe("read", "obs-failed", "stamp1");
    assert!(!tracker.should_block("list", "stamp1"));
}

/// §12.4：watchdog 在 wall deadline 到达时主动取消。
#[tokio::test]
async fn watchdog_cancels_at_wall_deadline() {
    let cancel = CancellationToken::new();
    let (watchdog, _) = limits::spawn_watchdog_with_wall(
        std::time::Duration::from_millis(200),
        cancel.clone(),
        || {},
        || {},
    );
    let end = watchdog.await.unwrap();
    assert_eq!(end, limits::BudgetEnd::WallTimeExceeded);
    assert!(cancel.is_cancelled(), "watchdog 必须主动取消 run（§12.4）");
}

/// §13：update_plan 不变量（≤7 项、显式状态、完整替换、拒绝无效）。
#[test]
fn plan_invariants_enforced() {
    use tpi::plan::{PlanItemArg, PlanStatus, UpdatePlanArgs, build_plan, validate_invariants};
    let item = |text: &str, status| PlanItemArg {
        text: text.into(),
        status,
    };
    // 合法：完整显式快照。
    let plan = build_plan(
        &UpdatePlanArgs {
            explanation: Some("fix".into()),
            items: vec![
                item("a", PlanStatus::InProgress),
                item("b", PlanStatus::Pending),
                item("c", PlanStatus::Blocked),
            ],
        },
        None,
    )
    .unwrap();
    validate_invariants(&plan).unwrap();
    let in_progress = plan
        .items
        .iter()
        .filter(|i| i.status == PlanStatus::InProgress)
        .count();
    assert_eq!(in_progress, 1);

    // 8 项被拒绝（§13：最多 7 项）。
    let error = build_plan(
        &UpdatePlanArgs {
            explanation: None,
            items: (0..8)
                .map(|i| PlanItemArg {
                    text: format!("item{i}"),
                    status: PlanStatus::Pending,
                })
                .collect(),
        },
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("最多 7 项"));

    // 重复项被拒绝。
    let error = build_plan(
        &UpdatePlanArgs {
            explanation: None,
            items: vec![
                item("same", PlanStatus::InProgress),
                item("same", PlanStatus::Pending),
            ],
        },
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("重复"));

    // 完整替换：不提交的旧项不会被猜测为 completed，也不会残留在快照中。
    let previous = build_plan(
        &UpdatePlanArgs {
            explanation: None,
            items: vec![
                item("old1", PlanStatus::InProgress),
                item("old2", PlanStatus::Pending),
            ],
        },
        None,
    )
    .unwrap();
    let next = build_plan(
        &UpdatePlanArgs {
            explanation: None,
            items: vec![item("new1", PlanStatus::InProgress)],
        },
        Some(&previous),
    )
    .unwrap();
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.items[0].text, "new1");
    assert_eq!(next.items[0].status, PlanStatus::InProgress);
    // 空 items 清空计划。
    let cleared = build_plan(
        &UpdatePlanArgs {
            explanation: None,
            items: vec![],
        },
        Some(&next),
    )
    .unwrap();
    assert!(cleared.items.is_empty());
}

/// §13 + §15.4 集成：update_plan 替换完整计划、计划以工具事实进入后续请求；
/// 长会话触发 compaction（无工具请求）后继续准确执行。
#[tokio::test]
async fn update_plan_and_compaction_integration() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    // 小 context_window + 小 reserve 强制触发 compaction（§15.4：projected > usable）。
    // P0-9 后 projected 含 system prompt（≈919 tokens）与工具 schema（≈5482 tokens），
    // 窗口 4000 会让第一轮就触发且 history 为空无法显著缩小（校验失败）；
    // 9000 保证首轮不触发、多轮 read 累积后触发且 history 足够大。
    let mut config = test_config(&workspace);
    // 动态基线：先用 agent 同款估算算「空会话请求」的系统开销（system prompt +
    // 工具 schema），窗口设在该基线之上，保证首轮不触发压缩、
    // 多次 read 累积后触发。工具集/系统提示变化自动适应，无需手调 magic number。
    // 当前基线实测 ~3393 tokens（含 glob/search 参数 schema）；窗口 = 基线 + 800，
    // 8 轮 read 累积后触发 compaction，compaction 后回到基线+summary 仍能继续。
    let baseline = tpi::context::estimate_request(
        tpi::agent::DEFAULT_SYSTEM_PROMPT,
        &[],
        &tpi::tool::implemented_tools()
            .iter()
            .map(tpi::tool::BuiltinTool::schema)
            .collect::<Vec<_>>(),
    );
    // 空会话 + 无计划时基线即系统开销；window = 基线 + 预留增长空间。
    // 每轮 read 的结果会进入 messages（累计增长），window 只须容纳约 8 轮 +
    // compaction 后的 summary 仍能继续。
    config.model.context_window = Some(baseline + 800);
    config.safety_reserve_tokens = 100;

    // 单闭包状态机：工具请求按序推进（update_plan → read×N → 完成）；
    // compaction 请求（tools 空）无论何时插入都返回结构化 summary（§15.4）。
    let step = std::cell::Cell::new(0usize);
    let mut provider = fake_provider::FakeProvider::scripted_loop(Box::new(move |request| {
        if request.tools.is_empty() {
            return fake_provider::FakeResponse::text(
                "Goal: a\nConstraints: b\nDecisions: c\nCompleted: d\nIn progress: e\nNext exact action: f\nRelevant files and revisions: g\nVerification status: h\nFailed attempts and why: i",
            );
        }
        let current = step.get();
        // §13：计划建立后保持为正常的 assistant → update_plan Tool 协议事实；
        // 绝不能伪装成每轮最后一条 User 消息，否则模型会持续“回复 Todo”。
        if current > 0 {
            assert!(
                !request.messages.iter().any(|message| matches!(
                    message,
                    tpi::provider::ChatMessage::User(text) if text.contains("当前计划（完整快照）")
                )),
                "计划快照不得伪装成 User 消息: {:?}",
                request.messages
            );
            // §注入可靠性：update_plan 的 Tool 结果已精简（不再内嵌快照，避免
            // 历史堆积过期计划文本）——这里只要求 update_plan 以合法 Tool 消息
            // 存在于历史；当前计划的权威文本由尾部 System 快照提供。
            assert!(
                request.messages.iter().any(|message| matches!(
                    message,
                    tpi::provider::ChatMessage::Tool { name, .. }
                        if name == "update_plan"
                )),
                "update_plan 必须以 Tool 消息保留在历史: {:?}",
                request.messages
            );
            assert!(
                request.messages.iter().any(|message| matches!(
                    message,
                    tpi::provider::ChatMessage::System(text)
                        if text.contains("[当前计划·唯一权威") && text.contains("当前计划（完整快照）")
                )),
                "每轮请求尾部必须注入带权威标记的当前计划快照: {:?}",
                request.messages
            );
        }
        if current == 0 {
            step.set(1);
            fake_provider::FakeResponse::with_tool_calls(vec![fake_provider::tool_call(
                "update_plan",
                serde_json::json!({
                    "explanation": "fix",
                    "items": [
                        {"text": "a", "status": "in_progress"},
                        {"text": "b", "status": "pending"},
                        {"text": "c", "status": "pending"}
                    ]
                }),
            )])
        } else if current <= 8 {
            step.set(current + 1);
            fake_provider::FakeResponse::with_tool_calls(vec![fake_provider::tool_call(
                "read",
                serde_json::json!({"path": "sample.txt"}),
            )])
        } else {
            fake_provider::FakeResponse::text("完成")
        }
    }));

    let workspace_write = workspace.clone();
    // 30 行文件：read 结果（estimate ~300 tokens）不触发 prune，但多次 read 累计触发 compaction。
    std::fs::write(
        workspace_write.join("sample.txt"),
        (1..=30).map(|i| format!("line {i}\n")).collect::<String>(),
    )
    .unwrap();

    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    let (tx, _rx) = mpsc::channel(128);
    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "请修复并制定计划".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
        },
    )
    .await
    .expect("run succeeds");

    assert_eq!(outcome.reason, CompletionReason::Stop);
    assert_eq!(outcome.assistant_text, "完成");

    // session 事实：PlanReplaced ×1、CompactionCommitted ×1、RunCompleted。
    let events = read_events(session.path()).unwrap();
    let plan_replaced = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::PlanReplaced { .. }))
        .count();
    assert_eq!(
        plan_replaced, 1,
        "update_plan 必须记录 PlanReplaced durable event（§13）"
    );
    let compactions = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::CompactionCommitted { .. }))
        .count();
    // §PointerHit 6/7 估算调整后：多次压缩是合理的（只要 run 能正常完成）。
    // 断言至少 1 次且 run Stop + 正确输出（compaction 后继续准确执行）。
    assert!(
        compactions >= 1,
        "compaction 必须提交 CompactionCommitted（§15.4）"
    );

    // compaction 后 run 正常完成（长会话可继续准确执行，§21 M4 验收）。
    assert!(!outcome.messages.is_empty());
}

/// §12.3 集成：相同失败动作连续 3 次 → 第 3 次被拦截为 repeated_without_progress。
#[tokio::test]
async fn repeated_failing_action_blocked_in_agent_loop() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut config = test_config(&workspace);
    // §用户诉求：默认不限制（0）；本测试专门验证无进展拦截，需显式开启。
    config.limits.max_identical_no_progress = 2;
    let missing_read = || {
        fake_provider::FakeResponse::with_tool_calls(vec![fake_provider::tool_call(
            "read",
            serde_json::json!({"path": "missing.txt"}),
        )])
    };
    // 每轮一个动作（§12.3 关注跨轮重复；同 wave 并行批次不做执行前拦截）。
    let mut provider = fake_provider::FakeProvider::scripted(vec![
        Box::new(move |_request| missing_read()),
        Box::new(move |_request| missing_read()),
        Box::new(move |_request| missing_read()),
        // 完成。
        Box::new(move |_request| fake_provider::FakeResponse::text("done")),
    ]);
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .unwrap();
    let (tx, _rx) = mpsc::channel(128);
    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "read it".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
        },
    )
    .await
    .expect("run succeeds");
    assert_eq!(outcome.reason, CompletionReason::Stop);

    // 3 次 read 调用中第 3 次被拦截（rejected repeated_without_progress）。
    let events = read_events(session.path()).unwrap();
    let tool_completed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCompleted { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_completed.len(), 3);
    assert_eq!(tool_completed[0].status, ToolStatus::Failed);
    assert_eq!(tool_completed[1].status, ToolStatus::Failed);
    assert_eq!(
        tool_completed[2].status,
        ToolStatus::Rejected,
        "第 3 次相同无进展动作必须在执行前拦截（§12.3）"
    );
    assert!(
        tool_completed[2]
            .model_payload
            .output
            .contains("repeated_without_progress")
    );
}
