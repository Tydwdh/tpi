//! P2-04：durability barrier 类型化的 fault injection 测试。
//!
//! 覆盖三种 barrier（commit / `commit_terminal` / `commit_pre_effect）在写入`
//! 失败时的行为：
//! - 错误必须传播（不静默吞掉）；
//! - 部分写后 seq 不回滚但 `pending_sync` 状态正确（下次 sync 重试）；
//! - recovery `matrix（tests/recovery_matrix.rs）不退化——本文件聚焦` barrier
//!   语义，不重复 crash 场景。
//!
//! fault 注入方式：barrier 方法内部调用 `append_event（写文件`）+ `sync_data`
//! （落盘）。无法直接注入文件系统错误，因此用一个**可故障的 in-memory
//! `SessionStore`**（append 成功但 sync 可失败）验证错误传播与意图方法等价
//! 于 append+sync。

use std::sync::atomic::{AtomicBool, Ordering};

use tpi::ids::{RunId, SessionId, ToolCallId};
use tpi::session::protocol::{CompletionReason, SessionEvent, Usage};
use tpi::session::store::SessionStore;

/// 可故障 in-memory store：append 总是成功；sync 按 flag 失败。
struct FaultyStore {
    events: Vec<SessionEvent>,
    seq: u64,
    session_id: SessionId,
    run_id: RunId,
    fail_sync: AtomicBool,
    state: tpi::session::SessionState,
}

impl FaultyStore {
    fn new(fail_sync: bool) -> Self {
        Self {
            events: Vec::new(),
            seq: 0,
            session_id: SessionId::new_v7(),
            run_id: RunId::new_v7(),
            fail_sync: AtomicBool::new(fail_sync),
            state: tpi::session::SessionState::default(),
        }
    }
    fn events(&self) -> &[SessionEvent] {
        &self.events
    }
}

impl SessionStore for FaultyStore {
    fn begin_run(&mut self) -> RunId {
        self.run_id = RunId::new_v7();
        self.run_id
    }
    fn session_id(&self) -> SessionId {
        self.session_id
    }
    fn seq(&self) -> u64 {
        self.seq
    }
    fn path(&self) -> &std::path::Path {
        std::path::Path::new(":faulty:")
    }
    fn append_event(&mut self, event: &SessionEvent) -> std::io::Result<u64> {
        self.events.push(event.clone());
        self.seq = self.seq.saturating_add(1);
        self.state.apply(event);
        Ok(self.seq)
    }
    fn sync_data(&mut self) -> std::io::Result<()> {
        if self.fail_sync.load(Ordering::SeqCst) {
            Err(std::io::Error::other("injected sync failure"))
        } else {
            Ok(())
        }
    }
    fn write_ahead_tool(
        &mut self,
        call_id: ToolCallId,
        recovery: Option<tpi::session::protocol::RecoveryMetadata>,
    ) -> std::io::Result<()> {
        self.append_event(&SessionEvent::ToolStarted { call_id, recovery })?;
        self.sync_data()
    }
    fn complete_tool(
        &mut self,
        call_id: ToolCallId,
        outcome: &tpi::outcome::StoredToolOutcome,
    ) -> std::io::Result<()> {
        self.append_event(&SessionEvent::ToolCompleted {
            call_id,
            outcome: outcome.clone(),
        })?;
        self.sync_data()
    }
    fn state(&self) -> &tpi::session::SessionState {
        &self.state
    }
    fn events_with_seq(&self) -> std::io::Result<Vec<(u64, SessionEvent)>> {
        Ok(self
            .events
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, e)| (i as u64 + 1, e))
            .collect())
    }
}

fn user_event(i: u64) -> SessionEvent {
    SessionEvent::UserSubmitted {
        content: format!("msg-{i}"),
    }
}

fn run_completed() -> SessionEvent {
    SessionEvent::RunCompleted {
        reason: CompletionReason::Stop,
        usage: Usage::default(),
    }
}

/// fault：sync 失败时 commit 必须传播错误，且事件已 append（不静默丢失）。
#[test]
fn commit_propagates_sync_failure() {
    let mut store = FaultyStore::new(true);
    let err = store.commit(&user_event(1)).expect_err("sync 失败必须传播");
    assert!(err.to_string().contains("injected"), "{err}");
    // 事件已 append（append 成功），seq 前进——调用方可重试 sync。
    assert_eq!(store.seq(), 1);
    assert_eq!(store.events().len(), 1);
}

/// 恢复：sync 失败后清除 flag，commit 再次成功（pending 状态可重试）。
#[test]
fn commit_succeeds_after_fault_cleared() {
    let mut store = FaultyStore::new(true);
    assert!(store.commit(&user_event(1)).is_err());
    store.fail_sync.store(false, Ordering::SeqCst);
    // 再次 commit 新事件成功。
    let seq = store
        .commit(&user_event(2))
        .expect("fault 清除后 commit 成功");
    assert_eq!(seq, 2);
    assert_eq!(store.events().len(), 2);
}

/// `commit_terminal` 与 commit 等价（append+sync）：sync 失败传播。
#[test]
fn commit_terminal_propagates_sync_failure() {
    let mut store = FaultyStore::new(true);
    assert!(store.commit_terminal(&run_completed()).is_err());
    store.fail_sync.store(false, Ordering::SeqCst);
    let seq = store
        .commit_terminal(&run_completed())
        .expect("终态提交成功");
    // 第一次失败的 append 已计入 seq（append 成功，sync 失败），第二次成功 seq=2。
    assert_eq!(seq, 2);
    assert!(matches!(
        store.events()[1],
        SessionEvent::RunCompleted { .. }
    ));
}

/// commit_pre_effect（write-ahead）sync 失败传播；成功时 `ToolStarted` 已落盘。
#[test]
fn commit_pre_effect_propagates_and_commits() {
    let mut store = FaultyStore::new(true);
    let call_id = ToolCallId::new_v7();
    assert!(
        store.commit_pre_effect(call_id, None).is_err(),
        "write-ahead sync 失败传播"
    );
    store.fail_sync.store(false, Ordering::SeqCst);
    store
        .commit_pre_effect(call_id, None)
        .expect("write-ahead 成功");
    assert!(matches!(
        store.events()[0],
        SessionEvent::ToolStarted { .. }
    ));
}

/// 意图方法等价性：commit(event) == `append_event` + `sync_data（无` fault 时）。
#[test]
fn typed_barriers_equal_append_plus_sync() {
    let mut typed = FaultyStore::new(false);
    let mut manual = FaultyStore::new(false);

    let t_seq = typed.commit(&user_event(1)).unwrap();
    let m_seq = manual.append_event(&user_event(1)).unwrap();
    manual.sync_data().unwrap();
    assert_eq!(t_seq, m_seq);
    assert_eq!(typed.events(), manual.events());

    // commit_terminal 等价
    let t2 = typed.commit_terminal(&run_completed()).unwrap();
    let m2 = manual.append_event(&run_completed()).unwrap();
    manual.sync_data().unwrap();
    assert_eq!(t2, m2);
    assert_eq!(typed.events(), manual.events());
}
