# ADR-007：Durable Async Subagent Runtime（multi-agent delegation）

- 状态：**Proposed（草案，待落地确认）**
- 关联：`docs/architecture.md` §7 Suspend/Resume、crates/tpi-runtime、crates/tpi-agent、crates/tpi-session
- 替代：ADR-006 的"parent cancellation"模型继续有效，本 ADR 在其上扩展 agent 生命周期的协作语义

---

## 0. 目标

把 TPI 的子代理从"**工具执行**"升级为"**runtime-managed resource**"：

> Process 是 managed resource，Terminal 是 managed resource，Agent 也应该是 managed resource；
> Subagent tool 只负责**控制 Agent resource**，而不负责执行完整的 child 生命周期。

核心行为变化：

```text
（现状：同步）
subagent(task) ──── await 整个 child agent::run ────▶ report   [主代理阻塞]

（目标：异步）
spawn_agent(task) ──▶ { agent_id: A17, status: running }       [几十毫秒返回]
  │
  ├── 主代理继续：修改 UI / 跑测试 / 继续调用模型
  └── child A17 后台跑 → 完成 → Parent Inbox → durable event → 下一个 model boundary 注入
```

**判定标准**：`spawn_agent` 在"注册完成"后就返回（毫秒级），**不等待 agent 完成**；主代理的 turn 不再因一个子代理而卡住。

---

## 1. 概念模型

### 1.1 三个分离的身份

```rust
AgentId      // = 谁在工作：一个被托管的 agent 实例（复用现有 id_type!）
DelegationId // = 为什么让它工作：一次委托（spawn）的记录
```

同一 Agent 未来可被多次委托（`Delegation #101 → Agent A`、`Delegation #123 → Agent A`）。

### 1.2 关键不变量（贯穿所有实现）

> **异步事件只允许在 AgentLoop 的 deterministic boundary 被消费。**
> **先 durable，再 model-visible。**

- 子代理 report 不允许直接修改某个**正在运行的** Context。
- report 先进入 **Parent Inbox**（易失唤醒通道），同时落盘为 **durable SessionEvent**。
- 只在下一个 **Model Request boundary**（`build_context`）把 report 注入 parent 的 model 可见上下文。
- **不污染进行中的 turn**（规避 Codex #14318 的 wrong-turn injection 竞态）。

---

## 2. 数据模型

### 2.1 新 id 类型（`tpi-core`）

```rust
id_type!(AgentId);
id_type!(DelegationId);
```

### 2.2 Agent 状态机

```rust
enum AgentState {
    Starting,     // 已注册，child task 尚未进入 Loop
    Running,      // child AgentLoop 执行中
    // （同 parent 一致的协作终态）
    Stopped,      // 正常完成（有 report 或 settlement）
    Failed,       // 不可恢复失败
    Cancelled,    // 被 cancel / 父 session 关闭
}
```

### 2.3 AgentHandle / Agent 记录（AgentManager 内）

```rust
struct AgentRecord {
    agent_id: AgentId,
    state: AgentState,
    /// child 的独立 session id（child 拥有自己的 SessionLog）。
    child_session: SessionId,
    /// 委托来源（供 Trace/Audit/ContextProjection 使用）。
    origin: ParentTraceContext,       // parent trace/span
    /// worker JoinHandle + cancel —— worker 生命周期由 AgentManager 拥有。
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    /// 最后已知状态/进度的朗视投影（跨 run 存活）。
    last_view: AgentView,
    notify: Arc<Notify>,              // 完成/状态变化唤醒 wait
}
```

### 2.4 Delegation（一次委托的记录）

```rust
struct Delegation {
    id: DelegationId,
    parent_session: SessionId,   // parent session（report 的落盘目标）
    parent_run: RunId,           // origin run —— WakeIfIdle 的 gate
    child_agent: AgentId,
    child_session: SessionId,
    state: DelegationState,
    report: Option<SubagentReport>, // 终态结果（completed 才有）
}
enum DelegationState { Running, Reported, Settled }
```

`DelegationState` 与 `AgentState` 的区分：
- `Report` = **语义结果**（child 侧给出的 summary/evidence）
- `Settled` = **runtime truth**（AgentLoop 达到终态，即使没有 report 也要 settled）

---

## 3. AgentManager（session 级共享注册表）

镜像 `ProcessRegistry` / `TerminalRegistry` 的既有模式，作为 `SessionRuntime` 的一个新字段：

```rust
pub struct SessionRuntime<P: Provider> {
    /* 现有字段 */
    pub agents: Arc<StdMutex<AgentManager>>,   // 新增
}

pub struct AgentManager {
    order: Vec<AgentId>,
    agents: HashMap<AgentId, AgentRecord>,
    delegations: HashMap<DelegationId, Delegation>,
    notify: Arc<Notify>,
}
```

### 3.1 API（AgentManager 方法）

```rust
impl AgentManager {
    // spawn：注册 agent + 启动 worker task（几十毫秒）；不 await 完成
    fn spawn(&mut self, request: SpawnRequest) -> AgentId;

    // 终态消息落盘后的 inbox 写入（跨 run 存活、给 parent 和 wait 唤醒）
    fn report(&mut self, delegation: DelegationId, report: SubagentReport);

    // 查询
    fn list(&self) -> Vec<AgentView>;
    fn status(&self, agent_id: AgentId) -> Option<AgentView>;

    // 控制
    fn cancel(&mut self, agent_id: AgentId);       // cancel token
    fn close(&mut self, agent_id: AgentId);        // cancel + 清理记录

    // 唤醒（wait/wake 用）
    fn wait(&mut self, agent_id: AgentId) -> WaitGuard; // 挂起直到 notify/终态
}
```

### 3.2 生命周期归属

遵循"谁创建谁清理"（AGENTS.md）。**AgentManager 拥有 worker task 的生命周期**：

```text
AgentManager::spawn
  ├─ 分配 AgentId
  ├─ 创建 child 独立 session（SessionLog::create_with_id）
  ├─ 创建只读 registry（reuse read_only_registry(capabilities)）
  ├─ 绑定 parent report_tx / output_tx（实时观察）
  ├─ tokio::spawn(child worker)
  └─ 返回 agent_id（立即）
```

Worker task 内部复用现有 `InProcessChildProvider::run_investigation` 的核心执行逻辑，
但**改动为不返回给调用者，而是自报终态**：

```text
child worker（独立 task）
  ├─ agent::run(...)                 // child 自己的 Loop
  ├─ outcome → 组装 SubagentReport
  ├─ 落盘 durable event：SubagentReported / SubagentSettled (parent session)
  ├─ AgentManager.report(...)        // mailbox 写入 + notify
  └─ 结束（AgentState → Stopped/Settled）
```

### 3.3 默认状态保留（V1 语义）

- 默认 `context mode = Fresh`（child 只拿任务指令 + 只读能力，无 parent 历史）。
- **V1 只读**（child 白名单 read/search/glob）。
- 允许同一 assistant step **并行 spawn 多个 child**（每个独立 AgentId，互不阻塞）。
- `depth = 1`（child 不递归）。

---

## 4. Durable SessionEvent（先 durable，再 model-visible）

以下事件**落盘到 parent session 的事件流**（JSONL 唯一事实源）：

```rust
enum SessionEvent {
    // ...现有变体

    /// 一次子代理委托已注册（child 后台开始工作）。
    SubagentSpawned {
        delegation_id: DelegationId,
        agent_id: AgentId,
        child_session: SessionId,
        instruction: String,          // 任务摘要（诊断/重放）
        capabilities: Vec<String>,    // 只读白名单快照
    },

    /// child 给出了语义 report（可能多条 progress / 一条 final）。
    SubagentReported {
        delegation_id: DelegationId,
        agent_id: AgentId,
        child_session: SessionId,
        summary: String,
        evidence: Vec<String>,
        final_report: bool,           // false=progress，true=首个 final
    },

    /// AgentLoop 达到终态（runtime truth；可能无 report）。
    SubagentSettled {
        delegation_id: DelegationId,
        agent_id: AgentId,
        child_session: SessionId,
        reason: SubagentFinishedReason,  // Stopped / Failed / Cancelled / ...
        // 终态 report（若有过 final report，也在此冗余一份以简化重放）
        report: Option<SubagentReport>,
    },
}
```

> 每个变体需同步更新：`type_name()`、`EventBody`、payload struct、`Envelope::new`、`Envelope::to_session_event`（protocol.rs）。参考现有 13 变体的一次性机械改动。

### 4.1 为什么落盘到 parent session 而不是 child session

child 已有**独立** session（隔离上下文）；但它**向 parent 报告**这一事实属于 parent 的
授权委托历史，应在 parent 事件流里可审计、可重放。child 自身 session 任保留完整 child
trace（供深度查看）。

### 4.2 report 进 parent context（deterministic boundary）

`build_context` 在每个 model request 前构建（`mod.rs:1575`）。新增一个 source：

```text
Session facts
  ├── conversation ...
  ├── plan snapshot ...
  ├── managed processes snapshot ...
  └── [NEW] pending subagent reports snapshot（来自 parent session 事件流）
        → 未消费的 SubagentReported/Settled，聚合为一条 system 消息
        → 只覆盖"自上次消费后新增"的 report
```

**消费语义（决策 2026-08：durable watermark）**：report 只在进入某次 activation 的
确定性输入边界之后、且该边界**成功进入 provider 请求**时，才推进"已投影"游标。

- **durable cursor 是真相**，以 **parent session 中的 mailbox watermark / checkpoint** 形式
  持久化（一个轻量字段，不是每消费一条 report 写一个事件）。
- **内存 cursor 只是缓存**（读取加速），可丢弃；durable cursor 才是真相。
- cursor 仅当消息**成功进入某次 activation 的确定性输入边界后**才推进（即已真正进入
  provider request，而非仅仅被读进构建缓冲区）。

约束：
- 不污染进行中 turn（只在 boundary 注入）；
- 幂等（同事件只注入一次，cursor 防止重复）；
- resume 时从 durable watermark 重建 pending 集合（新进程无内存缓存，直接从事件流 +
  durable watermark 算出尚未投影的 report）。

---

## 5. Parent Inbox（易失唤醒通道）

Inbox 是 AgentManager 内的易失状态（跨 run 存活，同 processes/terminals 生命周期）：

```rust
struct ParentInbox {
    /// 已落盘、但尚未投影到任何 parent model context 的 report。
    pending: VecDeque<InboxEntry>,
    /// report 到达时唤醒（WakeIfIdle / wait 用）。
    notify: Arc<Notify>,
}
struct InboxEntry {
    delegation_id: DelegationId,
    agent_id: AgentId,
    summary: String,
    evidence: Vec<String>,
    final_report: bool,
}
```

- **写入**：child worker 落盘 durable event 后，`AgentManager.report()` 写 inbox + notify。
- **读取**：AgentManager 查询（list/status）与 `build_context` 的 report snapshot 均从 inbox 读。
- **inbox 与 durable 的关系**：inbox 是**易失缓存**（加速 + 唤醒），durable event 是**真相**。
  进程崩溃后 inbox 丢失，但 report 仍可从事件流重建（进 context 的部分不受影响）。

---

## 6. Report 交付策略

```rust
enum ReportDelivery {
    NextBoundary,   // 只在下一个 model request 注入
    WakeIfIdle,     // 默认：parent 空闲时触发一次新 turn（携带 OriginRunId 校验）
    Quiet,          // 只进 inbox，等 parent 自然读取
}
```

- 默认 **WakeIfIdle**。
- 但**仅当** `delegation.origin_run` 仍属于可继续 workflow 时才 wake parent（避免
  DeepSeek 的"parent 永远醒不过来"）。origin run 已结束且无可续边界 → 只进 inbox，
  TUI 显示 `Agent A completed`，等模型下一次自然运行读取。

---

## 7. 主代理 turn 不再阻塞：`spawn_agent` 如何返回

关键改动点在 `ToolBatchExecutor`：**新增** `spawn_agent`（注册类操作，毫秒级返回），
**保留现有同步 `subagent` 工具并存**（决策 1：新增保留并存，不改既有调用与测试）。

- 现状：`subagent`（同步）的 `join_all(futures)` 同步等全部工具——保留不动。
- 目标：`spawn_agent` 的 execute 只调 `AgentManager::spawn`（同步锁内注册 + `tokio::spawn`，
  本身不 await child），立刻返回 `{ agent_id, status: running }` 作为 `ChatMessage::Tool`。
- 它**不进入 ReadOnly 并行 wave 的阻塞等待**（注册本身极快；作为独立操作类）。

这样，`spawn_agent` 在 wave 里与其它工具一样"瞬时完成"，主 agent 的 turn 照常继续，
不受 child 是否完成影响。

### 7.1 新增工具集（V1 final 形态）

| 工具 | 语义 | 阻塞? |
|---|---|---|
| `spawn_agent` | 注册 + 启动 child，返回 `agent_id` | 否（毫秒） |
| `list_agents` | 列出本 session 所有 agent 与状态 | 否 |
| `agent_status` | 查询单个 agent 状态 / pending report | 否 |
| `wait_agent` | 阻塞直到 agent 终态或有 report（响应 cancel） | 是（用户显式要求依赖时用） |
| `cancel_agent` | 取消一个 agent | 否 |

> `wait_agent` 进 V1（决策 2），但仅用于**显式依赖**场景（"我接下来必须依赖这个结果"）。
> 正常提示词应教育模型：Spawn early → 继续母工作 → 在 model boundary 消费已完成 report。
> **不要让 wait 成为标准工作流**（否则模型又 wait A+B，主代理仍 FULL BLOCKED）。

---

## 8. TUI / RuntimeEvent 扩展

`LiveEvent` 新增/扩展（带 delegation/agent 身份，杜绝 wrong-turn）：

```rust
enum LiveEvent {
    // 现有 SubagentReported { child_session, summary, evidence } → 扩展为带 delegation_id
    AgentSpawned {
        agent_id: AgentId, delegation_id: DelegationId, instruction: String,
    },
    AgentBuilding {
        agent_id: AgentId, text: String,      // child 活动转发（reuse ToolOutputDelta 通道亦可）
    },
    AgentReported {
        agent_id: AgentId, delegation_id: DelegationId, summary, evidence, final_report,
    },
    AgentTerminated {
        agent_id: AgentId, delegation_id: DelegationId, reason: ..., report: Option<...>,
    },
}
```

对应 `RuntimeEvent`（TUI view event）映射 + reducer 渲染：
- `AgentSpawned` → 创建 agent 卡片（running）
- `AgentReported` → 卡片进度 / final summary
- `AgentTerminated` → 终端态

同时修复既有接线缺口：生产 `report_tx` 目前是 `None`（`register_openai_subagent_tool`），
`LiveEvent::SubagentReported` 实际不发。本 ADR 落地时，agent 的实时观察通道改为经
AgentManager（session 级）注入，一并在运行时接通（不再依赖注册期的 None 通道）。

---

## 9. 与现有架构的一致性

- **Session is truth**：report/settle 落盘为 durable event（parent session）。
- **Context is projection**：report 只在 `build_context` boundary 投影，不污染运行中 turn。
- **Whovever creates an effect owns its cleanup**：AgentManager 拥有 worker task；
  worker 终态自报（settle），cancel 传播（复用 CancellationToken）。
- **AgentLoop stays thin**：新增的是 AgentManager（resource manager）+ 事件 + 工具；
  AgentLoop 核心逻辑不变（复用现有 run）。
- **ProcessSupervisor/TerminalSupervisor 同构 的 AgentSupervisor**：agent 是第三种
  被管理的 resource，但**生命周期不简单等于 parent run**（跨 run 存活，session 级别）。

### 9.1 Scope（生命周期范围）

```rust
enum AgentScope { Run, Session }
```

- **RunScoped**：未来可选项——parent run cancel → child cancel。
- **SessionScoped（V1 默认）**：parent turn/run 结束，child 继续；parent session 关闭 → child cancel。
- 不做 Detached/ApplicationScoped（易产生 orphan agent）。

AgentManager 生命周期绑定 **session**（`SessionRuntime.agents`），与 processes/terminals 一致。

---

## 10. 多 agent 写竞争（V2+，本 ADR 不实现）

V1 保持只读，天然无跨 agent 写冲突。V2 起：

```text
Subagent Write = global WorkspaceMutation lock → isolated worktree → merge/reconcile
```

本 ADR 只保证 V1 只读，不引入跨 Agent 写调度（未来独立 ADR）。

---

## 11. 落地顺序（分 commit，每步都是最终架构一部分，无过渡物）

> 全程不保留"注定删除"的临时架构；每 commit 是最终模型的子集。

### Commit 1 — 数据与事件层（纯增量，无行为改变）
- `id_type!(AgentId)`, `id_type!(DelegationId)`（tpi-core/ids.rs）
- `SubagentReport` 归入 tpi-session（作为事件 payload 用）
- `SessionEvent` 新增 4 变体 + `type_name` + `EventBody` + payload + `Envelope::new/to_session_event`
- 测试：round-trip 序列化、type_name、golden hash 不破坏现有事件

### Commit 2 — AgentManager（session 级共享注册表）
- `tpi-capabilities`（或 tpi-agent）新增 `AgentManager` + `AgentRecord` + `Delegation`,
  复用 ProcessRegistry 模式（order/hash/cancel/notify）
- `SessionRuntime.agents`（tpi-runtime）
- 纯逻辑单测（spawn 注册 / list / status / cancel / state 转移 / 上限）

### Commit 3 — 非阻塞工具集
- `spawn_agent` / `list_agents` / `agent_status` / `wait_agent` / `cancel_agent` 工具
  （Wire InputSchema + execute 只操作 AgentManager，本身不阻塞）
- **新增** `SpawnAgentTool` + `AgentControlTool`（或一个工具多命令），**保留现有同步
  `subagent` 工具并存**（决策 1：不删不替代，两者 namespace 不同，无冲突）
- registry 接线：composition root 注入 `AgentManager`

### Commit 4 — worker 终态自报到 ParentInbox + durable event
- child worker 复用 `InProcessChildProvider` 执行，改为 worker task 内完成
  落盘(report event) + AgentManager.report() + notify
- 实时观察通道经 AgentManager（修复 report_tx=None 缺口）
- cancel 传播 / session 关闭清理

### Commit 5 — context projection（deterministic boundary 注入）
- `build_context` 注入 pending subagent report snapshot（system 消息）
- **durable watermark**（parent session 的 mailbox checkpoint）+ 内存缓存；
  cursor 仅当消息成功进入某次 activation 的确定性输入边界后才推进
- 测试：不污染进行中 turn、幂等注入、resume 重建

### Commit 6 — TUI / LiveEvent / RuntimeEvent
- LiveEvent 扩展（AgentSpawned/Reported/Terminated）
- RuntimeEvent 投影 + reducer 卡片
- 端到端测试

### Commit 7 — 验证
- cargo build + cargo test + clippy + fmt
- 真实异步 spawn 场景（多个 child 并行，主 turn 不阻塞，report 在 boundary 注入）

---

## 12. 非目标（本 ADR 明确不做）

- Fork context（从 parent 历史创建 child）→ 未来
- continuable agent / nested subagent → 未来
- child 写 workspace / 跨 agent 写竞争 → V2+
- wait 成为默认工作流 → 明确不鼓励
- orphan/detached agent → 明确避免

---

## 13. 参考

- Codex: `multi_agents_spec.rs`（spawn/send/wait/mailbox 模型）
- Codex #14318（completion 污染错误 turn）——本 ADR 用 boundary 注入规避
- Codex #15723（后台 agent 完成不唤醒 parent）——本 ADR 用 WakeIfIdle + OriginRunId gate
- DeepSeek Harness：continuable-core report obligation / report delivery=wakeup / spawn vs fork
- 本仓库：docs/architecture.md §7、docs/adr/ADR-006、crates/tpi-capabilities/src/process/managed.rs