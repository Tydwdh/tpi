# 02. 目标架构与不可变契约

## 1. 架构风格

目标是“模块化单体 + 极小微内核 + ports/adapters + 显式 composition root”。它不是微服务，也不是运行时依赖注入框架。

迁移分两步：

1. **逻辑边界**：先在当前 crate 内建立模块和 dependency gate。
2. **物理边界**：稳定后拆 Cargo workspace，减少错误 feature/visibility 设计。

### 1.1 最终建议 workspace

```text
crates/
  tpi-core/          IDs、消息/内容、状态、命令、live event、错误分类
  tpi-session/       durable event codec/store/projection/recovery
  tpi-capabilities/  tool/provider/workspace/subagent contracts、registry、policy
  tpi-agent/         Agent machine、inbox、context assembly、supervisor
  tpi-adapters/      OpenAI、MCP、local/SSH workspace、process、builtin tools
  tpi-tui/           UiModel、components、layout、render、input adapter
  tpi-cli/           config/auth/doctor/eval/application controller/composition root
```

如果 `tpi-adapters` 变成无关实现集合，再按真实编译/发布需要拆 `tpi-provider-openai`、`tpi-workspace-ssh` 等；不要提前创建 `common`、`utils`、`manager` crate。

### 1.2 允许的依赖

```text
A -> B 表示 A 依赖 B：

tpi-session      -> tpi-core
tpi-capabilities -> tpi-core
tpi-agent        -> tpi-core + tpi-session + tpi-capabilities
tpi-adapters     -> tpi-core + tpi-session + tpi-capabilities
tpi-tui          -> tpi-core
tpi-cli          -> tpi-agent + tpi-adapters + tpi-tui + tpi-session
```

更精确规则：

- core 不依赖 session IO、capability 实现、provider、TUI、platform。
- session 可依赖 core protocol values，不依赖 Agent 实现或 TUI。
- capabilities 只定义 contract/registry/policy，不依赖具体 adapter。
- agent 依赖 core、session port、capability ports；不依赖 Ratatui、Reqwest、Russh、MCP SDK。
- adapters 实现 ports；允许依赖外部 IO 库。
- TUI 只依赖 core 的 command/event DTO 和自己的 view model；不依赖 agent implementation/session file。
- CLI/app 是唯一组合所有模块的 root。

依赖 gate 必须由脚本从 `cargo metadata`/源码 import 生成并在 CI 检查，不能只靠本文。

## 2. 四类模型，不得混用

### 2.1 Domain model

表达用户/模型/工具交互的稳定语义：

```rust
enum ContentBlock {
    Text(String),
    Image(ImageRef),
    Artifact(ArtifactRef),
    ToolCall(ToolCall),
    ToolResult(ToolResultRef),
}

struct DomainMessage {
    id: MessageId,
    role: MessageRole,
    content: Vec<ContentBlock>,
    source: MessageSource,
}
```

这是示意，不要求按字面代码实现。关键是不能再把 OpenAI-compatible `ChatMessage` 当全系统消息模型。

### 2.2 Durable model

只记录恢复、审计、模型可见性和用户可见历史所需的已提交事实。它有 schema/version/codec 约束，不能包含未承诺长期兼容的 TUI 字段。

### 2.3 Runtime model

表达当前进程里的增量和生命周期，如 request started、assistant delta、tool progress、agent phase changed。它可丢失、可合并，不逐 token 写盘，但必须能与 durable terminal fact 对齐。

### 2.4 View model

表达 TUI 卡片、颜色、折叠、layout、focus、selection、tail、diff 渲染。由 application projector 从 domain/runtime/durable reading 产生。任何 view 字段都不能反向决定 durable 业务语义。

## 3. 统一词汇：Run、Turn、Step、Attempt

当前代码的 `turn`、run 和 provider retry 容易被不同模块理解为不同边界。目标定义：

- **Run**：一次 public `AgentHandle::send`/resume 触发的执行，直到 stop、await input、cancel、budget/fatal terminal。
- **Turn**：一个用户/steering 输入批次引出的连续工作；可包含多个 Step。
- **Step**：一次 model request + 该 response 要求的全部工具处理与结果回填。
- **Attempt**：同一 Step 内 provider transport retry/recovery 的一次网络尝试。

不变量：

1. 一个 Step 至多提交一个 assistant message carrier。
2. 工具结果必须属于产生它们的 assistant message/Step。
3. Attempt 中断可以 durable 记录诊断，但不能伪装成 committed assistant message。
4. steering 在当前 Step 的工具终态后 claim；followup 在当前 Turn 自然完成后 claim。
5. Run terminal 之前，所有已启动工具必须有 terminal outcome 或明确 interrupted recovery record。

## 4. 状态机

### 4.1 Agent public phase

```text
Idle
  | send/resume
  v
Running <----- steering queued
  |  \ cancel/fatal
  |   v
  | Stopping ----> Idle/Disposed
  |
  +-- request input --> AwaitingInput -- answer --> Running
                             | cancel
                             v
                            Idle

Any non-disposed state -- dispose --> Stopping --> Disposed
```

不要用 `is_running + pending_input + cancelled + has_result` 表达。状态转换函数返回 typed result；非法转换是 programmer error，在 debug/test 中 assert，在 public API 返回明确 error。

### 4.2 Tool call phase

```text
Proposed -> Validated -> Authorized -> Queued -> Started
              |            |                    /  |  \
          Rejected      Denied             Succeeded Failed Cancelled
                                                 \
                                                  Interrupted(recoverable)
```

不是所有中间态都必须 durable。durable 边界由 effect class 决定；对可能改变 workspace 的调用，Started/write-ahead 必须先于 effect。

### 4.3 Child agent phase

```text
Queued -> Starting -> Running -> Completed
              |         |  \-> AwaitingInput
              |         |  \-> Failed
              |         \----> Cancelled
              \--------------> Failed(setup)
```

父任务只在 child 到 terminal 且 structured report 已提交后消费结果。

## 5. Commands、Events、Effects

### 5.1 方向

```text
TerminalInput -> UiIntent -> AppCommand -> use case
                                      |
                                      +-> Domain/Runtime event -> projectors
                                      +-> AppEffect -> adapter (clipboard/open URL/etc.)
```

- `UiIntent`：按键、mouse action 已解释成“提交”“向上翻页”“关闭 overlay”。
- `AppCommand`：开始 run、取消、切 session、回答 input、更新 config。
- `RuntimeEvent`：正在发生的领域语义，不带颜色/行布局。
- `UiAction/UiEffect`：复制、打开 URL、terminal title 等 presentation/platform effect。

### 5.2 事件分层

| 类别 | 是否持久化 | 是否可丢/合并 | 例子 |
|---|---|---|---|
| `SessionEvent` | 是 | 否 | UserSubmitted、ToolCompleted、RunCompleted |
| `RuntimeEvent` | 否 | delta 可合并 | AssistantDelta、ToolProgress、PhaseChanged |
| `LifecycleSignal` | 否 | 否；局部调用链 | before tool、shutdown、registration disposed |
| `UiEvent` | 否 | resize/mouse move 可合并 | key、resize、tick、view action |

禁止把所有类别塞进一个 event bus。durable append 返回成功才允许投影声称事实已提交。

### 5.3 Capability/Event/Projection 闭包

这三个词不是重命名，而要满足可组合规则：

- Capability composition 生成一个有版本、不可变、可解析的 capability snapshot；新增 provider不修改核心来源 `match`。
- Event composition 生成有序 committed facts或有因果关系的live observations；event不命令外部世界。
- Projection 是 `(State, Event) -> State` 的确定转换；多个projector可消费同一事实且不互相修改。
- Command请求动作，Runtime解析Capability并拥有Effect；Effect完成后才能产生“已经发生”的terminal Event。
- Plugin只是注册一组Capability、projector、invariant和presentation consumer，并拥有它们的disposer。

因此 Subagent/MCP/Skill/Web不会消失，它们的公开复杂度被压入已有构件；预算、隔离、transport和清理仍由对应deep module拥有。

## 6. 核心 ports

只为现有或已批准 consumer 定义。以下是目标责任，不是要求一次提交全部 trait。

### 6.1 SessionStore

```text
open/create
append(expected_head, event) -> committed envelope
sync(barrier)
read_from(cursor)
recover/diagnose
close
```

写者 ownership、序号单调和 schema codec 在实现内部；Agent 不接触 `File`/JSON line。

### 6.2 ModelAdapter

```text
model metadata/capabilities
stream(ModelRequest, CancelToken) -> normalized stream + terminal response
```

context/message conversion由独立 assembler 执行。adapter 只处理 provider wire differences。

### 6.3 CapabilityResolver

```text
snapshot(scope, policy) -> immutable ActiveCapabilities
resolve_tool(name)
describe_tools(context/budget)
resolve_subagent_provider(id)
```

一次 Step 使用同一 snapshot，避免模型看到的 schema 与执行时 handler 不一致。

### 6.4 Workspace ports

从真实消费者抽取最窄集合：

- `WorkspaceIdentity`：root、display name、transport/security traits；
- `FileOps`：read/stat/write plan/commit/list/search 所需操作；
- `CommandOps`：前台 shell 与环境/cwd transition；
- `ProcessOps`：后台 start/status/wait/cancel；
- `ArtifactStore`：bounded writer/read windows。

不要设计 POSIX 全功能 VFS，也不要把一个巨大 `Workspace` trait 传给所有工具。

### 6.5 EventSink

核心可调用 `emit(RuntimeEvent)`，headless sink 可以同步记录，TUI sink 可以有界异步发送。语义终态不允许因 UI 队列满而丢；高频 delta 可以 coalesce/drop 并附带 dropped count。

## 7. 生命周期和资源所有权

### 7.1 Ownership tree

```text
Application
  +-- RootCapabilityRegistry
  +-- SessionManager
  |     +-- SessionStore
  |     +-- ConversationProjector
  +-- AgentHandle
  |     +-- AgentScope registrations
  |     +-- AgentSupervisor
  |     |     +-- provider attempt tasks
  |     |     +-- tool tasks
  |     |     +-- child agents
  |     +-- Inbox
  +-- UiRuntime
        +-- terminal event stream
        +-- render scheduler
```

谁创建 effect 谁清理。父 owner dispose 的顺序：

1. 拒绝新输入/注册；
2. 关闭 inbox sender；
3. cancel token tree；
4. 等待 tracked tasks 到 quiescence；
5. 把已发生 terminal facts 写入/flush session；
6. 释放 registration；
7. 释放 terminal/OS resources；
8. 返回聚合 cleanup error，不因一个失败跳过后续清理。

### 7.2 禁止 detached task

每个 `tokio::spawn` 必须在代码审查中回答：

- owner 是谁；
- cancel 从哪来；
- sender/receiver 何时关闭；
- panic/error 到哪里；
- shutdown 是否 join；
- 测试如何证明不泄漏。

允许 fire-and-forget 的唯一情况是进程即将不可恢复退出且明确记录；普通 UI delta forwarder 不属于此类。

## 7.3 诊断与追踪平面

核心业务依赖不能反向指向具体 telemetry backend。目标分为：

```text
Ledger  = durable business truth
Trace   = correlated operational spans/decisions/lifecycle
Payload = opt-in exact sensitive/large diagnostic bodies
Metrics = low-cardinality aggregates
```

所有核心owner接受窄 `TraceSink/TraceContext` 或使用统一instrumentation facade；local JSONL/flight recorder/OTLP都是adapter。Trace sink失败只标诊断不完整，不改变业务；runtime invariant发现pre-effect协议破坏时则必须阻止effect。

每个supervisor/registry/session/process owner提供只读resource snapshot，组合后形成当前ownership/effect tree。完整方案见 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)。

## 8. 错误模型

不要建立一个 `anyhow::Error` 穿过所有 public boundaries。每层区分：

- programmer invariant violation；
- invalid user/config input；
- policy denied；
- provider/tool protocol error；
- recoverable environment error；
- cancellation/budget terminal；
- fatal durability failure。

`thiserror` 用于 typed error；CLI/config 可用 `miette` 添加 code/help/source span。日志记录一次 owner-level context，避免每层重复打印同一错误。

## 9. 配置模型

```text
defaults
  -> user config
  -> workspace config
  -> CLI overrides
  -> validation
  -> ResolvedConfig { domain-specific configs + provenance }
```

规则：

- 所有未知字段 fail fast；
- merge 保持字段级；
- secret 只存 reference/secure store，不进入 debug/trace/session；
- 每个组件只收到自己的 resolved 子配置；
- policy profile 与 UI theme 等非安全配置分离；
- 当前 `allow_outside_workspace` 默认行为在兼容发布内保持，迁移到 trust profile 必须有显式 UX，不可静默收紧/放宽。

## 10. 架构验收测试

最终至少有以下自动检查：

1. core dependency tree 不含 UI/network/SSH/Windows crates。
2. TUI 源码不引用 `crate::app`、具体 provider、session file 实现。
3. agent 源码不包含 Ratatui、Crossterm 或工具展示字符串。
4. runtime 的 special tool name allowlist 为空。
5. 全部 production `spawn` 通过 wrapper/supervisor 或有显式注释豁免。
6. 全局 mutable registry 搜索结果为空。
7. 每个 adapter 实现通过相应 conformance suite。
8. JSON headless 与 TUI 对同一 runtime trace 得到等价业务终态。
9. 当前稳定 session corpus 可读、可 replay、不会在只读打开时改写。
10. 重构后 default capability snapshot 与批准的产品 snapshot 一致。
