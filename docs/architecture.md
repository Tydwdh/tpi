# TPI Architecture

> 本文件解释**为什么这样设计**（AGENTS.md §26），不是目录说明。
> 吸收 DeepSeek Harness 的边界设计、Cordis 的生命周期思想，但**不照搬**
> Everything-is-Plugin / reactive coeffect（见 §「不吸收」）。

核心五句话：

> **Session is truth.**
> **Context is projection.**
> **AgentLoop stays thin.**
> **ToolExecutor hides complexity.**
> **Whoever creates an effect owns its cleanup.**

---

## 1. Kernel Boundary（核心边界）

```
                         TUI
                          │ events / commands
                          ▼
┌────────────────────────────────────────────┐
│              Agent Runtime                 │
│                                            │
│   AgentLoop → Context Projector            │
│        │                                   │
│        ▼                                   │
│   ToolExecutor（ToolBatchExecutor）        │
│        │                                   │
│        ▼                                   │
│   ToolRegistry（builtin / MCP / future）    │
│                                            │
│────────────────────────────────────────────│
│            Session Event Store             │
│             source of truth                │
└────────────────────────────────────────────┘
```

Kernel = AgentLoop / Session / Context / ToolExecutor / ToolRegistry。这些是**稳定的小核心**；
MCP、Skills、LSP、Web、Subagent 都是 extension。AgentLoop 不直接碰 MCP 协议、
文件系统细节、sandbox、context summary 或 TUI 状态。

## 2. Session Model（Session is truth）

`SessionEvent`（src/session/mod.rs）是 durable 事件的**唯一集合**，JSONL envelope
（schema/seq/event_id/timestamp），append-only。

事件类型（核心子集）：

```text
UserSubmitted / RunStarted / AssistantMessageCommitted /
AssistantAttemptInterrupted / ToolRequested / ToolStarted / ToolCompleted /
PlanReplaced / CompactionCommitted / RunCompleted
```

所有派生视图都是投影：TUI 历史、对话 transcript、模型 context、工具执行历史、
recovery 状态、resume/replay/fork——全部从事件流重建，**不维护五套互相同步的状态**。

不变量：**Model-visible ⇒ Durable**。任何进入模型请求的信息都必须能从 session 重建。
compaction 是显式 `CompactionCommitted` 事件（覆盖范围 + 摘要），不是偷偷删历史。

## 3. Context Projection（Context is projection）

Context ≠ Session ≠ Conversation。Context 是**某一次模型调用时的临时视图**，
从 session facts + 系统配置 + token budget 构造（`build_context` + `context/mod.rs`）：

```text
Session facts → estimate tokens → deterministic prune（只影响投影）→ Model request
```

compaction 是独立策略（保留最近完整消息 + 旧 tool 输出 prune + 摘要），
不在 AgentLoop 里写死 `if tokens > max { summarize }`。

## 4. Tool Execution（ToolExecutor 深模块）

`ToolBatchExecutor`（src/agent/tool_runtime.rs）是 AGENTS.md §十的 **ToolExecutor 深模块**：
模型 turn 状态机只提供 calls 与预算计数；以下复杂性全部隐藏在其内部：

```text
pre-check all args → build waves（tool::scheduler 纯函数）
→ write-ahead（ToolStarted / RecoveryMetadata，崩溃恢复）
→ execute wave（并行 Pure/Read，串行 Write/WorkspaceUnknown）
→ observe（无进展检测）→ persist（ToolCompleted）
→ refill results by source index
```

调度原语（资源声明、waves、action_key、ProgressTracker）物理上在
**src/tool/scheduler.rs**（tool 领域纯函数，无 IO / session / provider 依赖）。
tool_runtime 在 waves 之上编排执行、持久化与 UI 通知。

## 5. Tool Provider（capability seam）

`Tool` trait + `ToolRegistry`（src/tool/registry.rs）：

```rust
trait Tool { fn name/description/input_schema/origin; async fn execute(...) }
```

AgentLoop **不关心工具来源**：builtin / MCP / 未来 plugin 都注册进同一 registry，
`ToolOrigin` 只作 metadata（runtime_inspect 展示），绝不作为执行分支依据。

## 6. Lifecycle Ownership（谁创建谁清理）

RAII 版本的反向副作用（Cordis revertible effect 的 Rust 形态）：

- `ToolRegistry::register_owned(&Arc<Mutex<Registry>>, tool)` 返回 **`ToolRegistration`**
  handle；`drop(registration)` 自动从 registry 移除该工具。
- `McpManager::RunningServer` 持有每个 server 的 `Vec<ToolRegistration>`；
  server 重启/关闭 = drop handle = 工具自动注销——**不再按名字前缀扫描**。

```
注册工具 → drop scope → 工具消失（契约测试：raii_registrations_unregister_on_shutdown）
```

MCP server 生命周期（spawn/initialize/tools/list/call/shutdown）全部在 mcp/ 模块；
ToolRegistry 只拿到适配后的 `Tool`。

## 7. Suspend / Resume（等待用户 ≠ run 完成）

模型需要用户决定时调用 **`request_input`** 工具（不是"以提问结束 run"）：

```text
Running
  → request_input 工具执行（Succeeded）
  → batch 返回 SuspendRequested（ToolRequested/ToolCompleted 已持久化）
  → agent 记录 UserInputRequested + RunCompleted(AwaitingUserInput)
  → RunState 挂起，控制权交回 TUI（显示问题）
用户回答
  → app 层记录 UserInputReceived（durable 事实）
  → 普通 run 继续（UserSubmitted 投影为 User 消息，完整历史）
```

`CompletionReason::AwaitingUserInput` 是独立状态：不是失败、不是取消。
事件对（Requested/Received）完整保留，resume/replay 语义连续。

## 8. Runtime Introspection

`runtime_inspect` 工具（src/tool/inspect.rs）是 runtime 能力的只读投影：
当前工具目录（含 origin）、已发现 skills、workspace kind/identity、managed processes。
Agent 不靠 system prompt 猜"我有什么能力"，Runtime 才是事实来源。
（model/provider 由会话配置决定，见 /settings。）

## 9. Skill Boundary（Skill ≠ Tool ≠ MCP）

- **MCP** = Capability / Tool Provider（通过 registry 进入 AgentLoop）。
- **Skill** = Knowledge / Procedure / Workflow（SKILL.md，progressive disclosure；
  经 `activate_skill` 激活注入 context，**不是工具**）。

两者正交，不揉进同一个 registry。

## 10. TUI Boundary

TUI 是 **event consumer + command producer**，不拥有 Agent 状态：
键盘/鼠标 → UiEvent → reducer（纯状态转换）→ effect → draw；
Agent 事件（RuntimeEvent）经同一 reducer 投影。
TUI 可以从 session 重建（--continue/--resume 还原历史与运行状态）。

---

## 不吸收（明确不做）

- **Everything is Plugin**：AgentLoop / Session / Context 不是插件；核心保持稳定。
- **Reactive Coeffect**：tpi 用 trait / Arc / registry / RAII / 显式生命周期满足需求，
  不引入隐式依赖图。
- **Self-modifying Harness**：Agent 写插件 → compile → load 属实验性，非核心需求。
- 这些可能未来有用 → 记入 docs/future-design.md，不污染当前实现。

## 当前已知边界

- `agent::scheduler` 是 `tool::scheduler` 的 re-export（兼容既有测试引用），
  测试引用迁移完成后删除（AGENTS.md §27 清理）。
- remote（SSH）workspace 与 MCP 的 ToolContext 构造已统一；`request_input` 的
  options 字段目前仅作提示，未实现结构化选择。
