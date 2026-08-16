# TPI 前后端分离与 Web / Desktop 多端架构改造任务书

你正在维护项目：

```text
https://github.com/Tydwdh/tpi
```

这是一个 Rust 编写的 Coding Agent / Agent Harness。

当前项目已经完成较大的架构重构，核心思想包括：

```text
Session is truth.
Context is projection.
AgentLoop stays thin.
ToolExecutor hides complexity.
Whoever creates an effect owns its cleanup.
```

当前已有：

* `tpi-agent`
* `tpi-session`
* `tpi-capabilities`
* `tpi-tui`
* Tool Runtime
* Session / Event
* Subagent
* MCP
* Web Search
* Edit / Mutation Journal
* Background Process
* ask_user suspend/resume
* TUI

本次任务不是重新设计 Agent Core。

## 总目标

在**保持现有 Agent Core 稳定**的基础上，把 TPI 改造成真正的：

> Backend Runtime + Protocol + Multiple Frontends

最终能够低成本同时维护：

```text
TUI
Web
Desktop
未来 VSCode Extension
未来 Mobile / Remote Client
```

要求：

> 新增一种 UI 时，不需要修改 Agent 核心。

最终目标架构：

```text
                           ┌─────────────────────┐
                           │     TPI Runtime     │
                           │                     │
                           │ AgentLoop           │
                           │ Session             │
                           │ ToolExecutor        │
                           │ Capabilities        │
                           │ Provider            │
                           └──────────┬──────────┘
                                      │
                              Application API
                                      │
                              tpi-protocol
                                      │
                     ┌────────────────┼────────────────┐
                     │                │                │
                     ▼                ▼                ▼
                    TUI             Server          Other
                     │                │
                     │          HTTP / WebSocket
                     │                │
                     │        ┌───────┴────────┐
                     │        │                │
                     ▼        ▼                ▼
                  Ratatui    Web           Desktop
                            Browser           │
                                             │
                                          Tauri
                                             │
                                      reuse Web frontend
```

---

# 一、第一原则：不要重构 Agent Core

首先完整阅读并理解：

```text
docs/architecture.md
crates/tpi-agent
crates/tpi-session
crates/tpi-capabilities
crates/tpi-tui
```

尤其确认：

* Session ownership
* Event flow
* Tool execution lifecycle
* Context projection
* ask_user suspend/resume
* cancellation
* background process
* retry
* mutation journal

禁止为了实现 Web UI 再写一套 Agent。

禁止：

```text
WebAgent
DesktopAgent
TuiAgent
```

最终只能有：

```text
TpiRuntime / ApplicationService
```

作为唯一业务入口。

所有 UI 都只是：

> Command producer + Event consumer

---

# 二、建立真正的 Application Boundary

当前即使 TUI 与 Agent 已经 crate 分离，也不代表真正的前后端分离。

需要增加一个稳定 Application API。

建议新增：

```text
crates/
├── tpi-protocol
├── tpi-runtime
├── tpi-server
├── tpi-tui
├── ...
```

如现有 crate 已经承担对应职责，不要为了名字强行拆 crate。

重点是职责，而不是目录数量。

---

# 三、实现 `tpi-protocol`

`tpi-protocol` 是整个多端架构最重要的新边界。

它只定义：

```text
Command
Event
Query/View
Error
Protocol metadata
```

必须：

```rust
Serialize
Deserialize
Clone
Debug
```

优先使用 serde。

---

## 禁止依赖 UI / Transport

`tpi-protocol` 不允许依赖：

```text
ratatui
crossterm
tauri
axum
warp
actix
tokio-tungstenite
React
Web-specific types
```

协议不能知道自己最终通过：

```text
Rust channel
WebSocket
HTTP
IPC
```

传输。

---

# 四、定义统一 ClientCommand

根据当前真实功能设计，不要机械照抄下面名字。

参考：

```rust
enum ClientCommand {
    CreateSession {
        workspace: String,
    },

    ResumeSession {
        session_id: SessionId,
    },

    SubmitMessage {
        session_id: SessionId,
        content: String,
    },

    CancelRun {
        session_id: SessionId,
    },

    RetryRun {
        session_id: SessionId,
    },

    AnswerInput {
        session_id: SessionId,
        request_id: RequestId,
        answer: String,
    },

    Undo {
        session_id: SessionId,
    },

    Redo {
        session_id: SessionId,
    },

    ProcessAction {
        ...
    },
}
```

不要让前端直接调用：

```text
AgentLoop::run()
ToolExecutor::execute()
SessionStore::append()
Provider::request()
```

所有修改 Runtime 状态的动作必须经过统一 Application Boundary。

---

# 五、定义统一 RuntimeEvent

示意：

```rust
enum RuntimeEvent {
    SessionCreated { ... },

    RunStarted { ... },

    UserMessageAdded { ... },

    AssistantStarted { ... },

    AssistantDelta {
        session_id,
        run_id,
        delta,
    },

    AssistantCompleted { ... },

    ToolStarted {
        tool_call_id,
        tool_name,
        ...
    },

    ToolCompleted {
        tool_call_id,
        result,
        ...
    },

    InputRequested {
        request_id,
        ...
    },

    ProcessStarted { ... },

    ProcessOutput { ... },

    ProcessExited { ... },

    MutationRecorded { ... },

    RunCompleted { ... },

    RunFailed { ... },

    RunCancelled { ... },
}
```

要求：

> UI 不通过读取 Agent 内部对象推断状态。

UI 状态必须尽可能由：

```text
Initial Snapshot
+
RuntimeEvent stream
```

构造。

---

# 六、Event 必须带稳定 Identity

至少考虑：

```text
protocol_version
session_id
run_id
event_id / sequence
timestamp
```

重要异步操作还需要：

```text
tool_call_id
process_id
request_id
client_request_id
```

例如：

```rust
struct EventEnvelope {
    protocol_version: u32,
    session_id: SessionId,
    seq: u64,
    timestamp: ...,
    event: RuntimeEvent,
}
```

不要依赖：

```text
“上一条事件应该是谁”
```

这种隐式顺序推断。

---

# 七、建立 `RuntimeHandle / ApplicationService`

目标：

```text
Frontend
    ↓
RuntimeHandle.send(command)
    ↓
Application Runtime
    ↓
RuntimeEvent stream
```

参考：

```rust
pub struct RuntimeHandle {
    ...
}

impl RuntimeHandle {
    pub async fn command(
        &self,
        command: ClientCommand,
    ) -> Result<CommandAck>;

    pub fn subscribe(
        &self,
    ) -> RuntimeEventReceiver;
}
```

真正的 Agent Core 在内部。

Frontend 不持有：

```text
AgentLoop
ToolExecutor
Provider
MutationJournal
```

---

# 八、TUI 也迁移到统一边界

现有 TUI 不需要联网。

可以：

```text
TUI
 ↓
RuntimeHandle
 ↓
Agent Runtime
```

即：

```text
keyboard
    ↓
ClientCommand
    ↓
Runtime
    ↓
RuntimeEvent
    ↓
TUI state projection
    ↓
render
```

不要为了 Web/Desktop 把现有 TUI 强行改成 WebSocket 客户端。

我们要统一的是：

```text
Application Contract
```

不是强制统一 Transport。

---

# 九、实现 `tpi-server`

Server 是：

> Application API 的 Network Adapter

建议优先：

```text
Rust
Axum
Tokio
WebSocket
```

但先检查当前项目依赖和已有实现，避免重复框架。

---

# 十、HTTP 与 WebSocket 职责

不要把实时 Agent 流式输出全部做成 REST polling。

建议：

## HTTP

负责：

```text
GET /api/health
GET /api/version
GET /api/sessions
GET /api/sessions/:id
```

以及必要的静态资源。

不要设计庞大的 REST API。

---

## WebSocket

承担：

```text
ClientCommand
      ↓
WebSocket
      ↓
Runtime

RuntimeEvent
      ↓
WebSocket
      ↓
Client
```

例如：

```json
{
  "type": "command",
  "request_id": "...",
  "payload": {
    "type": "submit_message",
    "session_id": "...",
    "content": "..."
  }
}
```

服务器响应：

```json
{
  "type": "ack",
  "request_id": "..."
}
```

随后持续发送：

```text
AssistantDelta
ToolStarted
ToolCompleted
...
```

---

# 十一、必须考虑断线重连

Web UI 与 Desktop 都可能：

```text
sleep
network reconnect
page refresh
frontend reload
```

所以不要假设 WebSocket 永不掉线。

利用现有 Session/Event Store。

协议支持类似：

```text
subscribe:
    session_id
    after_seq
```

客户端保存：

```text
last_event_seq
```

断线：

```text
seq = 172
   ↓
reconnect
   ↓
resume after_seq = 172
   ↓
server replay 173...
```

如果现有 Session 架构不适合直接 replay，则实现：

```text
Initial SessionView
+
live events
```

第一阶段也可以：

```text
reconnect
→ GET current SessionView
→ subscribe live events
```

但必须避免出现：

```text
页面刷新
→ 正在运行的 Agent 消失
```

---

# 十二、前端绝对不能成为 Source of Truth

React/Web/Desktop 可以拥有：

```text
ViewState
SelectedTab
ScrollOffset
ExpandedToolCard
Theme
DraftText
```

但不能拥有：

```text
Run truth
Tool truth
Process truth
Session history truth
Agent state truth
Mutation truth
```

原则保持：

> Session is truth. UI is projection.

---

# 十三、实现 Web UI

建议技术栈：

```text
TypeScript
React
Vite
```

如果仓库已有其它 Web 技术栈，优先复用，不要为了偏好强制替换。

要求保持简单。

Web UI 首版不追求花哨。

优先实现：

```text
Session list
Chat transcript
Streaming assistant output
Tool execution cards
Process output
ask_user dialog
Cancel
Retry
Undo / Redo
Connection state
Current model / workspace basic information
```

---

# 十四、Web UI 推荐布局

可以参考成熟 Coding Agent：

```text
┌─────────────────────────────────────────────────────────┐
│ TPI                                    Connected / Model │
├─────────────┬───────────────────────────────────────────┤
│ Sessions    │                                           │
│             │            Conversation                   │
│ session A   │                                           │
│ session B   │    User                                   │
│ session C   │    Assistant                              │
│             │      Tool: read                           │
│             │      Tool: edit                           │
│             │      Process                              │
│             │                                           │
│             │                                           │
├─────────────┴───────────────────────────────────────────┤
│ Message input                                Send       │
└─────────────────────────────────────────────────────────┘
```

不要第一阶段做复杂 IDE。

TPI 是 Agent UI，不是 VSCode clone。

---

# 十五、Tool Rendering 做成组件协议

不要：

```text
if tool_name == "read"
if tool_name == "bash"
if tool_name == "edit"
...
```

散落整个前端。

设计：

```text
ToolCard
├── GenericToolCard
├── ReadToolCard
├── EditToolCard
├── BashToolCard
├── ProcessToolCard
└── ...
```

未知工具：

```text
GenericToolCard
```

必须能正常显示。

这样未来 MCP Tool / Plugin 不需要修改整个 UI。

---

# 十六、Assistant Streaming

前端必须支持增量：

```text
AssistantStarted
    ↓
AssistantDelta
AssistantDelta
AssistantDelta
    ↓
AssistantCompleted
```

不要等完整 response 才显示。

需要正确处理：

```text
Markdown
Code fence
unfinished markdown
reasoning/status if protocol supports
```

流式过程中不要频繁重建整个历史 DOM。

---

# 十七、Process 输出

后台 Job：

```text
ProcessStarted
ProcessOutput
ProcessExited
```

前端显示：

```text
Running
Elapsed
Output
Exit Code
Cancel
```

不要把完整日志全部放 React state。

使用：

```text
bounded window
cursor
virtualization
```

如果后端支持 artifact，则长日志使用 artifact。

---

# 十八、ask_user

现有真正 suspend/resume 语义必须保留。

流程：

```text
Runtime
  ↓
InputRequested(request_id)
  ↓
Frontend dialog
  ↓
AnswerInput(request_id)
  ↓
Runtime resumes
```

不要让 Web 单独实现一套 ask_user 状态机。

必须由 Runtime 决定：

```text
request 是否仍然 pending
answer 是否已接受
```

重复回答必须：

```text
idempotent / rejected safely
```

---

# 十九、实现 Desktop：优先 Tauri + 复用 Web UI

Desktop 不重新写一套 UI。

目标：

```text
tpi-web
    ↓
同一份 React frontend
    ↓
Browser
或
Tauri WebView
```

也就是说：

> Web 与 Desktop 尽可能 100% 共享 UI 代码。

Desktop 只是：

```text
window shell
local backend lifecycle
native integration
```

---

# 二十、Desktop Backend 不要走第二套 API

禁止：

```text
Web -> WebSocket protocol

Desktop -> Tauri invoke 直接操作 AgentLoop
```

这样会再次产生两套前端 API。

优先：

```text
Desktop
    ↓
启动 embedded/local TPI server
    ↓
WebSocket
    ↓
同一 Application Protocol
```

于是：

```text
Web client
Desktop client
```

使用完全相同的 TypeScript SDK。

---

# 二十一、Desktop 推荐生命周期

例如：

```text
Tauri starts
   ↓
find free localhost port
   ↓
generate random auth token
   ↓
start embedded TPI server
   ↓
WebView connects:
ws://127.0.0.1:<port>
   ↓
normal protocol
```

退出：

```text
Desktop close
   ↓
frontend disconnect
   ↓
request runtime shutdown
   ↓
bounded shutdown
   ↓
server stop
```

Agent / MCP / Job 生命周期必须遵循现有 Supervisor ownership。

---

# 二十二、本地 Server 安全

默认：

```text
127.0.0.1
```

不要默认：

```text
0.0.0.0
```

Desktop embedded server：

```text
random per-launch token
```

WebSocket / HTTP 都验证 token。

不要因为是 localhost 就完全裸奔。

如果未来支持远程 Web：

单独增加：

```text
--listen
--auth-token
```

不要让本次任务演变成完整账号系统。

---

# 二十三、CORS

默认只允许：

```text
expected local frontend origin
```

开发模式单独配置。

禁止生产默认：

```text
Access-Control-Allow-Origin: *
```

---

# 二十四、建立 TypeScript Protocol Client

不要让 React 组件直接：

```text
new WebSocket(...)
JSON.parse(...)
```

建立：

```text
packages/tpi-client
```

或 frontend 内：

```text
src/lib/tpi-client
```

职责：

```text
connect
disconnect
reconnect
sendCommand
subscribe
protocol decode
request correlation
event sequencing
```

React 只使用：

```ts
client.submitMessage(...)
client.cancelRun(...)
client.subscribe(...)
```

---

# 二十五、协议版本

从第一版就增加：

```text
protocol_version
```

例如：

```text
1
```

连接握手：

```text
ClientHello {
    protocol_version
    client_name
    client_version
}
```

Server：

```text
ServerHello {
    protocol_version
    server_version
}
```

不兼容：

```text
ProtocolVersionMismatch
```

不要等协议变复杂后再补版本控制。

---

# 二十六、不要暴露 Rust 内部 struct

错误做法：

```rust
serde_json::to_string(&InternalSession)
```

不要把：

```text
Agent internals
Session implementation details
Tool internal structs
Provider structs
```

直接作为 API。

Protocol DTO 必须独立。

例如：

```rust
Session
```

内部再复杂，前端只看到：

```rust
SessionView {
    id,
    title,
    workspace,
    status,
    created_at,
    updated_at,
}
```

这是 Anti-Corruption Layer。

---

# 二十七、错误必须结构化

协议不要发送：

```json
{
    "error": "something failed"
}
```

而是：

```json
{
    "code": "stale_revision",
    "message": "...",
    "retryable": false,
    "details": {}
}
```

参考：

```text
Cancelled
Timeout
ProviderFailure
RetryBudgetExhausted
SessionNotFound
InvalidCommand
InputAlreadyAnswered
ProcessNotFound
PermissionDenied
InternalError
```

UI 根据 `code` 做行为，不解析英文字符串。

---

# 二十八、Backpressure

Coding Agent 的事件可能非常多：

```text
token streaming
process logs
tool events
subagent events
```

不要：

```text
unbounded_channel
```

直接接到 WebSocket。

必须评估现有 Runtime Event Channel。

设计：

```text
critical events
    -> never silently drop

high-frequency stream
    -> bounded / coalescable
```

例如：

```text
AssistantDelta
ProcessOutput
```

允许批量合并。

但：

```text
ToolCompleted
RunCompleted
InputRequested
```

绝不能丢。

---

# 二十九、多客户端语义

架构必须允许：

```text
Web
+
Desktop
+
TUI
```

同时观察同一个 Runtime。

至少做到：

```text
one authoritative runtime
many subscribers
serialized commands
```

不要给每个 frontend 启动一个独立 Agent Runtime 操作同一个 Session。

第一阶段不需要复杂 distributed locking。

但要保证：

```text
Command queue ordered
ask_user first accepted answer wins
Cancel idempotent
Retry idempotent / state checked
Undo/Redo serialized
```

---

# 三十、TUI / Web / Desktop 功能一致性

定义 Capability Matrix。

例如：

| Capability  | TUI | Web | Desktop |
| ----------- | --: | --: | ------: |
| New session |   ✅ |   ✅ |       ✅ |
| Resume      |   ✅ |   ✅ |       ✅ |
| Streaming   |   ✅ |   ✅ |       ✅ |
| Tools       |   ✅ |   ✅ |       ✅ |
| ask_user    |   ✅ |   ✅ |       ✅ |
| Cancel      |   ✅ |   ✅ |       ✅ |
| Retry       |   ✅ |   ✅ |       ✅ |
| Undo/Redo   |   ✅ |   ✅ |       ✅ |
| Processes   |   ✅ |   ✅ |       ✅ |
| Settings    |   ✅ |   ✅ |       ✅ |

不要默默做出一个功能残缺的 Web Demo。

---

# 三十一、不要复制业务逻辑

如果发现：

```text
TUI:
    if session running ...
Web:
    if session running ...
Desktop:
    if session running ...
```

先判断：

这是：

```text
View logic
```

还是：

```text
Domain logic
```

Domain logic 必须移动到 Runtime。

例如：

```text
当前是否允许 Retry
当前是否允许 Cancel
当前 request_input 是否仍有效
```

应由 Runtime 给出事实。

前端只负责：

```text
按钮 enable/disable
render
interaction
```

---

# 三十二、建议目录

不要为了符合目录而强制迁移已有代码，但目标职责可以类似：

```text
crates/
├── tpi-core
├── tpi-agent
├── tpi-session
├── tpi-capabilities
│
├── tpi-protocol
│   ├── command.rs
│   ├── event.rs
│   ├── view.rs
│   ├── error.rs
│   └── version.rs
│
├── tpi-runtime
│   ├── application.rs
│   ├── handle.rs
│   ├── dispatcher.rs
│   └── subscription.rs
│
├── tpi-server
│   ├── http.rs
│   ├── websocket.rs
│   └── auth.rs
│
└── tpi-tui
```

Frontend：

```text
apps/
├── web/
│   ├── src/
│   └── ...
│
└── desktop/
    ├── src-tauri/
    └── ...
```

共享：

```text
packages/
└── tpi-client/
```

如使用 monorepo package manager，选项目最简单的方式即可。

不要为了引入复杂 JS workspace 工具而过度工程。

---

# 三十三、不要做微服务

Backend 只是：

```text
TPI Runtime
```

不是：

```text
Agent Service
Session Service
Tool Service
Process Service
MCP Service
```

不要微服务化。

保持：

> Modular Monolith + Stable Protocol Boundary

这是桌面 Coding Agent 更合理的形态。

---

# 三十四、测试要求

## Protocol tests

测试：

```text
Command round-trip serde
Event round-trip serde
Unknown fields compatibility
Protocol version mismatch
Structured errors
```

---

## Runtime contract tests

测试：

```text
submit command
→ expected event sequence
```

例如：

```text
SubmitMessage
→ RunStarted
→ ...
→ RunCompleted
```

---

## WebSocket tests

必须自动测试：

```text
connect
handshake
send command
receive ack
receive event
disconnect
reconnect
```

---

## Resume tests

重点：

```text
run 正在执行
↓
client disconnect
↓
runtime 继续
↓
client reconnect
↓
恢复正确状态
```

---

## Multi-client tests

至少：

```text
client A
client B

同一 session subscribe

A submit
A/B 都看到 event
```

---

## ask_user tests

```text
InputRequested
↓
Web responds
↓
Runtime resumes
```

以及：

```text
A/B 同时 AnswerInput
```

只能有一个成功。

---

## Desktop smoke tests

确保：

```text
Tauri starts backend
frontend connects
session works
shutdown finishes
```

---

# 三十五、构建与开发体验

最终希望支持类似：

```text
cargo run --bin tpi
```

继续启动 TUI。

Web：

```text
tpi server
```

然后 frontend dev server 连接。

Production Web：

```text
tpi server --listen 127.0.0.1:...
```

Desktop：

```text
desktop dev
```

自动启动 backend。

具体命令根据项目最终结构决定。

---

# 三十六、保持 CLI / TUI 向后兼容

本次改造完成后：

```text
原来的 TPI TUI
```

必须继续正常工作。

不允许为了 Web 把：

```text
CLI
TUI
config
workspace
session
```

全部打坏。

建议阶段性迁移：

```text
1. 建 protocol
2. 建 RuntimeHandle
3. TUI 接 RuntimeHandle
4. 保证全部旧测试通过
5. 加 server
6. 加 Web
7. 加 Desktop
```

不要同时重写所有层。

---

# 三十七、实施阶段

## Phase 0 — Audit

先输出：

```text
当前 UI -> Agent 调用链
当前 Event 流
当前 Session ownership
当前可以复用的类型
当前耦合点
迁移风险
```

不要立刻编码。

---

## Phase 1 — Protocol

实现：

```text
tpi-protocol
ClientCommand
RuntimeEvent
View DTO
Structured Error
Envelope
ProtocolVersion
```

此阶段不修改 UI 行为。

---

## Phase 2 — Application Runtime

实现：

```text
RuntimeHandle
command dispatcher
event subscription
```

把 TUI 当前直接业务调用逐步迁移进去。

要求：

```text
cargo test --workspace
```

持续通过。

---

## Phase 3 — TUI Adapter

TUI 变成真正：

```text
command producer
event consumer
```

验证没有业务能力回归。

---

## Phase 4 — Server

实现：

```text
Axum
HTTP
WebSocket
auth
handshake
reconnect
event subscription
```

---

## Phase 5 — Web

实现生产可用的基础 Web UI。

不是 demo。

核心 Agent 工作流必须完整。

---

## Phase 6 — Desktop

使用 Tauri 包装同一个 Web frontend。

Desktop embedded backend 使用与 Web 相同 protocol。

---

## Phase 7 — Hardening

完成：

```text
backpressure
reconnect
multi-client
bounded shutdown
security
logging
error UX
integration tests
```

---

# 三十八、每阶段必须提交设计说明

每完成一个 Phase，更新：

```text
docs/architecture.md
```

并说明：

```text
为什么这样设计
ownership 是谁
lifecycle 是谁
source of truth 是谁
transport boundary 在哪
失败如何传播
shutdown 如何工作
```

避免代码和 architecture 漂移。

---

# 三十九、关键架构 Invariants

最终系统必须满足：

```text
1. Session remains the source of truth.

2. No frontend owns Agent state.

3. No frontend directly calls ToolExecutor or Provider.

4. All state-changing frontend actions go through ClientCommand.

5. All frontend-visible runtime changes are expressible as RuntimeEvent
   or SessionView.

6. Protocol types do not depend on any UI framework.

7. Transport does not contain domain logic.

8. Web and Desktop share the same protocol.

9. Desktop reuses the Web frontend.

10. TUI does not need networking to use the same Application API.

11. Client disconnect never kills an Agent run unless explicitly requested.

12. Runtime lifecycle is independent of frontend lifecycle where appropriate.

13. Server shutdown is bounded.

14. Multiple clients observe one authoritative Runtime.

15. Unknown tools can still be rendered safely.

16. Protocol is versioned from day one.
```

---

# 四十、明确禁止的方案

禁止：

```text
❌ 给 Web 重新实现一套 Agent

❌ React 直接读 Session 文件

❌ Desktop 直接操作 AgentLoop，而 Web 走 WebSocket

❌ Tauri invoke 成为第二套业务 API

❌ tpi-protocol 依赖 ratatui / axum / tauri

❌ 把内部 Rust struct 直接 serde 后当公网协议

❌ WebSocket handler 内写 Agent domain logic

❌ 每个 UI 自己维护 Run 状态机

❌ Browser disconnect 导致 Agent 自动 cancel

❌ 前端靠解析日志字符串判断 Tool 状态

❌ 为了前后端分离把项目微服务化

❌ 一次性重写整个 TPI
```

---

# 四十一、Definition of Done

完成后必须达到：

```text
□ 原 TUI 正常工作

□ Agent Core 不依赖任何 UI

□ tpi-protocol 不依赖任何 UI/transport framework

□ Runtime 有稳定 Command/Event API

□ Web 可创建/恢复 Session

□ Web 支持完整消息 streaming

□ Web 可观察 Tool 执行

□ Web 支持 ask_user

□ Web 支持 Cancel / Retry

□ Web 支持 Undo / Redo

□ Web 可查看后台 Process

□ Web 刷新/断线可恢复

□ Desktop 使用同一 Web frontend

□ Desktop 使用与 Web 相同 protocol

□ Desktop 自动管理本地 Backend lifecycle

□ Backend 默认仅 localhost

□ Local Desktop connection 有随机 auth token

□ 多客户端可订阅同一个 Session

□ 所有关键事件具有稳定 ID / sequence

□ Server shutdown bounded

□ cargo test --workspace 通过

□ Web production build 通过

□ Desktop build 通过

□ docs/architecture.md 已更新
```

---

# 最终目标

不要仅仅实现：

```text
“TPI 加一个网页”
```

本任务真正要得到的是：

```text
                    TPI Runtime
                         │
                  Stable Protocol
                         │
             ┌───────────┼───────────┐
             │           │           │
            TUI         Web       Desktop
             │           │           │
          Ratatui     Browser      Tauri
```

以后实现：

```text
VSCode Extension
JetBrains Plugin
Mobile Client
Remote Web Client
```

应该只需要增加新的 Adapter / Frontend。

而不是再修改：

```text
AgentLoop
Session
ToolExecutor
Provider
```

最终判断标准：

> **新增一个 UI，是一个前端工程问题，而不再是一次 Agent 架构重构。**

请先完成 Phase 0 架构审计并给出具体迁移方案，再开始代码修改。每一阶段完成后运行测试、检查架构边界，并在确认没有回归后继续下一阶段。
