//! P0-03：session golden corpus 回归测试。
//!
//! `corpus（tests/fixtures/session_corpus/）来自真实` session 的脱敏副本：
//! - `001_tool_loop` / `002_stream_interrupted` / `003_awaiting_input` /
//!   `004_compaction_segment：真实事件序列`，`read_events` 必须完整 replay；
//! - `005_corrupt_tail：尾部未完成行` → reader `必须截断并成功（TailRepair::Truncate`）；
//! - `006_corrupt_middle：中间坏行` → reader 必须返回 `InvalidData`。
//!
//! 脱敏规则见 `scripts/scrub_session.py；来源/行数/hash` 见 manifest.json。
//! 这些 fixture 是 P2 拆 session 存储 / P10 迁移的回归网：任何改动必须
//! 让本文件保持绿色（并重新生成 manifest 的 hash 校验）。

mod fixtures;

use std::path::Path;

use tpi::session::{SessionEvent, read_events, read_events_with_seq};

fn corpus_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/session_corpus")
        .leak()
}

fn fixture(id: &str) -> std::path::PathBuf {
    corpus_dir().join(format!("{id}.jsonl"))
}

/// manifest.json 中记录的 fixture 元数据（行数 = 完整行数，corrupt 除外）。
fn expected_lines(id: &str) -> usize {
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(corpus_dir().join("manifest.json")).unwrap())
            .unwrap();
    manifest["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == id)
        .unwrap_or_else(|| panic!("manifest 缺 fixture {id}"))["fixture_lines"]
        .as_u64()
        .unwrap() as usize
}

fn has_event(id: &str, pred: impl Fn(&SessionEvent) -> bool) -> bool {
    read_events(&fixture(id)).unwrap().iter().any(pred)
}

#[test]
fn real_fixtures_replay_completely() {
    // 001-004：完整 replay，事件数 = manifest 记录的行数。
    for id in [
        "001_tool_loop",
        "002_stream_interrupted",
        "003_awaiting_input",
        "004_compaction_segment",
    ] {
        let events = read_events(&fixture(id)).unwrap_or_else(|e| panic!("{id} replay 失败: {e}"));
        assert_eq!(
            events.len(),
            expected_lines(id),
            "{id} 事件数应等于 manifest 行数"
        );
    }
}

#[test]
fn seq_is_strictly_monotonic() {
    // envelope seq 严格递增（P0-3/§19B：seq 是投影与恢复的依赖）。
    for id in [
        "001_tool_loop",
        "003_awaiting_input",
        "004_compaction_segment",
    ] {
        let seqs: Vec<u64> = read_events_with_seq(&fixture(id))
            .unwrap()
            .into_iter()
            .map(|(seq, _)| seq)
            .collect();
        for w in seqs.windows(2) {
            assert!(w[0] < w[1], "{id}: seq 未严格递增 {w:?}");
        }
    }
}

#[test]
fn lifecycle_coverage_present() {
    // 每个特殊 lifecycle 都有对应 fixture 且能被 reader 还原。
    assert!(has_event("002_stream_interrupted", |e| matches!(
        e,
        SessionEvent::AssistantAttemptInterrupted { .. }
    )));
    assert!(has_event("003_awaiting_input", |e| matches!(
        e,
        SessionEvent::UserInputRequested { .. }
    )));
    assert!(has_event("004_compaction_segment", |e| matches!(
        e,
        SessionEvent::CompactionCommitted { .. }
    )));
    assert!(has_event("001_tool_loop", |e| matches!(
        e,
        SessionEvent::PlanReplaced { .. }
    )));
    assert!(has_event("001_tool_loop", |e| matches!(
        e,
        SessionEvent::ToolRequested { .. }
    )));
}

#[test]
fn corrupt_tail_is_truncated() {
    // 尾部未完成行：reader 截断并成功，事件数 = 原 002 的行数（36）。
    let events = read_events(&fixture("005_corrupt_tail")).expect("corrupt tail 应成功");
    assert_eq!(events.len(), 36, "坏尾部应被截断");
}

#[test]
fn corrupt_middle_is_rejected() {
    // 中间坏行：reader 必须报 InvalidData（不允许静默跳过）。
    let err = read_events(&fixture("006_corrupt_middle")).expect_err("corrupt middle 应报错");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    let msg = err.to_string();
    assert!(
        msg.contains("损坏") || msg.contains("InvalidData"),
        "错误应说明损坏行: {msg}"
    );
}

#[test]
fn manifest_hashes_are_stable() {
    // fixture 未被手改：每个文件的 blake3 与 manifest 一致。
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(corpus_dir().join("manifest.json")).unwrap())
            .unwrap();
    for f in manifest["fixtures"].as_array().unwrap() {
        let id = f["id"].as_str().unwrap();
        let expected = f["blake3"]
            .as_str()
            .unwrap_or_else(|| panic!("{id} 缺 blake3 字段"));
        let bytes = std::fs::read(fixture(id)).unwrap();
        let digest = blake3::hash(&bytes).to_hex().to_string()[..16].to_string();
        assert_eq!(
            &digest, expected,
            "{id} blake3 与 manifest 不一致（fixture 被手改？）"
        );
    }
}
