//! P0-05：合成性能基线（显式运行：`cargo test --release --test perf_baseline -- --ignored`）。
//!
//! 目标：为重构建立**可重复**的性能高水位，不要求先优化（P0-05 验收：
//! 结果存 artifact）。覆盖文档要求的核心 fixture/指标：
//!
//! - session replay：1k / 10k 消息合成 session 的 append + read_events 耗时；
//! - context build：1k / 10k 消息的 estimate_request + prune_messages 耗时；
//! - 1MB streaming message：estimate_tokens + model push 耗时；
//! - 100 tool cards：model 高频 append（近似卡片行）耗时；
//! - 高频 append：连续 append 的耗时曲线（10h 模拟的合成形式）。
//!
//! 结果写入 target/perf-baseline.json（stdout 也打印摘要）。
//! 基准数值是**基线**，不是目标；后续 Phase 必须对比此基线证明不退化
//! （P10-03 逐项 before/after）。

use std::time::Instant;

use tpi::config::Config;
use tpi::context;
use tpi::ids::RunId;
use tpi::provider::ChatMessage;
use tpi::session::{
    AssistantMessage, CompletionReason, ModelRef, RunLimits, SessionEvent, SessionLog, Usage,
};
use tpi::tui::model::{LineKind, ViewModel};

fn test_config(workspace: &camino::Utf8PathBuf) -> Config {
    let mut cfg = fixtures::test_config(workspace);
    // 性能测试需要足够大的 context window 让 10k 消息走 estimate 主路径。
    cfg.model.context_window = Some(1_000_000);
    cfg
}

/// 合成 n 条 assistant 消息的 session（n/10 个 run，每 run 10 条），
/// 返回 (append 耗时, 文件字节, replay 耗时)。
fn synth_session(n_messages: usize) -> (std::time::Duration, u64, std::time::Duration) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let cfg = test_config(&workspace);
    let mut session =
        SessionLog::create(&cfg.sessions_root, workspace.as_std_path(), RunId::new_v7())
            .expect("create session");
    let model = ModelRef {
        name: "perf-model".into(),
        provider: "perf".into(),
    };
    let limits = RunLimits {
        max_turns: 100,
        max_tool_calls: 100,
    };
    let t0 = Instant::now();
    let runs = n_messages.div_ceil(10);
    for _ in 0..runs {
        session
            .append_event(&SessionEvent::UserSubmitted {
                content: "perf user message".into(),
            })
            .unwrap();
        session
            .append_event(&SessionEvent::RunStarted {
                model: model.clone(),
                limits,
            })
            .unwrap();
        for i in 0..10 {
            session
                .append_event(&SessionEvent::AssistantMessageCommitted {
                    message: AssistantMessage {
                        content: format!("assistant message {i} with some content to measure"),
                        tool_calls: Vec::new(),
                    },
                })
                .unwrap();
        }
        session
            .append_event(&SessionEvent::RunCompleted {
                reason: CompletionReason::Stop,
                usage: Usage::default(),
            })
            .unwrap();
    }
    let append = t0.elapsed();
    let bytes = std::fs::metadata(session.path()).unwrap().len();
    let t1 = Instant::now();
    let n = tpi::session::read_events(session.path()).unwrap().len();
    let replay = t1.elapsed();
    // 每 run = UserSubmitted + RunStarted + 10×Assistant + RunCompleted = 13。
    assert_eq!(n, runs * 13, "合成事件数应 = run×13");
    (append, bytes, replay)
}

/// 合成 n 条消息的 ChatMessage 数组，测 estimate_request + prune 耗时。
fn synth_context(n_messages: usize) -> (std::time::Duration, u64, std::time::Duration) {
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(n_messages);
    for i in 0..n_messages {
        messages.push(ChatMessage::User(format!("user message {i} some content")));
        messages.push(ChatMessage::Assistant {
            content: format!("assistant message {i} some content to measure"),
            tool_calls: Vec::new(),
        });
    }
    let t0 = Instant::now();
    let tokens = context::estimate_request("perf system prompt", &messages, &[]);
    let t1 = t0.elapsed();
    let t2 = Instant::now();
    let pruned = context::prune_messages(messages);
    let prune = t2.elapsed();
    assert!(!pruned.is_empty());
    (t1, tokens, prune)
}

// ---- 结果记录：stdout + target/perf-baseline.json ----

use std::sync::Mutex;

static RESULTS: Mutex<Vec<(String, f64)>> = Mutex::new(Vec::new());

fn record(key: &str, value: f64) {
    println!("  {key:<36} {value:12.3}");
    RESULTS.lock().unwrap().push((key.to_string(), value));
}

fn write_results() {
    let results = RESULTS.lock().unwrap();
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("perf-baseline.json");
    let obj: serde_json::Value = serde_json::json!({
        "generated": "2026-08-14",
        "profile": "release",
        "rustc": "1.97.1",
        "crate_version": env!("CARGO_PKG_VERSION"),
        "metrics": results
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v)))
            .collect::<serde_json::Map<_, _>>(),
    });
    std::fs::write(&out, serde_json::to_string_pretty(&obj).unwrap()).unwrap();
    println!("  -> 写入 {}", out.display());
}

/// 串行运行全部测量（顺序可控、结果一次写入）。
#[test]
#[ignore = "P0-05 显式性能基线（release 下运行）"]
fn perf_all() {
    println!("== session replay 1k ==");
    let (a, b, r) = synth_session(1_000);
    record("replay_1k_append_ms", a.as_secs_f64() * 1000.0);
    record("replay_1k_file_bytes", b as f64);
    record("replay_1k_read_ms", r.as_secs_f64() * 1000.0);

    println!("== session replay 10k ==");
    let (a, b, r) = synth_session(10_000);
    record("replay_10k_append_ms", a.as_secs_f64() * 1000.0);
    record("replay_10k_file_bytes", b as f64);
    record("replay_10k_read_ms", r.as_secs_f64() * 1000.0);

    println!("== context build 1k ==");
    let (e, t, p) = synth_context(1_000);
    record("context_1k_estimate_ms", e.as_secs_f64() * 1000.0);
    record("context_1k_estimate_tokens", t as f64);
    record("context_1k_prune_ms", p.as_secs_f64() * 1000.0);

    println!("== context build 10k ==");
    let (e, t, p) = synth_context(10_000);
    record("context_10k_estimate_ms", e.as_secs_f64() * 1000.0);
    record("context_10k_estimate_tokens", t as f64);
    record("context_10k_prune_ms", p.as_secs_f64() * 1000.0);

    println!("== 1MB streaming message ==");
    let big = "x".repeat(1024 * 1024);
    let t0 = Instant::now();
    let tokens = context::estimate_tokens(&big);
    record(
        "stream_1mb_estimate_ms",
        t0.elapsed().as_secs_f64() * 1000.0,
    );
    record("stream_1mb_tokens", tokens as f64);
    let mut model = ViewModel::default();
    let t1 = Instant::now();
    model.push_stream_delta(LineKind::Assistant, &big);
    record(
        "stream_1mb_model_push_ms",
        t1.elapsed().as_secs_f64() * 1000.0,
    );

    println!("== 100 tool cards ==");
    let mut model = ViewModel::default();
    let t0 = Instant::now();
    for i in 0..100 {
        model.push_line(
            LineKind::Tool,
            format!("tool card {i}: bash -c 'echo perf' (completed 0.00s)"),
        );
    }
    record(
        "tool_cards_100_push_ms",
        t0.elapsed().as_secs_f64() * 1000.0,
    );

    println!("== 高频 append（10h 模拟的合成形式）==");
    let dir = tempfile::tempdir().unwrap();
    let workspace = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let cfg = test_config(&workspace);
    let mut session =
        SessionLog::create(&cfg.sessions_root, workspace.as_std_path(), RunId::new_v7())
            .expect("create session");
    let t0 = Instant::now();
    let mut samples = Vec::new();
    for i in 0..10_000 {
        session
            .append_event(&SessionEvent::AssistantMessageCommitted {
                message: AssistantMessage {
                    content: format!("msg {i} content"),
                    tool_calls: Vec::new(),
                },
            })
            .unwrap();
        if (i + 1) % 1000 == 0 {
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
    }
    record(
        "hf_append_10k_total_ms",
        t0.elapsed().as_secs_f64() * 1000.0,
    );
    for (idx, s) in samples.iter().enumerate() {
        record(&format!("hf_append_10k_cum_{}", (idx + 1) * 1000), *s);
    }

    write_results();
}

mod fixtures;
