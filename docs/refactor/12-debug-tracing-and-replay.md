# 12. 全链路追踪、运行时不变量与安全回放

## 1. 目标定义

“全程追踪每一个细节”不能被实现成到处打印字符串。TPI 要达到的是：

> 对每一次重要决定、状态转换、外部调用和副作用，都能回答谁发起、基于什么输入、选择了哪个 Capability、经历了哪些阶段、产生了什么结果、写入了哪些 durable facts，以及失败后哪些信息完整、哪些已丢失。

这里的“每一个细节”指**语义和因果上的完整性**，不是记录每条 CPU 指令。系统必须能调试：

- 为什么选了这个 provider/model/tool/context；
- 模型实际看到了什么；
- 某段 context 为什么进入、被裁剪或被压缩；
- 工具为什么被允许、拒绝、排队或串行；
- 哪个 retry/timeout/cancel 结束了操作；
- session event 在副作用前后何时提交与 sync；
- 哪个 plugin/capability 注册了当前实现，何时被替换或 dispose；
- 哪个 task/process/channel 仍然存活，owner 是谁；
- UI 为什么显示某个状态，是否丢过 delta；
- 子代理继承了什么能力、预算和上下文；
- trace 本身是否完整，是否因容量、崩溃或脱敏被省略。

追踪失败不能改变业务结果；但运行时 invariant 发现真实协议破坏时，应阻止继续产生不可信副作用。

## 2. 从 DeepSeek Harness 吸收什么

本地审计基线：DeepSeek Harness `47f943859bef60e4160492346772ded9b24f765a`。

### 2.1 可重建的模型请求

DeepSeek Harness 把 `turn/start`、`step/start`、`request/header`、`request/context`、`assistant/chunk`、`assistant/message`、`tool/call`、`tool/result`、`step/end`、`turn/end` 记录为有序 session facts。`request/header` 保存 resolved model config、system prompt 和有序 tool schemas；运行时 invariant 在模型 dispatch 前比较实际 request 与 session fold，发现不一致立即失败。

TPI 应采用“model-visible ⇔ locally reconstructable”原则，但按 TPI 的 JSONL 写入成本和隐私约束调整 chunk/payload 存储。

### 2.2 Telemetry 是 session 的 projection

DeepSeek 的 session telemetry 可：

- 跟随 live `session/event`；
- 从 canonical log 按 seq prefix 重放；
- deep-copy 后脱敏，不改写 canonical log；
- 把 batching/retry/queue/loss policy 交给 backend；
- 以 `(session.id,event.seq)` 去重；
- 区分 durable ledger record 与 operational record；
- 默认关闭远程上传，并披露 full/feedback-only/disabled sharing 状态。

TPI 应采用 live/replay 同一 projection、backend 独立、sharing 显式的原则。

### 2.3 Runtime invariant companion

DeepSeek 让每个拥有状态协议的 package 注册自己的 invariant：session enclosure、call/result pairing、agent status transition、inbox FIFO conservation、model-request reconstruction、tool pipeline stage、subagent start/end pairing 等。诊断框架本身不导入所有业务包。

TPI 应采用“协议 owner 提供 checker”的模式，而不是建立一个知道所有领域细节的 `DiagnosticsManager`。

### 2.4 Effect/Fiber diagnostics tree

DeepSeek/Cordis 为 effect 加 label，fiber 能列出仍存活的 effect tree；registration、listener、service 与 disposer 归属可检查。

TPI 不需要复制 Cordis runtime，但 supervisor/registry/process/MCP/subagent 必须暴露统一的**只读 ownership snapshot**，能看到资源树和当前 lifecycle。

### 2.5 需要改进的部分

DeepSeek telemetry 文档明确指出：无部署方规则时可导出完整 event data；delivery 默认 best-effort；只导出每个 step 的第一个 assistant chunk；OTel SDK 的队列和丢失由 backend 配置承担。TPI 的方案应：

- 内置最低脱敏规则，不允许“没装规则就裸传”；
- 把本地完整诊断与远程 telemetry 分开；
- 显式记录 trace gaps/drop counters；
- 提供可选 durable diagnostic spool，而不是宣称 best-effort 等于完整；
- 不把 OpenTelemetry SDK 变成核心 contract。

## 3. 四个数据平面

```text
Domain operation
     |
     +--> Ledger Plane ------ canonical committed facts
     |
     +--> Trace Plane ------- spans, decisions, timings, lifecycle
     |
     +--> Payload Plane ----- opt-in sensitive/large exact bodies
     |
     +--> Metrics Plane ----- low-cardinality aggregates
```

| 平面 | 权威性 | 默认可靠性 | 内容 | 是否可采样 |
|---|---|---|---|---|
| Ledger | 业务事实源 | durable contract | session/event、effect intent/receipt、terminal | 否 |
| Trace | 诊断事实 | 本地有界；完整性显式 | span、decision、cache、queue、retry、ownership | 标准模式可降采高频项 |
| Payload | 诊断附件 | 配置决定；content-addressed | exact request/body/chunks/output/frame | 可关闭；forensic 才完整 |
| Metrics | 聚合观测 | best effort | count、histogram、gauge | 是 |

### 3.1 不允许混淆

- Trace 写失败不能让 Ledger 假装 append 失败。
- Ledger 不能因为 OTel exporter 慢而阻塞。
- Payload 删除后 Trace 仍能说明 hash、size、redaction 和缺失原因。
- Metrics 不能用高基数 session/path/tool args 当 label。
- Trace record 不是 SessionEvent；不能把 trace sink 的行为再次写入同一 trace 形成递归风暴。

## 4. 因果与身份模型

### 4.1 Trace 边界

一个 public Agent Run 对应一个 `TraceId`。后续 follow-up 若形成新 Run，则新 Trace；session ID 关联它们。子代理默认新 Trace，并通过 `SpanLink/parent_child` 关联父 trace，避免把长生命周期 child 强塞进一棵永不关闭的 span tree。

```text
SessionId
  +-- TraceId (Run A)
  |     +-- RunSpan
  |           +-- TurnSpan
  |                 +-- StepSpan
  |                       +-- ContextSpan
  |                       +-- ProviderAttemptSpan
  |                       +-- ToolBatchSpan
  |                             +-- ToolCallSpan(s)
  +-- TraceId (Follow-up Run B)
  +-- Child TraceId(s) -- linked from parent delegation span
```

### 4.2 必须存在的 typed IDs

- `TraceId`、`SpanId`、`ParentSpanId`；
- `SessionId`、`RunId`、`TurnId`、`StepId`、`AttemptId`、`RequestId`；
- `ToolCallId`、`CapabilityId`、`RegistrationId`；
- `ChildAgentId`、`ProcessId`、`WorkspaceId`；
- durable `EventId` 和 `EventSeq`；
- `CommandId`、`EffectId`（仅在一个 command 可能产生多个 effect 或重试时需要）。

不要要求每条 record 填全部 ID。创建 record 的 owner 填它能权威提供的身份；sink 继承 active trace context。

### 4.3 Causation 与 links

树形 parent span 只表达嵌套执行。非树关系使用显式 link：

- inbox message -> claimed turn；
- assistant tool call -> tool invocation；
- parent delegation -> child trace；
- retry attempt -> previous attempt；
- runtime terminal -> committed session event；
- UI view action -> app command；
- recovery decision -> prior ToolStarted event。

`caused_by_event_id` 只引用 durable event；`links` 可引用其他 trace/span/message/tool identity。

## 5. Trace record contract

示意结构：

```rust
struct TraceRecord {
    schema: TraceSchemaVersion,
    record_seq: u64,
    wall_time: OffsetDateTime,
    monotonic_ns: u64,
    trace_id: TraceId,
    span_id: SpanId,
    parent_span_id: Option<SpanId>,
    kind: TraceRecordKind,       // span_open/event/span_close/gap/link/snapshot
    name: TraceName,
    level: TraceLevel,
    outcome: Option<TraceOutcome>,
    ids: CorrelationIds,
    attributes: BTreeMap<String, TraceValue>,
    payload: Option<PayloadRef>,
    source: Option<SourceLocation>,
    completeness: RecordCompleteness,
}
```

### 5.1 顺序与时间

- `record_seq` 是一个 trace/store shard 内的单调提交顺序；
- wall clock 用于人类对齐，不用于计算 duration/order；
- monotonic clock 用于同进程 duration；
- 跨进程/远端只依赖 causal IDs 和各自时间，不假装全局纳秒顺序；
- span close 记录 duration/status；进程 crash 时缺 close 是有效诊断；
- exporter 重试可产生重复，接收端按 `(trace_id,record_seq,sink_generation)` 去重。

### 5.2 TraceValue 与敏感度

禁止直接接受任意 `Debug` string。字段构造时声明：

```rust
enum Sensitivity {
    Public,
    Internal,
    WorkspaceContent,
    Secret,
}

enum TraceValue {
    Plain(JsonScalar),
    Hashed { blake3: String, bytes: u64 },
    Redacted { reason: RedactionReason },
    Payload(PayloadRef),
}
```

字段 schema 定义允许的 sensitivity 和 exporter policy。`Authorization`、API key、credential value、完整 environment 永远不能 `Plain`。

### 5.3 Completeness

每份 trace manifest 必须报告：

```text
complete | lossy | truncated | redacted | crashed | writer_failed
dropped_records_by_kind
first/last_record_seq
missing_payloads
sink generations
shutdown marker present
```

如果队列满，不允许静默丢弃。恢复容量后写 `trace/gap {from,to,count,kinds}`；若连 gap 都无法写，在 manifest/side counter 记录。

## 6. Span 与事件目录

### 6.1 Application

| Span/Event | 必要字段 |
|---|---|
| `app.command` | command ID/type/source、accepted/rejected、queue delay |
| `app.effect` | effect ID/type/owner、target class、status、duration |
| `session.switch` | old/new IDs、flush/close status |
| `config.resolve` | sources/provenance、changed fields（secret values absent） |

### 6.2 Agent

| Span/Event | 必要字段 |
|---|---|
| `agent.run` | session/run/trace、trigger、budget snapshot、terminal reason |
| `agent.turn` | turn、claimed inbox message IDs、end reason |
| `agent.step` | step、capability snapshot rev、model request ID |
| `agent.phase.changed` | from/to、cause、event ID |
| `agent.cancel.requested` | actor/target/reason、propagation count |
| `budget.debited` | budget kind、before/after、source attempt/tool/child |

### 6.3 Inbox/Interaction

- insert/replace/remove/claim/release/discard；
- next-step/next-turn/answer/approval class；
- message ID、byte size、queue depth，不在标准 trace复制正文；
- request/answer IDs、expiry、actor；
- claim 后对应 durable event ID；
- queue-full policy decision。

### 6.4 Context

`context.assemble` 下每个 contribution 记录：

- provider/capability/registration ID；
- source event range/artifact/file revision；
- candidate bytes/tokens；
- included/excluded/truncated/pruned/compacted；
- decision code 和优先级；
- final order/offset；
- cache key/hit/miss/invalidation reason；
- final message count/token estimate/request hash。

标准 trace 只存 hash/size/reason；forensic payload 存完整 normalized context。

### 6.5 Provider

- resolve candidates/winner/registration generation；
- prepared effective config 与 adapter defaults；
- attempt start、URL host/path（不含 credentials/query secret）、connect/status；
- retry eligibility、reason、backoff、Retry-After、累计 wait；
- first byte/first semantic chunk/first text token；
- 每个 normalized chunk 的 index/type/bytes，在 verbose/forensic 存 body；
- tool-call assembly transition；
- finish reason/usage/cache tokens；
- cancel observed at which stage；
- protocol/transport error typed chain；
- exact wire request/response body只进入 forensic Payload，header allowlist 永不包含 Authorization。

### 6.6 Tool pipeline

每个 pipeline stage 记录 start/decision/end：

```text
resolve -> parse -> validate -> authorize -> analyze footprint
-> schedule/acquire -> write-ahead -> execute -> normalize
-> directive/effect report -> terminal commit -> cleanup
```

字段：definition/registration/snapshot rev、args hash/bytes、schema rev、policy code、resource footprint、wave/permit wait、effect class、recovery metadata ref、progress dropped、outcome status/bytes/artifacts、session event IDs、cleanup result。

### 6.7 Workspace/Process/MCP

- canonical target 与 display target 分开；标准 trace 允许 workspace-relative path，绝对路径按 policy；
- file stat/revision/bytes/atomic phase；正文用 PayloadRef；
- command program/argv 按 sensitivity 分字段，不拼一条可能泄密的 shell string；
- process spawn/exit/cancel/kill tree、pid/owned process ID、stdout/stderr byte/drop；
- MCP process/transport generation、initialize capabilities、list revision、tool registration IDs、request/response IDs、restart/drain/dispose；
- server stderr 是 untrusted payload，有大小/escape/redaction限制。

### 6.8 Session/Projection

- append candidate/accepted/rejected、event type/id/seq/bytes；
- flush/sync barrier type/duration/error；
- lock acquisition/owner conflict；
- reader version conversion、tail drop、corruption；
- recovery scan/decision/synthetic terminal；
- projector name/version/watermark/apply duration/cache；
- incremental/full mismatch 直接触发 invariant incident。

### 6.9 Capability/Lifecycle

- plugin/bundle load config hash；
- capability register/override/unregister；
- scope、CapabilityId、RegistrationId、origin、definition rev；
- setup stages、dependency wait、publication、rollback；
- immutable snapshot creation和 entry list hash；
- disposer start/end/error；
- task/process/channel/listener 子资源 snapshot。

### 6.10 TUI

标准模式记录 semantic `UiIntent`、focus transition、overlay stack、scroll mode/anchor transition、render duration/cache/dropped delta，不记录每次 mouse move或完整 frame。

verbose UI filter 可记录 raw key/mouse/resize（文本输入正文默认 hash）；forensic incident 可保存触发前后有限 buffer snapshots。必须防止 terminal escape 和用户代码在 inspector 中直接执行/渲染。

### 6.11 Subagent

- delegation span 与 child trace link；
- provider/agent profile、capability snapshot、workspace mode；
- budget allocation/debit/return；
- queued/start/await input/terminal；
- cancel propagation；
- report hash/schema/artifact refs；
- parent context 是否接纳/裁剪 report及原因。

## 7. 模型请求可重建合同

这是全链路调试的最高优先级。

### 7.1 Durable header snapshot

当以下任一项变化时写 full logical header snapshot：

- provider/model/effective adapter defaults；
- reasoning/max output/sampling/stop 等实际请求配置；
- rendered system prompt；
- ordered tool schemas；
- capability snapshot revision；
- provider context window/model capability metadata。

连续相同 header 不重复正文，但每个 request manifest 引用生效的 header event ID/hash。

### 7.2 Per-request manifest

每次真正 dispatch 前 durable 或按批准的 trace contract 记录：

```text
request_id / turn / step / attempt relationship
header_event_id + header_hash
conversation/session head event_id
context projection name + version
ordered source event/message IDs
normalized request hash and byte/token estimates
payload_ref（forensic exact request，可选）
```

如果只有当前算法才能重建，而升级后无法 byte-exact 重建，manifest 必须诚实标 `semantic_reconstructable`，不能标 `byte_exact`。

### 7.3 Dispatch-time invariant

在 adapter 收到 request 的瞬间：

1. 从当前 committed session head fold header/messages/context；
2. canonical serialize actual request；
3. 比较 provider/model/system/tools/messages/config；
4. 写 actual hash；
5. 不一致时禁止网络 dispatch，生成 `MODEL_REQUEST_DESYNC` incident。

这比事后日志更重要，因为它阻止“模型看到了 session 无法解释的内容”。

### 7.4 Stream fidelity

三个等级：

- Standard：stream started、first semantic chunk、counts/bytes、assembled message、terminal；
- Verbose：所有 normalized chunk metadata，正文进入受限 payload或按组件配置；
- Forensic：exact normalized chunks和可选 raw SSE frames，content-addressed/压缩/短保留。

若未来决定像 DeepSeek 一样把 raw normalized chunks放进 canonical session，必须先通过：JSONL 写入/packing benchmark、100 chunks/s UI/IO、crash tail、session size和隐私 ADR。第一阶段不直接改变主 ledger。

## 8. Ownership/Effect Tree

### 8.1 只读诊断结构

```rust
struct OwnedResourceSnapshot {
    resource_id: ResourceId,
    owner_id: OwnerId,
    kind: ResourceKind,
    label: String,
    state: ResourceState,
    created_at: InstantRef,
    correlation: CorrelationIds,
    children: Vec<OwnedResourceSnapshot>,
}
```

资源包括：registration、tracked task、channel endpoint、process、MCP transport、session writer、payload writer、child agent、timer/watchdog。不是所有资源必须存进一个全局 registry；每个 owner 实现只读 `diagnostic_snapshot()`，app 在用户请求时组合。

### 8.2 必须能回答

- 哪个 Agent/Plugin 拥有它；
- 何时创建、当前 phase；
- cancel/close/dispose 是否已请求；
- 为什么仍未结束；
- child resources；
- 最近错误；
- snapshot 是否一致/可能过期。

`tpi trace resources --session/--run` 在 shutdown/leak test 中比较前后资源树。

## 9. Runtime Invariant 系统

### 9.1 Contract

```rust
trait RuntimeInvariant {
    fn owner(&self) -> CapabilityId;
    fn seed(&mut self, durable_prefix: &[Envelope]) -> Result<(), InvariantViolation>;
    fn observe(&mut self, observation: &Observation) -> Result<(), InvariantViolation>;
    fn snapshot(&self) -> InvariantSnapshot;
}
```

实际 API 应支持增量/流式 seed，避免读全量 Vec。每个 invariant 注册有精确 disposer和 scope；session-backed checker 从 durable log重建，live-only checker 标明无法观察加载前操作。

### 9.2 首批 invariant

1. session turn/step/request/tool enclosure；
2. actual model request == durable reconstruction；
3. ToolCall requested/started/terminal exactly once；
4. write-ahead precedes recoverable effect；
5. registry disposer generation identity；
6. capability snapshot prompt/execute consistency；
7. inbox FIFO conservation和 answer routing；
8. agent phase legal transitions；
9. supervisor terminal后无 live owned task；
10. subagent start/end、budget、parent link；
11. projector incremental == rebuild（采样/测试）；
12. UI tool card terminal与 runtime terminal一致。

### 9.3 失败策略

| 环境/严重度 | 行为 |
|---|---|
| test/debug 任意 invariant | 立即测试失败，输出 minimal trace slice |
| production pre-effect safety invariant | 阻止 effect，run 以 internal invariant terminal |
| production post-effect/durability invariant | 停止后续 effect，优先写 recovery/incident，不 panic 丢现场 |
| presentation-only invariant | 隔离该 projection，显示诊断，业务 truth 不改 |
| observer 自身失败 | disable observer/sink，标 trace incomplete；不改变业务 |

Invariant violation 生成 incident bundle：相关 ledger range、trace span ancestry、resource snapshot、config provenance（脱敏）、error chain。不要自动上传。

## 10. 采集等级

```text
Off       仅 fatal stderr/最小 crash marker；不建议常态
Standard  默认：所有语义 span/decision/terminal，payload hash/size
Verbose   所有 normalized chunks/progress、详细 scheduler/context/UI transitions
Forensic  exact request/response/tool bodies/raw frames/有限 UI snapshots
```

### 10.1 配置维度

- global level；
- component filters：agent/context/provider/tool/session/ui/mcp/process/subagent；
- per-session/per-run override；
- payload policy；
- retention days/bytes/traces；
- local sink、incident spool、OTLP sink；
- sampling只允许高频 record，span terminal/error/invariant 永不采样。

切换 level 是 `TraceControlCapability` 的显式 command。进入 Forensic 时 UI 显示：可能记录 prompt、代码、命令和工具输出；作用范围与自动到期。secret 类字段即使 Forensic 也不记录 plaintext。

## 11. Local Flight Recorder

Standard 模式始终维护有界内存环：按 record count 与总 bytes 双重限制。发生 error/invariant/user “capture incident” 时：

1. 冻结错误前窗口；
2. 继续采集短的错误后窗口或直到 run terminal；
3. 写 incident manifest；
4. 关联 session range和payload refs；
5. 标出被逐出的起始记录和所有 gaps。

环形缓冲不是 durable truth；它只提高“问题发生前发生了什么”的可见性。大小通过 P0 benchmark 决定，禁止无界。

## 12. 本地存储、轮转与背压

建议布局：

```text
~/.tpi/diagnostics/
  traces/YYYY-MM-DD/<trace-id>.jsonl
  payloads/<blake3-prefix>/<hash>.blob
  incidents/<incident-id>/manifest.json
  index/               optional rebuildable catalog
```

### 12.1 Writer

- TraceSink 使用有界非阻塞队列；
- standard 队列满可丢高频 record但必须计 gap；terminal/error优先保留；
- forensic 要求完整时，队列容量不足应将该 trace 标 incomplete并停止接收大 payload，不能无限阻塞 provider/tool；
- writer 是 supervisor-owned task/thread，正常 shutdown drain/flush；
- crash 允许丢 trace tail，但 Ledger pre-effect barrier不受影响；
- 每行/record/depth/payload有上限；
- trace file append失败隔离到该 sink，写最小 stderr/incident marker。

当前 `tracing_appender::non_blocking` 默认是 lossy，并提供 error counter；新实现必须读取并呈现 dropped count，而不是使用默认后假装完整。`WorkerGuard` 应由 application owner持有并在正常退出 drop，不用永久 `Box::leak` 作为最终生命周期模型。

### 12.2 Retention

- 按总 bytes、天数、trace count 三重限制；
- 正在写、pinned incident、用户正在导出的文件不删除；
- 先删无引用 payload，再删 old trace；
- 删除是同一 diagnostics root 内的明确路径操作；
- CLI 支持 dry-run/list/prune；
- material deletion 告知可恢复性；
- forensic 自动短到期，除非用户 pin。

### 12.3 Payload

- BLAKE3 content address + length + media/schema；
- optional compression 需要 benchmark/size cap；
- dedupe 不跨不同 encryption/privacy domain；
- PayloadRef 记录 unavailable/redacted/pruned状态；
- secret 不因 hash去重而侧信道泄漏；secret默认不进入 payload。

## 13. 脱敏、隐私与导出

### 13.1 内置 fail-closed 基线

至少结构化删除：

- Authorization/Cookie/Proxy-Authorization；
- API key/token/password/private key/credential store values；
- process env 中非 allowlist 值；
- URL credential和敏感 query；
- MCP/server 自报字段中的 known secret keys；
- config secret references 的解析值。

正文、workspace paths、命令、tool output不是 secret，但属于 `WorkspaceContent`，远程导出默认 hash/redact。

### 13.2 两次脱敏

1. record construction：secret 永不进入内存 trace value；
2. sink/export projection：按 destination 再过滤 workspace/internal fields。

导出在 deep-copied record 上运行，不重写 local ledger/trace。规则失败时该 record fail closed，并写本地不含 payload 的 redaction error。

### 13.3 Sharing modes

```text
LOCAL_ONLY（默认）
INCIDENT_MANUAL（用户预览并选择 bundle）
OTLP_REDACTED（明确配置 trusted collector）
```

UI/CLI 始终能显示当前 sharing mode。不得用“emit成功”文案声称远端已收到；区分 queued、handed-off、exported/unknown。

## 14. 查询、Inspector 与回放

### 14.1 CLI

```text
tpi trace list [--session|--status|--since]
tpi trace show <trace-id> [--tree|--timeline|--json]
tpi trace follow [--session] [--filter]
tpi trace resources [--session|--run]
tpi trace explain <request-id|tool-call-id|event-id>
tpi trace doctor <trace-id>
tpi trace export <trace-id> --format jsonl|chrome|otlp-bundle
tpi trace replay <trace-id> --surface tui|json --no-effects
tpi trace prune --dry-run
```

`explain` 应生成因果摘要，例如：

```text
tool call X
  requested by assistant message/event E42
  resolved to builtin:edit registration R7 in snapshot S3
  allowed by policy P(strict) because workspace-relative write
  waited 14ms for path resource lock
  write-ahead committed at E44
  effect changed revision A -> B
  terminal committed at E45
  UI card projection applied E45 at render generation 881
```

### 14.2 TUI diagnostics overlay

- live span tree；
- current Agent phase/inbox/budget；
- active capability snapshot和来源；
- owned resource tree；
- recent warnings/errors/invariant；
- queue/drop/gap counters；
- jump to related transcript/tool/child/session event；
- copy redacted incident summary；
- 开关 component-level verbose trace。

默认主 transcript 不显示全部 debug detail，避免信息密度爆炸。

### 14.3 安全回放

默认 replay 是纯消费：

- 读取 ledger + trace/payload；
- 用 recorded provider chunks/tool outcomes 驱动 Agent/TUI projector；
- 禁止网络、文件写、process、MCP、child spawn；
- adapter若找不到 recorded outcome立即 fail，而不是回退真实调用；
- 比较 reconstructed request hash、event sequence、projection snapshots；
- 支持从指定 turn/step/event断点重放，但前缀先只读 fold。

“重新执行真实 effect”不是 replay；如未来需要，必须叫 `rerun`，使用新 session/trace和正常权限确认。

## 15. OpenTelemetry 位置

OpenTelemetry 是可选 exporter，不是 TPI 内部数据模型。内部先用 `tracing` spans + typed DiagnosticRecord；OTLP adapter 映射：

- Agent Run/Turn/Step/Provider/Tool/Child -> spans；
- structured diagnostic -> span events/log records；
- low-cardinality counters/histograms -> metrics；
- session/trace/request/tool IDs -> attributes，正文不默认导出；
- subagent/remote process -> span links/propagated trace context（协议支持时）。

调研时 OpenTelemetry Rust 各 signal/stability仍不同，`tracing-opentelemetry`/OTLP版本变化不能影响 core contract。OTLP feature默认关闭；启用时 batch exporter、queue、shutdown timeout、retry/loss都要显式配置和暴露状态。

## 16. 静态可观测性生成物

运行时 trace之外，CI 从代码生成：

1. Event producer/consumer matrix；
2. Capability definition/provider/consumer matrix；
3. Session event owner/projector matrix；
4. Tool pipeline stage owner表；
5. production spawn site/declared supervisor owner表；
6. config field owner/sensitivity表；
7. trace span/event catalog与字段schema；
8. runtime invariant registration coverage。

生成器必须使用 Rust syntax/metadata而非脆弱纯字符串猜测；初期可用显式 registry/catalog + verification script。生成文档只读，源 contract在代码。

## 17. 性能预算

P0 测基线后批准具体数字，至少度量：

- Standard tracing 对吞吐/TTFT/tool latency 的 P50/P95/P99 overhead；
- 100 chunks/s、10MB tool output 的 queue/drop；
- trace writer CPU/RSS/disk bytes；
- Payload hash/compression成本；
- inspector 读取 1GB traces 的分页内存；
- 10 小时 retention/prune；
- OTel collector unavailable 时应用性能和 shutdown；
- invariant checker每 event成本。

初始目标：Standard 不超过关键路径 5% wall-time退化；无法达到则先减少高频细节而不删除 semantic terminal/decision。Forensic 可以更贵，但必须有可见 overhead/磁盘预估和自动到期。

## 18. 实施 Track O0–O8

该 track 与 [08-migration-roadmap.md](08-migration-roadmap.md) 的主 Phase 同步，不另开 big-bang observability rewrite。

### O0：让现有 tracing 可信

- 修复 async function 中跨 `await` 持有 `Span::enter()` guard；改用 `Future::instrument`/`#[instrument]`/显式 parent。
- inventory 所有 span/event/trace writer；建立 correlation gap 清单。
- 测 WorkerGuard shutdown、loss counter和 provider trace同步写入开销。
- 验收：并发两 Run不会互相继承 span；测试用 subscriber验证 parent tree。

> **现状登记（2026-08-14，P0-09 完成时）**：唯一跨 await `span.enter()` 位于 `src/agent/mod.rs` 原 `run` 顶部，已改为 `Future::instrument`（`run` wrapper + `run_inner`）；`tests/trace_ancestry.rs` 用 capture layer 断言并发 run 的 `agent.run` enter 深度 ≤ 1（修复前 = 2）。
>
> provider trace（`src/provider/trace.rs`）风险登记：进程级旁路（`OnceLock<Option<Mutex<File>>>` 一次决定）；每次记录同步 `write_all + flush`（stream 每 SSE chunk 一次，慢盘放大延迟）；每 chunk 抢全局 Mutex；`TPI_TRACE_PROVIDER=body` 记录完整 request body（可含用户代码）；记录无 session/run/request/attempt 关联 ID（仅 ts_ms + kind）。main.rs 标准日志：rolling daily + non_blocking，WorkerGuard `Box::leak`，EnvFilter 默认 INFO，loss counter 未暴露。Standard 日志 secret canary：src 内 `tracing::*!` 无直接记录 api_key/authorization/password/secret/token 字段的调用点。上述问题由 O2（Local TraceSink）+ O3（request reconstruction）解决，O0 不重写。

### O1：TraceContext 与 catalog

- typed Trace/Turn/Step/Attempt IDs；
- span naming/field/sensitivity conventions；
- `TraceContext` 通过显式 request/context传播，不用全局 mutable current run；
- 生成 span/event catalog。

### O2：Local TraceSink + flight recorder

- typed TraceRecord/TraceSink；
- bounded queue、gap counter、manifest/completeness；
- supervised writer/retention；
- current provider trace adapter写入新 sink，环境变量保留一阶段兼容后删除。

### O3：模型请求重建

- request header snapshot + manifest；
- dispatch-time invariant；
- context contribution decisions；
- provider attempt/chunk metadata；
- safe payload mode。

### O4：Tool/Session/Capability/Resource instrumentation

- pipeline stage spans；
- ledger event links；
- registration/snapshot/effect tree；
- process/MCP/shutdown lifecycle；
- 首批 runtime invariants。

### O5：Inspector 与 incident

- `trace list/show/explain/doctor/resources`；
- invariant incident bundle；
- TUI diagnostics overlay；
- redacted manual export preview。

### O6：安全 replay

- recorded provider/tool outcome adapters；
- no-effects enforcement；
- request hash/event/projection parity；
- fuzz malicious/corrupt trace/payload。

### O7：Optional OTLP

- feature-gated adapter；
- sharing modes/built-in redaction；
- collector down/backpressure/shutdown tests；
- no raw payload default。

### O8：Subagent/remote propagation

- parent/child trace links；
- ACP/MCP trace context extension（协议允许时）；
- remote clock/process identity；
- combined timeline without assuming global order。

## 19. 验证矩阵

### Correlation

- 两个并发 sessions/runs/tools/children 不串 trace；
- spawned task显式继承或显式脱离 parent；
- retry attempts可区分且链接；
- durable event与 runtime terminal双向查找。

### Completeness

- normal run每个 open span有 close；
- crash缺 close并标 crashed；
- queue overflow产生 gap；
- payload删除/retention 后引用状态正确；
- sink失败业务继续，trace manifest不宣称 complete。

### Security

- seeded API keys/tokens/passwords/env/private key不出现在任何 level/export；
- workspace content只在批准 payload mode；
- redaction rule throw fail closed；
- export preview与实际 bundle hash一致；
- trace inspector处理escape/恶意JSON不执行。

### Invariant

- 每个 checker有 valid/invalid/replay/reload/dispose测试；
- 故意构造 model request desync在网络前失败；
- ToolCall duplicate/missing terminal、inbox丢失、illegal phase产生 incident；
- production failure policy不造成第二个副作用。

### Replay

- replay不发网络/不写 workspace/不spawn；
- recorded trace得到相同 request hash/session projection/TUI semantic snapshot；
- incomplete trace明确拒绝需要缺失payload的 byte-exact replay，但仍允许可完成的 semantic replay。

### Performance/soak

- Standard/Verbose/Forensic分别测 overhead；
- collector down、disk full、slow disk、queue flood；
- 10小时 rotation/prune；
- shutdown drain bounded且报告未flush数。

## 20. 禁止实现方式

- 在每个函数入口/出口机械打文本日志；
- 一个 `serde_json::Value` 全局 event bus；
- 用 trace 代替 session durable event；
- 同步 `write+flush` 每个 provider chunk；
- 默认上传 prompt、代码、命令和工具输出；
- 依赖 regex 才删除 Authorization/API key；
- 使用 wall clock 排并发因果；
- 跨 `await` 保持 `Span::enter()` guard；
- queue满静默丢而仍标 complete；
- replay悄悄回退真实 provider/tool；
- invariant catch 后仅写 warn继续执行危险 effect；
- 为“统一”把所有业务错误、trace event和metrics塞进同一枚举；
- trace文件/flight recorder/cache无限增长。

## 21. 完成标准

全链路追踪能力只有在以下全部成立时才完成：

1. 任意 request/tool/event可从一个 ID追到完整因果链；
2. 实际 model request在 dispatch前与 durable reconstruction一致；
3. 当前 capability/resource ownership可查询；
4. trace明确报告完整/丢失/脱敏/崩溃；
5. 默认模式不记录 secret、不远程上传 raw workspace content；
6. incident可在无网络环境本地生成、预览、导出；
7. replay默认无 effect且能复现关键 projection；
8. runtime invariants在错误跨越安全边界前失败；
9. Standard性能和长期磁盘/RSS满足批准预算；
10. OTel/backend移除不影响 session、Agent和本地诊断 contract。
