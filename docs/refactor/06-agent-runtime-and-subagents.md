# 06. Agent Runtime、Inbox、Supervisor 与子代理

## 1. Runtime 的职责边界

Agent runtime 负责：

- Run/Turn/Step 状态机；
- inbox claim 与 message sequence；
- context assembly 调用；
- provider stream 规范化后的消费；
- 工具 pipeline 协调；
- budget/cancel/terminal reason；
- durable fact 与 live event 的正确提交顺序。

它不负责：

- terminal 输入和颜色布局；
- slash command；
- 具体 provider HTTP/SSE；
- 文件/SSH/MCP 实现；
- tool card target/diff/tail 格式；
- app 如何弹出 approval/input overlay。

## 2. Public API

目标 public handle 可参考：

```rust
trait AgentHandle {
    async fn send(&self, message: DomainMessage) -> Result<RunReceipt, AgentError>;
    async fn steer(&self, message: DomainMessage) -> Result<InboxReceipt, AgentError>;
    async fn follow_up(&self, message: DomainMessage) -> Result<InboxReceipt, AgentError>;
    async fn answer(&self, request: InputRequestId, answer: UserAnswer) -> Result<(), AgentError>;
    async fn cancel(&self, target: CancelTarget) -> Result<(), AgentError>;
    fn subscribe(&self) -> RuntimeEventStream;
    async fn when_idle(&self) -> Result<(), AgentError>;
    async fn shutdown(&self) -> Result<(), ShutdownError>;
}
```

是否用 trait/object 按 consumer 决定；核心是所有操作返回 receipt/明确错误，不能把发送到 channel 当作任务已经执行。

### 2.1 Receipt

`RunReceipt`/`InboxReceipt` 至少携带 identity、accepted_at、queue class、durability status。UI 可以立即显示“已接收/已排队”，随后由 runtime event 更新 claimed/running/terminal。

## 3. Inbox 语义

### 3.1 两种队列

- **Steering / next-step**：当前 Step 的工具完成后、下一次模型请求前 claim；用于纠正正在运行的任务。
- **Follow-up / next-turn**：当前 Run/Turn 自然结束后再启动；用于“做完这个，再做那个”。

`answer(request_id, ...)` 是第三种专门通道，不进入普通 prompt 队列。它必须匹配当前 `AwaitingInput` request ID。

### 3.2 Queue policy

```text
max count + max bytes + max message bytes
```

满队列默认拒绝新消息并返回 `QueueFull`，不静默丢旧消息。若产品决定保留“丢最旧”，必须让 caller 明确 opt-in，并 durable/visible 记录被丢 identity。安全起见，approval answer 永不因普通队列满被丢。

### 3.3 Claim 时序

1. Step 开始前冻结 capability/model/context policy snapshot。
2. claim 当前所有已接受 steering（按序）和至多一个 primary queued prompt，规则写入测试。
3. durable append claimed/inserted facts（若消息将 model-visible）。
4. build context。
5. 发 model request。
6. 完成该 response 的全部 tool terminal。
7. 若有 steering，回 2；否则按 finish reason 决定继续/结束。
8. Turn 自然完成后取 follow-up，启动新 Turn/Run policy 所定义的边界。

### 3.4 Cancel/abort

- cancel active run 不默认丢队列；queued steering/followup 返回 UI composer 或保留，产品行为必须显式。
- 当前 Pi 的“Esc 把 queued message 恢复 editor”很人性化；TPI 可由 app 将 unclaimed messages 映射回 composer，但 runtime 只报告 release/discard，不知道 editor。
- cancellation terminal 必须和 wall time/provider error 区分。
- cancel 不能只停止 model stream；必须传播至工具、子进程和 child agent。

## 4. Supervisor

### 4.1 结构

```text
ApplicationSupervisor (root token/tracker)
  +-- AgentSupervisor
  |     +-- RunSupervisor
  |     |     +-- ProviderAttempt
  |     |     +-- ToolWave/ToolInvocation
  |     |     +-- ProgressForwarder
  |     +-- ChildAgentSupervisor(s)
  +-- McpSupervisor(s)
  +-- UiSupervisor
```

每一层使用 child `CancellationToken` 和 `TaskTracker`。任务 panic 变成 owner 的 structured failure；不能只有 tracing 后继续显示 running。

### 4.2 Shutdown protocol

```text
mark stopping
close intake
cancel children
wait tracked tasks (bounded by policy, not arbitrary sleep)
terminate/kill remaining owned OS process tree if required
commit interruption/terminal facts
flush session
dispose registrations/resources
publish disposed
```

超时是 shutdown policy 的最后手段，不是竞态修复。发生强制 kill 时记录哪些 resource 未优雅结束。

### 4.3 Tool output forwarder 修复目标

当前 forwarder abort 后不 join。迁移时：

1. handler senders 全 drop；
2. receiver drain 到 close；
3. coalescer flush 最后一帧；
4. await forwarder；
5. 如果 run cancel，forwarder 仍有短暂 flush budget；
6. terminal event 只能在最后 delta 已处理/明确 dropped 后发送。

## 5. Provider 与 streaming

### 5.1 Normalized stream

```rust
enum ModelStreamEvent {
    ReasoningDelta(TextChunk),
    TextDelta(TextChunk),
    ToolCallDelta { index, ... },
    Usage(UsageDelta),
    Warning(ProviderWarning),
}
```

adapter 返回 stream handle + terminal result；Agent 不提供一个 UI mpsc sender给 provider。这样 headless 不需要 drain task。

### 5.2 Retry/continuation

- 未收到任何语义 event 前的 transport failure 可按 provider policy retry；
- 收到 text/tool delta 后不能重发同请求假装无副作用；使用 interrupted attempt +明确 continuation/recovery；
- tool identity/name/index 中途改变是 protocol error；
- `[DONE]`、finish reason、usage 的 provider 差异在 adapter 内；
- cancellation 要打断 backoff、connect、body read 和 stream assembly；
- response/argument/delta byte 数有上限。

现有 provider contract tests 应抽成 `ModelAdapterConformance`，fake clock/network fault 可重复。

## 6. Budget 模型

不要只用 max turns/tool calls。目标预算：

```rust
struct RunBudget {
    max_steps: u32,
    max_tool_calls: u32,
    max_wall_time: Option<Duration>,
    max_input_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
    max_cost: Option<Money>,
    max_children: u32,
    max_child_depth: u8,
    max_artifact_bytes: u64,
}
```

不是所有 provider 都能精确报告 cost；未知时用 token/请求上限，UI 标注 estimate。预算消耗是单调事实，不能因 retry/resume 重置。child 获得 parent 划拨的子预算，未用额度如何返还由 policy 明确。

## 7. 子代理为什么要作为 capability

子代理解决四个真实场景：

1. **隔离上下文**：大规模调查不污染主对话；
2. **并行只读探索**：互不依赖的仓库区域同时分析；
3. **专长/模型路由**：不同模型或 agent 处理 UI、安全、测试；
4. **外部 Agent 互操作**：通过 ACP/subprocess 使用其他成熟 harness。

它不应被用来：

- 掩盖主 Agent 无法分解任务；
- 无限递归 spawn；
- 让多个 child 无协调地编辑同一 workspace；
- 规避权限/预算；
- 默认把所有子对话塞回 parent context；
- 实现不可审计的“自主 swarm”。

## 8. Subagent capability contract

```rust
trait SubagentProvider {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn start(
        &self,
        spec: ChildSpec,
        ctx: ChildLaunchContext,
    ) -> Result<ChildHandle, ChildStartError>;
}

struct ChildSpec {
    purpose: String,
    prompt: DomainMessage,
    mode: ChildMode,              // Fresh | Fork(parent head)
    model: Option<ModelSelector>,
    capability_profile: CapabilityProfile,
    workspace_mode: WorkspaceMode,
    budget: ChildBudget,
    output_contract: ReportSchema,
}
```

`ChildHandle` 支持 subscribe/status/cancel/wait/shutdown，terminal 返回：

```rust
struct ChildReport {
    status: ChildTerminalStatus,
    summary: String,              // 有界，给 parent model
    findings: Vec<StructuredFinding>,
    artifacts: Vec<ArtifactRef>,
    changes: WorkspaceChangeSet,
    usage: Usage,
    diagnostics: Vec<DiagnosticRef>,
    child_session: SessionRef,
}
```

## 9. 子代理隔离与并发

### 9.1 Session/context

- 默认 fresh child session，只继承显式 system/project context 和 task prompt；
- fork 模式继承 parent 某个 committed head，不继承 ephemeral UI/runtime state；
- child 完整 transcript 留在 child session；parent 只接收 structured report；
- child context compaction 与 parent 独立；
- parent/child lineage durable 可追踪。

### 9.1.1 Parent/child trace propagation

parent 和 child 共享 durable lineage，但不伪造成一个无限大的同步调用栈：

- parent `spawn_agent` span 记录 `child_id`、provider、budget、capability/workspace snapshot digest；
- child run 默认创建自己的 `trace_id`，通过 span link 指向 parent spawn/request span；
- parent session 和 child session 各自保持单调 seq；不能用 arrival time 猜跨 session 全序；
- report commit 记录 child terminal span/session head 的 reference；UI 可从 summary 下钻独立 trace；
- cancel/timeout/shutdown 必须沿 parent token 传播，并在两侧分别产生 terminal outcome；
- 外部 ACP/subprocess 不能传播原生上下文时，至少传 opaque correlation ID，并把断点标为 `remote_boundary`，不能伪造完整度。

只读 child 的发布门禁包括：从 parent command 到 child terminal/report commit 的因果链可查询，100 次 start/cancel 后 ownership tree 无 orphan。字段、link 和 gap 语义见 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)。

### 9.2 Capabilities

child capability snapshot 是 parent policy 允许集合的子集。默认：

- 调查型 child：read/search/list/LSP/web（按 policy），无 write/process；
- 实现型 child：可在隔离 worktree/分支写；
- 外部 agent：按其协议实际能力标注，不能假装 TPI 能约束内部执行。

### 9.3 Workspace 模式

| 模式 | 用途 | 风险控制 |
|---|---|---|
| SharedReadOnly | 并行调查 | runtime 拒绝 write/process-mutating tools |
| SharedCoordinated | 小型串行实现 | central scheduler 按 path/resource lock；默认不开放 |
| IsolatedWorktree | 并行代码修改 | 独立 git worktree/branch；合并由 parent/user 显式执行 |
| RemoteSandbox | 不可信/高风险任务 | 外部隔离环境；能力/数据传输明确 |

TPI 不能把线程或 in-process child 称为安全沙箱。DeepSeek workflow worker 也明确不是 security boundary；本文沿用该原则。

### 9.4 并发与公平

- root 有 global semaphore；默认同时运行 child 不超过 4，具体值配置且受机器/供应商限制；
- 每个 parent 有 fanout/depth 上限；
- queued child 不预先占用 provider/tool resources；
- interactive parent 的事件和工具优先级不被批量 child 饿死；
- provider rate limit budget 可按 key/model共享；
- child 完成顺序不改变 parent 任务输入顺序，除非显式 `completion_order` strategy。

## 10. 子代理交互

child 若 `AwaitingInput`：

1. 默认不能直接抢占主 composer；
2. child event 上报 parent supervisor；
3. policy 可选择：parent 代答、升级给用户、取消 child、使用预授权默认；
4. 用户 overlay 显示 child identity/purpose/question；
5. answer 带 child + input request IDs；
6. parent cancel 立即取消 pending interaction；
7. 超时是明确 terminal/blocked，不 busy loop 重问。

## 11. 子代理发布阶段

### Phase A：只读单 child

- `SubagentProvider` + fake provider；
- in-process fresh session；
- 无递归、无 write、并发 1；
- structured report；
- cancellation/terminal/session tests；
- 默认 feature/config off。

### Phase B：有界并行调查

- concurrency semaphore；
- 多 task source-order report；
- UI child tree；
- output/artifact cap；
- provider rate/fairness metrics。

### Phase C：隔离实现

- worktree/sandbox adapter；
- change manifest/diff artifact；
- parent 只审阅/合并，不让 child 偷偷写主树；
- cleanup/recovery for worktrees。

### Phase D：外部 provider

- 选择 ACP client 或 subprocess JSON-RPC；
- handshake/version/capability negotiation；
- disconnect/reconnect/partial report；
- trust UI；
- 与 in-process provider 跑同一 conformance suite。

### Phase E：递归（只有真实需求时）

- depth <= 2 的起始硬上限；
- budget tree；
- cycle/lineage checks；
- total descendants cap；
- emergency root cancel。

没有 Phase A/B 的观测数据，不进入 C–E。

## 12. TUI 表示

主 transcript 默认只显示：

```text
[child: security-review] running · 1m12s · 2/6 tool calls
[child: tui-review] completed · 4 findings · open details
```

展开 overlay/side panel 才显示 child stream/tool cards。状态颜色和 icon 不能是唯一信息。支持：cancel one、cancel all、jump child session、copy report、查看预算/权限。

## 13. 测试矩阵

- inbox insert/claim/discard/release 顺序属性测试；
- answer 永不进入普通 prompt；重复/过期 request ID 拒绝；
- cancel at every await point；
- supervisor shutdown 无 task/process/registration 泄漏；
- child setup 任一 fault point rollback；
- parent cancel propagates；child terminal 不反向 cancel parent；
- source-order vs completion-order；
- budget never increases；resume/retry 不重置；
- shared read-only 无写 bypass；
- worktree cleanup recoverable；
- report schema/size validation；恶意外部 report；
- provider disconnect after partial stream；
- depth/fanout/rate limit；
- 100 次 start/cancel loop 后 task/process/handle 数回到 baseline。

## 14. 明确的非目标

第一轮子代理不会提供：任意 DAG workflow UI、自治角色市场、自动合并多 child 代码、无监督长期 daemon、跨机器分布式队列、自己安装插件/修改 policy。只有具体用户场景、威胁模型和验收指标出现后再新增 capability。
