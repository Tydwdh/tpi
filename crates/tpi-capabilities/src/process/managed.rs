//! Managed Background Process（任务书 §1-§73）。
//!
//! 与 Bash/ShellSession 的边界（§70）：
//! - Command 有明确结束点；Process 是 TPI 拥有的长期资源；
//! - ShellSession = cwd/env；ProcessRegistry = long-running processes；
//! - ProcessId 是 TPI 逻辑身份（`p{n}`），不是 OS PID（PID 会复用、Local/Remote
//!   语义不同；真实 PID 只作内部 metadata）；
//! - Output telemetry（live tail）lossy/bounded；durable output 在 artifact。
//!
//! 本模块只做 domain model 与纯状态转换（P1，§55）；进程启动/输出/取消的
//! 执行侧在 `start_background`（P2+，接 process-host + Job Object）。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::Job;
use crate::tool::command::RunArgs;
use tpi_session::artifact::ArtifactWriter;

/// live tail 预算（任务书 §16：第一层 bounded ring/tail，建议 64 KiB）。
pub const LIVE_TAIL_BUDGET: usize = 64 * 1024;

/// 同时运行进程上限（任务书 §47：资源有限；避免模型反复启动泄漏）。
pub const MAX_MANAGED_PROCESSES: usize = 16;

/// 逻辑进程 ID（任务书 §7：`p17`、`p18`；不是 PID）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId(u64);

static NEXT_PROCESS_NUMBER: AtomicU64 = AtomicU64::new(1);

impl ProcessId {
    /// 分配下一个逻辑 ID（session 内单调递增，不回绕）。
    pub fn next() -> Self {
        Self(NEXT_PROCESS_NUMBER.fetch_add(1, Ordering::Relaxed))
    }

    pub fn number(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "p{}", self.0)
    }
}

impl std::str::FromStr for ProcessId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some(number) = s.strip_prefix('p') else {
            return Err(format!("invalid process id: {s}"));
        };
        let number = number
            .parse::<u64>()
            .map_err(|_| format!("invalid process id: {s}"))?;
        Ok(Self(number))
    }
}

/// ManagedProcess 核心状态（任务书 §6）。
///
/// timeout / cancel / connection_lost 与程序退出状态分离：不伪装成
/// `exit_code != 0`。`Unknown` 用于取消/终止结果无法确认（§22 不伪造成功）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagedProcessState {
    /// 已注册、drain task 正在启动 target。
    Starting,
    /// target 已 spawn 成功，正在运行。
    Running,
    /// target 正常退出（无论 exit code 是否非零）。
    Exited { exit_code: i32 },
    /// spawn 失败 / host 异常（进程未运行或运行状态不可知）。
    Failed,
    /// 显式取消（Job tree 已终止）。
    Cancelled,
    /// 结束方式无法确认。
    Unknown,
}

impl ManagedProcessState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Exited { .. } | Self::Failed | Self::Cancelled | Self::Unknown
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited { .. } => "exited",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// 一个 TPI 拥有的后台进程（任务书 §8：不是 `HashMap<Id, Child>`——
/// Remote Process 未来复用同一逻辑模型，因此这里只存逻辑状态与快照，
/// 运行时句柄（Job/pid/取消 token）由执行侧另行持有）。
#[derive(Debug, Clone)]
pub struct ManagedProcess {
    pub id: ProcessId,
    /// Workspace identity（`local:...` / `ssh:host:root`，任务书 §8）。
    pub workspace: String,
    /// 用户命令原文（诊断/展示用）。
    pub command: String,
    /// 启动时 cwd 快照（§9：process creation 时继承，之后不受 session 影响）。
    pub cwd: String,
    /// 启动时 env overlay 快照（§9：只继承，绝不反向修改 ShellSessionState）。
    pub env: HashMap<String, String>,
    pub state: ManagedProcessState,
    pub exit_code: Option<i32>,
    pub started_at: Instant,
    /// 结束时间（未结束时为 None）。
    pub finished_at: Option<Instant>,
    /// live tail（stdout+stderr 合并，bounded，§16 第一层）。
    pub tail: Vec<u8>,
    /// 已消费输出总字节（stdout+stderr；诊断用，不随 tail 截断归零）。
    pub total_bytes: u64,
    /// full artifact 引用（`@artifact/<session>/<id>`，完成时设置，§16 第二层）。
    pub artifact: Option<String>,
    /// 真实 OS PID（内部 metadata，任务书 §7：不作为主要身份暴露）。
    pub pid: Option<u32>,
}

impl ManagedProcess {
    pub fn new(
        id: ProcessId,
        workspace: String,
        command: String,
        cwd: String,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            id,
            workspace,
            command,
            cwd,
            env,
            state: ManagedProcessState::Starting,
            exit_code: None,
            started_at: Instant::now(),
            finished_at: None,
            tail: Vec::new(),
            total_bytes: 0,
            artifact: None,
            pid: None,
        }
    }

    /// 当前状态文本（status 工具 / snapshot 用）。
    pub fn status_line(&self) -> String {
        let runtime = match self.finished_at {
            Some(end) => end.duration_since(self.started_at),
            None => self.started_at.elapsed(),
        };
        match self.state {
            ManagedProcessState::Exited { exit_code } => format!(
                "{} exited {}  {}  {:.1}s",
                self.id,
                exit_code,
                self.command,
                runtime.as_secs_f64()
            ),
            ManagedProcessState::Starting => {
                format!(
                    "{} starting  {}  {:.1}s",
                    self.id,
                    self.command,
                    runtime.as_secs_f64()
                )
            }
            ManagedProcessState::Running => {
                format!(
                    "{} running   {}  {:.1}s",
                    self.id,
                    self.command,
                    runtime.as_secs_f64()
                )
            }
            ManagedProcessState::Cancelled => {
                format!(
                    "{} cancelled {}  {:.1}s",
                    self.id,
                    self.command,
                    runtime.as_secs_f64()
                )
            }
            ManagedProcessState::Failed => {
                format!(
                    "{} failed    {}  {:.1}s",
                    self.id,
                    self.command,
                    runtime.as_secs_f64()
                )
            }
            ManagedProcessState::Unknown => {
                format!(
                    "{} unknown   {}  {:.1}s",
                    self.id,
                    self.command,
                    runtime.as_secs_f64()
                )
            }
        }
    }
}

/// ProcessRegistry（任务书 §8）：统一持有所有 ManagedProcess。
///
/// 内存有界：live tail 有界（[`LIVE_TAIL_BUDGET`]）、进程数有上限
/// （[`MAX_MANAGED_PROCESSES`]）、完整输出在 artifact 不在 registry。
///
/// 运行时侧表：`cancels`（每个进程的取消 token，drain task 与 process cancel
/// 共享同一 token）；`notify`（进程结束唤醒 process wait）。
pub struct ProcessRegistry {
    /// 按插入顺序记录 ProcessId（保持 p 号展示顺序稳定）。
    order: Vec<ProcessId>,
    processes: HashMap<ProcessId, ManagedProcess>,
    cancels: HashMap<ProcessId, CancellationToken>,
    notify: Arc<Notify>,
}

/// 保留的进程条目总数上限（ISSUE-005：每个终态进程留存 tail≤64KiB +
/// env 快照，长期会话无界增长会耗尽内存）。超过后淘汰**最早的终态**进程
/// （active 进程永不淘汰，`process status` 仍可查询最近进程）。
pub const MAX_RETAINED_PROCESSES: usize = 64;

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self {
            order: Vec::new(),
            processes: HashMap::new(),
            cancels: HashMap::new(),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个新进程（Starting）。达到上限返回 Err（§47：资源有限）。
    pub fn insert(&mut self, process: ManagedProcess) -> Result<(), String> {
        if self.active_count() >= MAX_MANAGED_PROCESSES {
            return Err(format!(
                "managed process limit reached ({MAX_MANAGED_PROCESSES} running); cancel an existing process first"
            ));
        }
        let cancel = CancellationToken::new();
        self.order.push(process.id);
        self.cancels.insert(process.id, cancel);
        self.processes.insert(process.id, process);
        // 新条目可能把总量推过上限（大量终态进程时），顺带淘汰最旧终态。
        self.evict_terminal();
        Ok(())
    }

    pub fn get(&self, id: ProcessId) -> Option<&ManagedProcess> {
        self.processes.get(&id)
    }

    pub fn get_mut(&mut self, id: ProcessId) -> Option<&mut ManagedProcess> {
        self.processes.get_mut(&id)
    }

    /// 按插入顺序遍历（列表展示稳定）。
    pub fn iter(&self) -> impl Iterator<Item = &ManagedProcess> {
        self.order.iter().filter_map(|id| self.processes.get(id))
    }

    /// 仍在运行的进程数（Starting/Running）。
    pub fn active_count(&self) -> usize {
        self.processes
            .values()
            .filter(|p| !p.state.is_terminal())
            .count()
    }

    /// 追加输出到 live tail（bounded；§15/§16）。
    pub fn append_output(&mut self, id: ProcessId, bytes: &[u8]) {
        let Some(process) = self.processes.get_mut(&id) else {
            return;
        };
        process.total_bytes = process.total_bytes.saturating_add(bytes.len() as u64);
        append_bounded(&mut process.tail, bytes, LIVE_TAIL_BUDGET);
    }

    /// 状态迁移（terminal 后不允许再变，除非显式 reset——本阶段无 reset）。
    pub fn transition(
        &mut self,
        id: ProcessId,
        state: ManagedProcessState,
        exit_code: Option<i32>,
    ) -> bool {
        let Some(process) = self.processes.get_mut(&id) else {
            return false;
        };
        if process.state.is_terminal() {
            return false;
        }
        process.state = state;
        process.exit_code = exit_code;
        if state.is_terminal() {
            process.finished_at = Some(Instant::now());
            // 终态后立即腾出空间（先完成时间戳再淘汰，保证淘汰逻辑可读）。
            self.evict_terminal();
        }
        true
    }

    /// ISSUE-005：淘汰最早的终态进程，直到条目总数 ≤ [`MAX_RETAINED_PROCESSES`]。
    /// active（Starting/Running）进程永不淘汰；`process status` 仍可查询最近
    /// 终态进程。淘汰同时释放 tail/env 内存与取消 token。
    fn evict_terminal(&mut self) {
        while self.processes.len() > MAX_RETAINED_PROCESSES {
            // order 按插入序：找第一个终态进程淘汰（头部是 active 时向后找）。
            let Some(&victim) = self.order.iter().find(|id| {
                self.processes
                    .get(id)
                    .map(|p| p.state.is_terminal())
                    .unwrap_or(true)
            }) else {
                break; // 全部 active：无法淘汰（上限检查在 insert 已保证 active 数）。
            };
            self.remove_entry(victim);
        }
    }

    /// 移除一个进程的全部记录（order/processes/cancels）。
    fn remove_entry(&mut self, id: ProcessId) {
        self.order.retain(|existing| *existing != id);
        self.processes.remove(&id);
        self.cancels.remove(&id);
    }

    /// 设置运行时 metadata（真实 pid / artifact 引用）。
    pub fn set_runtime(&mut self, id: ProcessId, pid: Option<u32>, artifact: Option<String>) {
        let Some(process) = self.processes.get_mut(&id) else {
            return;
        };
        if pid.is_some() {
            process.pid = pid;
        }
        if artifact.is_some() {
            process.artifact = artifact;
        }
    }

    /// 注入上下文的紧凑快照（§25：只含 active + 近期发生状态变化尚未消费的进程）。
    /// 调用方在每次展示后调用 [`ProcessRegistry::mark_consumed`]。
    pub fn snapshot_lines(&self, consumed: &[ProcessId]) -> Vec<String> {
        let mut lines = Vec::new();
        for process in self.iter() {
            let recent = process
                .finished_at
                .is_some_and(|end| end.elapsed() < std::time::Duration::from_secs(60))
                && !consumed.contains(&process.id);
            if !process.state.is_terminal() || recent {
                lines.push(process.status_line());
            }
        }
        lines
    }

    /// 标记一批进程的状态已向模型展示（§25：避免上下文膨胀）。
    /// 内存回收不依赖本方法——终态进程由 [`Self::evict_terminal`] 在超过
    /// [`MAX_RETAINED_PROCESSES`] 时按最旧淘汰（本方法保持 no-op，只依赖
    /// snapshot 的 60s 展示窗口）。
    pub fn mark_consumed(&mut self, ids: &[ProcessId]) {
        let _ = ids;
    }

    /// 请求取消一个进程：取消 drain task 的 token，由 drain task 执行
    /// TerminateJobObject 并迁移到 Cancelled（§22）。返回 false = 进程不存在
    /// 或已结束。
    pub fn cancel(&self, id: ProcessId) -> bool {
        let Some(token) = self.cancels.get(&id) else {
            return false;
        };
        let Some(process) = self.processes.get(&id) else {
            return false;
        };
        if process.state.is_terminal() {
            return false;
        }
        token.cancel();
        true
    }

    /// drain task 结束时清理取消 token（进程已 terminal）。
    pub fn remove_cancel(&mut self, id: ProcessId) {
        self.cancels.remove(&id);
    }
}

/// bounded append（复用前台进程层语义，§16 第一层）。
fn append_bounded(buffer: &mut Vec<u8>, bytes: &[u8], budget: usize) {
    if bytes.len() >= budget {
        buffer.clear();
        buffer.extend_from_slice(&bytes[bytes.len() - budget..]);
    } else {
        let total = buffer.len() + bytes.len();
        if total > budget {
            buffer.drain(..total - budget);
        }
        buffer.extend_from_slice(bytes);
    }
}

/// 后台启动请求（P2）：由 bash 工具 background 分支构造。
pub struct BackgroundStartRequest {
    /// 启动规格（program/args/cwd/env/env_remove，与前台同一注入路径）。
    pub args: RunArgs,
    /// launcher 标记（`git-bash`）。
    pub launcher: Option<&'static str>,
    /// Workspace identity 文本（`local:...`，任务书 §8）。
    pub workspace: String,
    /// 用户命令原文（诊断/展示）。
    pub command: String,
    pub artifacts_root: std::path::PathBuf,
    pub session_id: String,
    /// Optional workspace snapshot owned by the drain task. It is committed
    /// only after the child reaches a terminal state, so long-running jobs do
    /// not bypass the mutation journal.
    pub workspace_tracker: Option<crate::workspace::tracked::TrackedWorkspace>,
    /// registry 由整个 session 共享（Arc<Mutex>）；drain task 与 process 工具
    /// 通过它更新/读取状态。
    pub registry: Arc<std::sync::Mutex<ProcessRegistry>>,
}

/// 启动后台进程（P2，任务书 §56）：
/// 注册 Starting → spawn drain task → 等待 host 的 MSG_STARTED 确认，
/// 在明显短于命令本身的时间返回逻辑 ProcessId。
///
/// 启动确认窗口 3 秒：收到 MSG_STARTED → 返回 `Ok(id)`（进程 running）；
/// spawn 失败（host 发 Exit(-2)）→ `Err`；超时乐观返回（进程仍 Starting，
/// 模型可用 process status 查询）。
pub async fn start_background(request: BackgroundStartRequest) -> Result<ProcessId, String> {
    let BackgroundStartRequest {
        args,
        launcher,
        workspace,
        command,
        artifacts_root,
        session_id,
        workspace_tracker,
        registry,
    } = request;
    let id = ProcessId::next();
    let process = ManagedProcess::new(id, workspace, command, args.cwd.clone(), args.env.clone());
    {
        let mut reg = tpi_core::util::lock_mutex(&registry, "process_registry");
        reg.insert(process).map_err(|error| {
            format!("status: failed\nprocess: {id}\nerror: process_limit\n\n{error}")
        })?;
    }
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    spawn_drain(
        BackgroundStartRequest {
            args,
            launcher,
            workspace: String::new(),
            command: String::new(),
            artifacts_root,
            session_id,
            workspace_tracker,
            registry: registry.clone(),
        },
        id,
        started_tx,
    );
    match tokio::time::timeout(Duration::from_secs(3), started_rx).await {
        Ok(Ok(Ok(()))) => Ok(id),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) => Err("process drain task exited before confirming start".into()),
        Err(_) => Ok(id), // 超时：乐观返回（Starting；status 可查）。
    }
}

/// spawn 后台 drain task：独立 Job Object + 持续读帧（输出永不阻塞，§15）。
fn spawn_drain(
    request: BackgroundStartRequest,
    id: ProcessId,
    started_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    tokio::spawn(async move {
        let BackgroundStartRequest {
            args,
            launcher: _,
            workspace: _,
            command: _,
            artifacts_root,
            session_id,
            workspace_tracker,
            registry,
        } = request;
        if let Err(error) = drain_loop(
            &args,
            &registry,
            id,
            started_tx,
            &artifacts_root,
            &session_id,
            workspace_tracker,
        )
        .await
        {
            tracing::error!(process = %id, %error, "managed process drain failed");
            {
                let mut reg = tpi_core::util::lock_mutex(&registry, "process_registry");
                reg.transition(id, ManagedProcessState::Failed, None);
                reg.notify.notify_waiters();
            }
        }
        tpi_core::util::lock_mutex(&registry, "process_registry").remove_cancel(id);
    });
}

/// 单进程 drain 主循环（每个 ManagedProcess 一个独立 Job Object，§12/§13）。
///
/// 与前台 `run_in_host` 的差异：无 timeout（§45/§46：background 无默认短
/// timeout）、取消经共享 token、MSG_STARTED 确认启动、输出持续写入
/// live tail + artifact、Job 生命周期由本 task 持有直到结束。
async fn drain_loop(
    args: &RunArgs,
    registry: &Arc<std::sync::Mutex<ProcessRegistry>>,
    id: ProcessId,
    started_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    artifacts_root: &std::path::Path,
    session_id: &str,
    mut workspace_tracker: Option<crate::workspace::tracked::TrackedWorkspace>,
) -> Result<(), String> {
    let cancel = {
        let reg = tpi_core::util::lock_mutex(registry, "process_registry");
        reg.cancels
            .get(&id)
            .cloned()
            .ok_or_else(|| "managed process cancel token missing".to_string())?
    };
    // 独立 Job Object（KILL_ON_JOB_CLOSE，禁 breakaway——继承现有隔离语义）。
    let job = Job::create().map_err(|error| format!("create job: {error}"))?;
    let exe = std::env::var_os("TPI_PROCESS_HOST")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "process-host executable unavailable".to_string())?;
    let mut host = tokio::process::Command::new(&exe)
        .arg("__process-host")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(super::create_no_window_flag())
        .spawn()
        .map_err(|error| format!("spawn process-host: {error}"))?;
    job.assign_process(
        host.id()
            .ok_or_else(|| "process-host pid unavailable".to_string())?,
    )
    .map_err(|error| format!("assign host to job: {error}"))?;

    let mut stdin = host
        .stdin
        .take()
        .ok_or_else(|| "host stdin unavailable".to_string())?;
    let mut stdout = host
        .stdout
        .take()
        .ok_or_else(|| "host stdout unavailable".to_string())?;
    if let Some(host_stderr) = host.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(host_stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(line = %line, "process-host stderr (managed)");
            }
        });
    }

    // 发送 Start spec（framed：len + kind + payload）。
    let spec = serde_json::json!({
        "program": args.program,
        "args": args.args,
        "cwd": args.cwd,
        "env": args.env,
        "env_remove": args.env_remove,
    });
    let payload = serde_json::to_vec(&spec).map_err(|error| format!("spec json: {error}"))?;
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| "managed process start spec exceeds protocol limit".to_string())?;
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&payload_len.to_le_bytes());
    header[4] = super::MSG_START;
    stdin
        .write_all(&header)
        .await
        .map_err(|error| format!("write spec: {error}"))?;
    stdin
        .write_all(&payload)
        .await
        .map_err(|error| format!("write spec payload: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("flush spec: {error}"))?;
    drop(stdin);

    let mut artifact = ArtifactWriter::create(artifacts_root, session_id, "process", "text/plain")
        .map_err(|error| format!("create artifact: {error}"))?;
    let mut started: Option<tokio::sync::oneshot::Sender<Result<(), String>>> = Some(started_tx);
    let mut confirm = |result: Result<(), String>| {
        if let Some(tx) = started.take() {
            let _ = tx.send(result);
        }
    };
    let mut terminated = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled(), if !terminated => {
                // §22：TerminateJobObject 杀掉整棵后台进程树（含 host）。
                job.terminate(1).map_err(|error| format!("terminate process tree: {error}"))?;
                terminated = true;
            }
            read = super::read_frame(&mut stdout) => {
                match read {
                    Ok(Some((super::MSG_STARTED, payload))) => {
                        let pid = if payload.len() == 4 {
                            payload[..4].try_into().ok().map(u32::from_le_bytes)
                        } else {
                            None
                        };
                        {
                            let mut reg = tpi_core::util::lock_mutex(registry, "process_registry");
                            reg.set_runtime(id, pid, None);
                            reg.transition(id, ManagedProcessState::Running, None);
                        }
                        confirm(Ok(()));
                    }
                    Ok(Some((super::MSG_OUTPUT, payload))) if !payload.is_empty() => {
                        let stream = payload[0];
                        let bytes = &payload[1..];
                        if stream == super::STREAM_STDOUT || stream == super::STREAM_STDERR {
                            artifact
                                .write(
                                    if stream == super::STREAM_STDOUT { "stdout" } else { "stderr" },
                                    bytes,
                                )
                                .map_err(|error| format!("write artifact: {error}"))?;
                            tpi_core::util::lock_mutex(registry, "process_registry")
                                .append_output(id, bytes);
                        }
                    }
                    Ok(Some((super::MSG_EXIT, payload))) if payload.len() == 4 => {
                        let code = i32::from_le_bytes(payload[..4].try_into().expect("len=4"));
                        let spawn_failed = code == -2;
                        {
                            let mut reg = tpi_core::util::lock_mutex(registry, "process_registry");
                            if spawn_failed {
                                reg.transition(id, ManagedProcessState::Failed, None);
                            } else {
                                reg.transition(
                                    id,
                                    ManagedProcessState::Exited { exit_code: code },
                                    Some(code),
                                );
                            }
                        }
                        if spawn_failed {
                            confirm(Err("process failed to start (spawn error)".into()));
                        } else {
                            confirm(Ok(())); // 极快退出：仍是“启动成功且已结束”。
                        }
                        break;
                    }
                    Ok(Some((_, _))) => { /* 忽略未知帧 */ }
                    Ok(None) => break, // EOF：host 已退出
                    Err(error) => {
                        tracing::warn!(error = %error, "managed process read error");
                        break;
                    }
                }
            }
        }
    }
    let _ = host.kill().await;
    let _ = host.wait().await;
    // 循环外终态补丁：取消 → Cancelled；host 异常退出（无 Exit）→ Failed。
    {
        let mut reg = tpi_core::util::lock_mutex(registry, "process_registry");
        if !reg.get(id).is_some_and(|p| p.state.is_terminal()) {
            reg.transition(
                id,
                if terminated {
                    ManagedProcessState::Cancelled
                } else {
                    ManagedProcessState::Failed
                },
                None,
            );
        }
    }
    let artifact_ref = artifact
        .finish()
        .ok()
        .map(|record| format!("@artifact/{session_id}/{}", record.id));
    if let Some(reference) = artifact_ref {
        tpi_core::util::lock_mutex(registry, "process_registry").set_runtime(
            id,
            None,
            Some(reference),
        );
    }
    if let Some(tracker) = workspace_tracker.as_mut()
        && let Err(error) = tracker.commit(artifacts_root, session_id)
    {
        // The process is already terminal; preserve its real lifecycle state
        // and record the journal failure as a high-severity diagnostic rather
        // than silently pretending the workspace remained observable.
        tracing::error!(process = %id, %error, "managed process workspace journal failed");
    }
    // 唤醒 process wait（§20）。
    tpi_core::util::lock_mutex(registry, "process_registry")
        .notify
        .notify_waiters();
    Ok(())
}

/// process wait（任务书 §20）：最多等 `timeout`；完成 → 返回终态，
/// 仍运行 → 返回当前状态（running/starting），都不是错误。
/// 进程不存在 → None。
pub async fn wait_process(
    registry: &Arc<std::sync::Mutex<ProcessRegistry>>,
    id: ProcessId,
    timeout: Duration,
) -> Option<ManagedProcessState> {
    let deadline = Instant::now() + timeout;
    loop {
        let notify = {
            let reg = tpi_core::util::lock_mutex(registry, "process_registry");
            match reg.get(id) {
                Some(process) if process.state.is_terminal() => return Some(process.state),
                Some(_) => reg.notify.clone(),
                None => return None,
            }
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return tpi_core::util::lock_mutex(registry, "process_registry")
                .get(id)
                .map(|p| p.state);
        }
        let _ = tokio::time::timeout(remaining.min(Duration::from_millis(100)), notify.notified())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: ProcessId, command: &str) -> ManagedProcess {
        ManagedProcess::new(
            id,
            "local:test".into(),
            command.into(),
            "C:/proj".into(),
            HashMap::from([("FOO".into(), "1".into())]),
        )
    }

    #[test]
    fn process_id_display_and_parse_round_trip() {
        let id = ProcessId::next();
        let text = id.to_string();
        assert!(text.starts_with('p'), "id 文本必须是 p{{n}}: {text}");
        assert_eq!(text.parse::<ProcessId>().unwrap(), id);
        assert!("p17".parse::<ProcessId>().unwrap().number() == 17);
        assert!("17".parse::<ProcessId>().is_err(), "缺 p 前缀必须拒绝");
        assert!("pabc".parse::<ProcessId>().is_err(), "非数字必须拒绝");
        assert!("p-1".parse::<ProcessId>().is_err());
    }

    #[test]
    fn state_lifecycle_start_run_exit() {
        let mut registry = ProcessRegistry::new();
        let id = ProcessId::next();
        registry.insert(sample(id, "sleep 5")).unwrap();
        assert_eq!(
            registry.get(id).unwrap().state,
            ManagedProcessState::Starting
        );

        assert!(registry.transition(id, ManagedProcessState::Running, None));
        assert_eq!(
            registry.get(id).unwrap().state,
            ManagedProcessState::Running
        );

        assert!(registry.transition(id, ManagedProcessState::Exited { exit_code: 0 }, Some(0)));
        assert_eq!(
            registry.get(id).unwrap().state,
            ManagedProcessState::Exited { exit_code: 0 }
        );
        assert!(registry.get(id).unwrap().finished_at.is_some());
        assert!(registry.get(id).unwrap().state.is_terminal());
        // terminal 之后不允许再迁移（状态机守恒）。
        assert!(!registry.transition(id, ManagedProcessState::Running, None));
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn state_cancel_and_unknown_are_terminal_and_separate() {
        let mut registry = ProcessRegistry::new();
        let id = ProcessId::next();
        registry.insert(sample(id, "server")).unwrap();
        assert!(registry.transition(id, ManagedProcessState::Cancelled, None));
        assert!(registry.get(id).unwrap().state.is_terminal());
        assert_eq!(registry.active_count(), 0);

        let id2 = ProcessId::next();
        registry.insert(sample(id2, "wget")).unwrap();
        assert!(registry.transition(id2, ManagedProcessState::Unknown, None));
        assert_eq!(
            registry.get(id2).unwrap().state,
            ManagedProcessState::Unknown,
            "Unknown 是独立状态，不得伪装成 exit_code != 0（§6/§22）"
        );
    }

    #[test]
    fn registry_enforces_running_cap() {
        let mut registry = ProcessRegistry::new();
        for _ in 0..MAX_MANAGED_PROCESSES {
            registry
                .insert(sample(ProcessId::next(), "sleep 5"))
                .unwrap();
        }
        let err = registry.insert(sample(ProcessId::next(), "extra"));
        assert!(err.is_err(), "超过上限必须拒绝（§47）");
        // 结束一个后可继续注册。
        let first = registry.order[0];
        registry.transition(first, ManagedProcessState::Exited { exit_code: 0 }, Some(0));
        registry.insert(sample(ProcessId::next(), "extra")).unwrap();
    }

    #[test]
    fn live_tail_is_bounded_and_counts_total() {
        let mut registry = ProcessRegistry::new();
        let id = ProcessId::next();
        registry.insert(sample(id, "spam")).unwrap();
        let chunk = vec![b'x'; 4096];
        for _ in 0..100 {
            registry.append_output(id, &chunk);
        }
        let process = registry.get(id).unwrap();
        assert!(
            process.tail.len() <= LIVE_TAIL_BUDGET,
            "live tail 必须 bounded"
        );
        assert_eq!(process.total_bytes, 4096 * 100, "总字节不随截断归零");
        // tail 保留的是最后一段。
        assert!(process.tail.iter().all(|b| *b == b'x'));
    }

    #[test]
    fn transition_to_missing_id_returns_false() {
        let mut registry = ProcessRegistry::new();
        assert!(!registry.transition(ProcessId::next(), ManagedProcessState::Running, None));
    }

    #[test]
    fn status_line_reports_state_and_runtime() {
        let mut registry = ProcessRegistry::new();
        let id = ProcessId::next();
        registry.insert(sample(id, "python server.py")).unwrap();
        registry.transition(id, ManagedProcessState::Running, None);
        let line = registry.get(id).unwrap().status_line();
        assert!(line.contains(&id.to_string()));
        assert!(line.contains("running"));
        assert!(line.contains("python server.py"));
    }

    /// ISSUE-005：终态进程超过 [`MAX_RETAINED_PROCESSES`] 时淘汰最旧的终态，
    /// active 进程永不淘汰；内存（tail/env/取消 token）随之释放。
    #[test]
    fn issue_005_evicts_oldest_terminal_but_keeps_active() {
        let mut registry = ProcessRegistry::new();
        // 塞满上限。
        let mut active = ProcessId::next();
        for i in 0..MAX_RETAINED_PROCESSES {
            let id = ProcessId::next();
            registry.insert(sample(id, &format!("proc {i}"))).unwrap();
            registry.transition(id, ManagedProcessState::Exited { exit_code: 0 }, Some(0));
            if i == 0 {
                active = id;
            }
        }
        assert_eq!(registry.processes.len(), MAX_RETAINED_PROCESSES);

        // 再插一个终态进程：最旧终态被淘汰，总量保持上限。
        let newest = ProcessId::next();
        registry.insert(sample(newest, "newest")).unwrap();
        registry.transition(
            newest,
            ManagedProcessState::Exited { exit_code: 0 },
            Some(0),
        );
        assert_eq!(
            registry.processes.len(),
            MAX_RETAINED_PROCESSES,
            "总量必须被限制"
        );
        assert!(registry.get(newest).is_some(), "最新进程必须保留");
        assert!(registry.get(active).is_none(), "最早的终态进程必须被淘汰");
        // 淘汰的进程不再占用取消 token / order 条目。
        assert!(!registry.cancels.contains_key(&active));
        assert!(!registry.order.contains(&active));

        // active 进程永不淘汰：把第一个进程保持 Running，其余终态塞满。
        let mut registry2 = ProcessRegistry::new();
        let running = ProcessId::next();
        registry2.insert(sample(running, "keep running")).unwrap();
        registry2.transition(running, ManagedProcessState::Running, None);
        let mut first_terminal = None;
        for i in 0..MAX_RETAINED_PROCESSES + 1 {
            let id = ProcessId::next();
            registry2.insert(sample(id, &format!("term {i}"))).unwrap();
            registry2.transition(id, ManagedProcessState::Exited { exit_code: 0 }, Some(0));
            if first_terminal.is_none() {
                first_terminal = Some(id);
            }
        }
        assert!(registry2.get(running).is_some(), "active 进程不得被淘汰");
        assert_eq!(registry2.active_count(), 1);
        assert_eq!(
            registry2.processes.len(),
            MAX_RETAINED_PROCESSES,
            "总量仍受上限约束"
        );
        assert!(
            registry2.get(first_terminal.unwrap()).is_none(),
            "最旧的终态被淘汰"
        );
    }
}
