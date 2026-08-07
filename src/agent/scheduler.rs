//! 工具调度（文档 §12）。
//!
//! - §12.1 资源访问声明：read/list/search 生成 read lock；edit/write 生成 write lock；
//!   run/bash 记为 `WorkspaceUnknown`（按源顺序串行）。
//! - §12.2 batch 调度：先验证全部参数；按原 call index 与资源冲突构建 execution waves；
//!   同 wave 无冲突 Pure/Read 并行（受 `max_parallel_tools` 限制）；Write/WorkspaceUnknown
//!   按源顺序执行；结果按原 call index 送回 provider。
//! - §12.3 无进展检测：`ActionKey + ObservationKey + StateStamp` 相同才算重复；
//!   默认连续 2 次后，第 3 次执行前返回 `repeated_without_progress`。

use std::collections::HashMap;

use crate::tool::{BuiltinTool, ToolContext, ValidatedArgs};

/// 资源访问声明（§12.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAccess {
    Pure,
    Resources(Vec<ResourceLock>),
    WorkspaceUnknown,
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
) -> ToolAccess {
    fn resolved_path(workspace_root: &camino::Utf8PathBuf, path: &str) -> camino::Utf8PathBuf {
        crate::tool::resolve_workspace_path(workspace_root, path)
            .unwrap_or_else(|_| camino::Utf8PathBuf::from(path))
    }

    match (tool, args) {
        (BuiltinTool::Read, ValidatedArgs::Read(args)) => {
            let path = resolved_path(workspace_root, &args.path);
            file_lock(FileScope::Exact(path), AccessMode::Read)
        }
        (BuiltinTool::List, ValidatedArgs::List(args)) => {
            let path = resolved_path(workspace_root, &args.path);
            file_lock(FileScope::Recursive(path), AccessMode::Read)
        }
        (BuiltinTool::Search, ValidatedArgs::Search(args)) => {
            let path = resolved_path(workspace_root, &args.path);
            file_lock(FileScope::Recursive(path), AccessMode::Read)
        }
        (BuiltinTool::Edit, ValidatedArgs::Edit(args)) => {
            let path = resolved_path(workspace_root, &args.path);
            file_lock(FileScope::Exact(path), AccessMode::Write)
        }
        (BuiltinTool::Write, ValidatedArgs::Write(args)) => {
            let path = resolved_path(workspace_root, &args.path);
            file_lock(FileScope::Exact(path), AccessMode::Write)
        }
        (BuiltinTool::Bash, _) => ToolAccess::WorkspaceUnknown,
        // update_plan 是原生同步控制操作（§13）：不进入普通调度队列。
        (BuiltinTool::UpdatePlan, _) | (BuiltinTool::WebSearch, _) | (BuiltinTool::WebFetch, _) => {
            ToolAccess::Pure
        }
        _ => ToolAccess::Pure,
    }
}

fn file_lock(scope: FileScope, mode: AccessMode) -> ToolAccess {
    ToolAccess::Resources(vec![ResourceLock {
        resource: ResourceId::File(scope),
        mode,
    }])
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
    pub tool: BuiltinTool,
    pub args: ValidatedArgs,
    pub access: ToolAccess,
    /// §12.3：ActionKey = hash(tool_name + canonical_json(args))。
    pub action_key: String,
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
    // serde_json 的 Map 默认有序（BTreeMap），canonical 序列化可复现。
    let canonical = serde_json::from_str::<serde_json::Value>(args_json)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| args_json.to_string());
    let digest = blake3::hash(format!("{}|{canonical}", tool.name()).as_bytes());
    digest.to_hex()[..16].to_string()
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
            let store = crate::util::lock_mutex(&ctx.snapshot_store, "snapshot_store");
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
