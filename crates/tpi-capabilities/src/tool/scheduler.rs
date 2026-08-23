//! 基于资源访问声明的工具调度（ToolExecutor 的调度层，纯函数）。
//!
//! - §12.1 资源访问声明：read/list/search 生成 read lock；edit/write 生成 write lock；
//!   run/bash 记为 `WorkspaceUnknown`（跨 AgentGraph 的全局 barrier）。
//! - §12.2 batch 调度：先验证全部参数；按原 call index 与资源冲突构建 execution waves；
//!   同 wave 无冲突 Pure/Read 并行，实际执行再由 `GlobalEffectScheduler` 跨 agent
//!   复核；结果按原 call index 送回 provider。
//! - §12.3 无进展检测：`ActionKey + ObservationKey + StateStamp` 相同才算重复；
//!   默认连续 2 次后，第 3 次执行前返回 `repeated_without_progress`。
//!
//! 边界（AGENTS.md §十）：本模块只做**纯状态转换与声明推导**——无 IO、无 session、
//! 无 provider 依赖。实际执行、持久化与 UI 通知由 ToolExecutor（agent/tool_runtime.rs
//! 的 ToolBatchExecutor）在 waves 之上编排；AgentLoop 只提供 calls 与预算计数。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::tool::{BuiltinTool, ToolContext, ToolExecutionClass, ValidatedArgs};

/// 资源访问声明（§12.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAccess {
    Pure,
    Resources(Vec<ResourceLock>),
    WorkspaceUnknown,
}

/// A cross-agent effect submitted to the workspace scheduler.
///
/// `ToolAccess` is deliberately independent from an agent's identity.  The
/// same scheduler can therefore coordinate calls from every runtime sharing a
/// workspace, while the journal can still retain the caller identity.
#[derive(Debug, Clone)]
pub struct EffectRequest {
    pub agent_id: tpi_core::ids::AgentId,
    pub tool_call_id: tpi_core::ids::ToolCallId,
    pub effect: ToolAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectAcquireError {
    Cancelled,
}

#[derive(Default)]
struct EffectState {
    next_permit: u64,
    active: Vec<(u64, EffectRequest)>,
}

/// Workspace-wide effect scheduler.
///
/// The existing `build_waves` function remains useful for ordering calls in a
/// single model response.  This object is the missing global boundary: every
/// actual tool execution acquires a permit here, so two concurrent agent runs
/// cannot bypass each other's resource declarations.  A permit is held only
/// for the tool call itself; spawning an agent does not acquire a workspace
/// lock.
pub struct GlobalEffectScheduler {
    state: Mutex<EffectState>,
    wake: tokio::sync::Notify,
}

impl Default for GlobalEffectScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalEffectScheduler {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(EffectState::default()),
            wake: tokio::sync::Notify::new(),
        }
    }

    pub fn active_requests(&self) -> Vec<EffectRequest> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .iter()
            .map(|(_, request)| request.clone())
            .collect()
    }

    pub async fn acquire(
        self: &Arc<Self>,
        request: EffectRequest,
        cancel: &CancellationToken,
    ) -> Result<EffectPermit, EffectAcquireError> {
        loop {
            let notified = {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if !state
                    .active
                    .iter()
                    .any(|(_, active)| effects_conflict(&active.effect, &request.effect))
                {
                    let permit_id = state.next_permit;
                    state.next_permit = state.next_permit.wrapping_add(1);
                    state.active.push((permit_id, request.clone()));
                    return Ok(EffectPermit {
                        scheduler: self.clone(),
                        permit_id,
                    });
                }
                self.wake.notified()
            };
            tokio::select! {
                _ = notified => {}
                _ = cancel.cancelled() => return Err(EffectAcquireError::Cancelled),
            }
        }
    }

    fn release(&self, permit_id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(index) = state.active.iter().position(|(id, _)| *id == permit_id) {
            state.active.swap_remove(index);
            self.wake.notify_waiters();
        }
    }
}

pub struct EffectPermit {
    scheduler: Arc<GlobalEffectScheduler>,
    permit_id: u64,
}

impl Drop for EffectPermit {
    fn drop(&mut self) {
        self.scheduler.release(self.permit_id);
    }
}

fn effects_conflict(a: &ToolAccess, b: &ToolAccess) -> bool {
    match (a, b) {
        (ToolAccess::Pure, _) | (_, ToolAccess::Pure) => false,
        (ToolAccess::WorkspaceUnknown, ToolAccess::WorkspaceUnknown)
        | (ToolAccess::WorkspaceUnknown, ToolAccess::Resources(_))
        | (ToolAccess::Resources(_), ToolAccess::WorkspaceUnknown) => true,
        (ToolAccess::Resources(left), ToolAccess::Resources(right)) => left
            .iter()
            .any(|a| right.iter().any(|b| locks_conflict(a, b))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLock {
    pub resource: ResourceId,
    pub mode: AccessMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessMode {
    Read,
    Write,
}

/// 文件资源作用域（§12.1：exact 与 recursive 作用域）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceId {
    File(FileScope),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileScope {
    Exact(camino::Utf8PathBuf),
    Recursive(camino::Utf8PathBuf),
}

/// 工具访问声明（§12.1）。
pub fn tool_access(
    tool: BuiltinTool,
    args: &ValidatedArgs,
    workspace_root: &camino::Utf8PathBuf,
    allow_outside_workspace: bool,
) -> ToolAccess {
    fn resolved_path(
        workspace_root: &camino::Utf8PathBuf,
        path: &str,
        allow_outside: bool,
    ) -> camino::Utf8PathBuf {
        crate::tool::resolve_lock_path(workspace_root, path, allow_outside)
    }
    if args.tool() != tool {
        tracing::error!(
            tool = ?tool,
            args_tool = ?args.tool(),
            "tool_access: tool 与已校验参数不匹配；保守隔离执行"
        );
        return ToolAccess::WorkspaceUnknown;
    }

    let class = tool.execution_class();
    match class {
        ToolExecutionClass::Pure => ToolAccess::Pure,
        ToolExecutionClass::WorkspaceUnknown => ToolAccess::WorkspaceUnknown,
        ToolExecutionClass::FileReadExact | ToolExecutionClass::FileWriteExact => {
            let Some(raw_path) = args.path() else {
                tracing::error!(
                    tool = ?tool,
                    class = ?class,
                    "tool_access: 文件资源工具缺少 path；保守隔离执行"
                );
                return ToolAccess::WorkspaceUnknown;
            };
            let path = resolved_path(workspace_root, raw_path, allow_outside_workspace);
            match class {
                ToolExecutionClass::FileReadExact => {
                    file_lock(FileScope::Exact(path), AccessMode::Read)
                }
                ToolExecutionClass::FileWriteExact => {
                    file_lock(FileScope::Exact(path), AccessMode::Write)
                }
                ToolExecutionClass::Pure | ToolExecutionClass::WorkspaceUnknown => {
                    tracing::error!(
                        tool = ?tool,
                        class = ?class,
                        "tool_access: 执行类别在资源锁构造期间发生不一致；保守隔离执行"
                    );
                    ToolAccess::WorkspaceUnknown
                }
            }
        }
    }
}

fn file_lock(scope: FileScope, mode: AccessMode) -> ToolAccess {
    ToolAccess::Resources(vec![ResourceLock {
        resource: ResourceId::File(scope),
        mode,
    }])
}

/// Read-only workspace access declaration for external tools. This is an
/// effect classification, not an agent capability; it can overlap other
/// readers and conflicts with writes.
pub fn read_workspace_lock(
    workspace_root: &camino::Utf8PathBuf,
    allow_outside_workspace: bool,
) -> ToolAccess {
    let root = crate::tool::resolve_lock_path(workspace_root, ".", allow_outside_workspace);
    file_lock(FileScope::Recursive(root), AccessMode::Read)
}

/// 两个文件作用域是否冲突（§12.1：同一 root 下祖先/后代包含关系且至少一方为 write）。
pub fn scopes_conflict(a: &FileScope, b: &FileScope) -> bool {
    fn path_of(scope: &FileScope) -> &camino::Utf8PathBuf {
        match scope {
            FileScope::Exact(path) | FileScope::Recursive(path) => path,
        }
    }
    let a_path = path_of(a);
    let b_path = path_of(b);
    let a_contains_b = if matches!(a, FileScope::Recursive(_)) {
        a_path == b_path || b_path.starts_with(a_path)
    } else {
        a_path == b_path
    };
    let b_contains_a = if matches!(b, FileScope::Recursive(_)) {
        b_path == a_path || a_path.starts_with(b_path)
    } else {
        b_path == a_path
    };
    a_contains_b || b_contains_a
}

/// 两个资源锁是否冲突（§12.1：至少一方为 write 时冲突）。
pub fn locks_conflict(a: &ResourceLock, b: &ResourceLock) -> bool {
    let same_root = match (&a.resource, &b.resource) {
        (ResourceId::File(scope_a), ResourceId::File(scope_b)) => scopes_conflict(scope_a, scope_b),
    };
    same_root && (a.mode == AccessMode::Write || b.mode == AccessMode::Write)
}

/// 已预检的 tool call（§8.2 `PreparedToolCall` 的 M4 形态）。
#[derive(Debug)]
pub struct PreparedCall {
    pub source_index: usize,
    pub kind: PreparedKind,
    pub access: ToolAccess,
    /// §12.3：ActionKey = hash(tool_name + canonical_json(args))。
    pub action_key: String,
    /// §10.7：写工具（edit/write）的提交计划（temp/backup 路径）。
    /// 预检阶段生成一次，write-ahead 持久化与真正执行**必须复用同一 plan**，
    /// 否则 recovery metadata 指向的路径与实际执行不一致，崩溃恢复判定失效。
    pub plan: Option<crate::tool::edit::CommitPlan>,
}

/// 已预检的工具种类：builtin（类型化）或外部（MCP adapter，README2 §2.1）。
/// 不 derive Debug：`Arc<dyn Tool>` 不实现 Debug。
#[derive(Clone)]
pub enum PreparedKind {
    /// 内建工具（类型化 args + commit plan 语义）。
    Builtin {
        tool: BuiltinTool,
        args: ValidatedArgs,
    },
    /// 外部工具（MCP 等；JSON args，经 Tool trait 执行）。
    External {
        name: String,
        args_json: String,
        adapter: std::sync::Arc<dyn crate::tool::registry::Tool>,
    },
}

impl PreparedKind {
    pub fn name(&self) -> &str {
        match self {
            PreparedKind::Builtin { tool, .. } => tool.name(),
            PreparedKind::External { name, .. } => name,
        }
    }
}

impl std::fmt::Debug for PreparedKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreparedKind::Builtin { tool, .. } => {
                write!(f, "Builtin({})", tool.name())
            }
            PreparedKind::External { name, .. } => {
                write!(f, "External({name})")
            }
        }
    }
}

/// 构建 execution waves（§12.2 第 3-4 条）。
///
/// - Write / WorkspaceUnknown：独占 wave（按源顺序）；
/// - Pure / Read：与前面 wave 中冲突的 write 隔离；
/// - 同 wave 并行度受 `max_parallel` 限制。
pub fn build_waves(calls: Vec<PreparedCall>, max_parallel: usize) -> Vec<Vec<PreparedCall>> {
    let max_parallel = max_parallel.max(1);
    let mut waves: Vec<Vec<PreparedCall>> = Vec::new();
    for call in calls {
        match &call.access {
            ToolAccess::WorkspaceUnknown => {
                waves.push(vec![call]); // §12.1：按源顺序串行
            }
            ToolAccess::Pure => {
                push_with_conflict_check(&mut waves, call, max_parallel, false);
            }
            ToolAccess::Resources(locks) => {
                let is_write = locks.iter().any(|lock| lock.mode == AccessMode::Write);
                if is_write {
                    // Write 独占 wave（§12.2 第 4 条：Write 按源顺序执行）。
                    waves.push(vec![call]);
                } else {
                    push_with_conflict_check(&mut waves, call, max_parallel, true);
                }
            }
        }
    }
    waves.into_iter().filter(|wave| !wave.is_empty()).collect()
}

/// 把 read call 放入与写锁不冲突的 wave；当前 wave 满或冲突时开新 wave。
///
/// WorkspaceUnknown（bash）影响整个 workspace：其 wave 是"隔离点"，
/// 后续 call 不得并入该 wave（只能开新 wave；新 wave 之间仍可正常合并）。
fn push_with_conflict_check(
    waves: &mut Vec<Vec<PreparedCall>>,
    call: PreparedCall,
    max_parallel: usize,
    check_conflicts: bool,
) {
    if let Some(wave) = waves.last_mut() {
        let last_is_unknown_wave = wave
            .iter()
            .any(|other| matches!(other.access, ToolAccess::WorkspaceUnknown));
        let fits = wave.len() < max_parallel && !last_is_unknown_wave;
        let no_conflict = if check_conflicts {
            let empty: Vec<ResourceLock> = Vec::new();
            let locks = match &call.access {
                ToolAccess::Resources(locks) => locks,
                _ => &empty,
            };
            !wave.iter().any(|other| {
                let other_locks = match &other.access {
                    ToolAccess::Resources(locks) => locks,
                    _ => &empty,
                };
                locks.iter().any(|lock| {
                    other_locks
                        .iter()
                        .any(|other_lock| locks_conflict(lock, other_lock))
                })
            })
        } else {
            true
        };
        if fits && no_conflict {
            wave.push(call);
            return;
        }
    }
    waves.push(vec![call]);
}

/// ActionKey（§12.3）：hash(tool_name + canonical_json(args))。
pub fn action_key(tool: BuiltinTool, args_json: &str) -> String {
    action_key_from_name(tool.name(), args_json)
}

/// ActionKey（外部工具版，README2 Phase 5）。
pub fn action_key_from_name(name: &str, args_json: &str) -> String {
    // serde_json 的 Map 默认有序（BTreeMap），canonical 序列化可复现。
    // ISSUE-042：数值字面量归一——`1000`（int）与 `1000.0`/`1e3`（float）
    // 在 Value::to_string 中产生不同文本，会把同一动作判成不同 action；
    // 统一为「整数值按 u64、浮点按 u64 精确表示时」的形式后再哈希。
    let canonical = serde_json::from_str::<serde_json::Value>(args_json)
        .map(|value| normalize_numbers(value).to_string())
        .unwrap_or_else(|_| args_json.to_string());
    let digest = blake3::hash(format!("{name}|{canonical}").as_bytes());
    digest.to_hex()[..16].to_string()
}

/// 递归把数值统一：整数保持 u64；浮点若精确等于某整数（`1000.0`、`1e3`）
/// 归一为该整数，否则保留原文。数组/对象递归。
fn normalize_numbers(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_u64() {
                serde_json::Value::Number(serde_json::Number::from(i))
            } else if let Some(f) = n.as_f64()
                && f.fract() == 0.0
                && f.is_finite()
                && f >= 0.0
                && f <= u64::MAX as f64
            {
                serde_json::Value::Number(serde_json::Number::from(f as u64))
            } else {
                serde_json::Value::Number(n)
            }
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(normalize_numbers).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, normalize_numbers(v)))
                .collect(),
        ),
        other => other,
    }
}

/// ObservationKey（§12.3）：hash(status + bounded_model_payload)，排除易变字段。
pub fn observation_key(status: &str, bounded_payload: &str) -> String {
    let digest = blake3::hash(format!("{status}|{bounded_payload}").as_bytes());
    digest.to_hex()[..16].to_string()
}

/// 无进展检测器（§12.3）。
#[derive(Debug, Default)]
pub struct ProgressTracker {
    known_workspace_epoch: u64,
    last: Option<RepeatKey>,
    repeat_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepeatKey {
    action: String,
    observation: String,
    state: String,
}

impl ProgressTracker {
    /// 相关文件 revision 变化后允许重试（§12.3：edit/write 成功或观察到新 revision 时增加）。
    pub fn bump_workspace_epoch(&mut self) {
        self.known_workspace_epoch += 1;
    }

    pub fn workspace_epoch(&self) -> u64 {
        self.known_workspace_epoch
    }

    /// 当前重复计数（测试与诊断用）。
    pub fn repeat_count(&self) -> u32 {
        self.repeat_count
    }

    /// 执行前判定：相同 action 且 StateStamp 未变，且前两次 (action, observation, state)
    /// 全同（连续 2 次无进展，repeat_count >= 1）时，第 3 次执行前拒绝（§12.3）。
    pub fn should_block(&self, action: &str, state: &str) -> bool {
        self.repeat_count >= 1
            && self
                .last
                .as_ref()
                .map(|k| k.action == action)
                .unwrap_or(false)
            && self
                .last
                .as_ref()
                .map(|k| k.state == state)
                .unwrap_or(false)
    }

    /// 记录本次执行后的观察（§12.3：ActionKey + ObservationKey + StateStamp 相同才算重复）。
    pub fn observe(&mut self, action: &str, observation: &str, state: &str) {
        let key = RepeatKey {
            action: action.to_string(),
            observation: observation.to_string(),
            state: state.to_string(),
        };
        if self.last.as_ref() == Some(&key) {
            self.repeat_count += 1;
        } else {
            self.repeat_count = 0;
        }
        self.last = Some(key);
    }
}

/// 从工具结果构造 ObservationKey 的稳定输入（§12.3：排除 duration/timestamp/artifact id）。
pub fn stable_observation(tool: &str, status: &str, payload: &str) -> String {
    format!(
        "{tool}:{status}:{}",
        &blake3::hash(payload.as_bytes()).to_hex()[..24]
    )
}

/// 从快照存储构造 StateStamp（§12.3：access footprint revisions）。
pub fn state_stamp_from_ctx(ctx: &ToolContext, access: &ToolAccess) -> String {
    match access {
        ToolAccess::Resources(locks) => {
            let store = tpi_core::util::lock_mutex(&ctx.snapshot_store, "snapshot_store");
            let mut parts: Vec<String> = locks
                .iter()
                .filter_map(|lock| match &lock.resource {
                    ResourceId::File(FileScope::Exact(path)) => {
                        let path =
                            crate::tool::resolve_workspace_path(&ctx.workspace_root, path.as_str())
                                .unwrap_or_else(|_| path.clone());
                        store
                            .latest(&path)
                            .map(|snapshot| format!("{}={}", path, snapshot.revision))
                    }
                    _ => None,
                })
                .collect();
            parts.sort();
            let digest = blake3::hash(parts.join(";").as_bytes());
            digest.to_hex()[..16].to_string()
        }
        _ => String::new(),
    }
}

/// 批次结果按原 call index 回填（§12.2 第 6 条）。
pub fn collect_by_source_index<T>(mut results: HashMap<usize, T>, count: usize) -> Vec<Option<T>> {
    let mut ordered: Vec<Option<T>> = Vec::with_capacity(count);
    for index in 0..count {
        ordered.push(results.remove(&index));
    }
    ordered
}

#[cfg(test)]
mod global_scheduler_tests {
    use super::*;
    use std::time::Duration;

    fn write(path: &str) -> ToolAccess {
        ToolAccess::Resources(vec![ResourceLock {
            resource: ResourceId::File(FileScope::Exact(path.into())),
            mode: AccessMode::Write,
        }])
    }

    fn request(effect: ToolAccess) -> EffectRequest {
        EffectRequest {
            agent_id: tpi_core::ids::AgentId::new_v7(),
            tool_call_id: tpi_core::ids::ToolCallId::new_v7(),
            effect,
        }
    }

    #[tokio::test]
    async fn different_file_writes_can_overlap() {
        let scheduler = Arc::new(GlobalEffectScheduler::new());
        let cancel = CancellationToken::new();
        let _a = scheduler
            .acquire(request(write("a.rs")), &cancel)
            .await
            .unwrap();
        let _b = scheduler
            .acquire(request(write("b.rs")), &cancel)
            .await
            .unwrap();
        assert_eq!(scheduler.active_requests().len(), 2);
    }

    #[tokio::test]
    async fn same_file_write_waits_and_unknown_is_global_barrier() {
        let scheduler = Arc::new(GlobalEffectScheduler::new());
        let cancel = CancellationToken::new();
        let first = scheduler
            .acquire(request(write("a.rs")), &cancel)
            .await
            .unwrap();

        let waiter_scheduler = scheduler.clone();
        let waiter_cancel = cancel.clone();
        let mut waiter = tokio::spawn(async move {
            waiter_scheduler
                .acquire(request(write("a.rs")), &waiter_cancel)
                .await
                .unwrap()
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );
        drop(first);
        let _second = waiter.await.unwrap();

        let unknown = scheduler
            .acquire(request(ToolAccess::WorkspaceUnknown), &cancel)
            .await
            .unwrap();
        let waiter_scheduler = scheduler.clone();
        let waiter_cancel = cancel.clone();
        let mut blocked = tokio::spawn(async move {
            waiter_scheduler
                .acquire(request(write("different.rs")), &waiter_cancel)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );
        drop(unknown);
        assert!(blocked.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_leak_a_permit() {
        let scheduler = Arc::new(GlobalEffectScheduler::new());
        let cancel = CancellationToken::new();
        let _first = scheduler
            .acquire(request(ToolAccess::WorkspaceUnknown), &cancel)
            .await
            .unwrap();
        let blocked_cancel = CancellationToken::new();
        let blocked = scheduler.acquire(request(write("a.rs")), &blocked_cancel);
        blocked_cancel.cancel();
        assert!(matches!(blocked.await, Err(EffectAcquireError::Cancelled)));
        assert_eq!(scheduler.active_requests().len(), 1);
    }
}
