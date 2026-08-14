# 05. Session、Context、持久化与恢复计划

## 1. 核心决定

JSONL event log 继续是 session 事实源。大重构期间不把主存储迁到 SQLite/redb，也不把 UI transcript 或 provider request 变成事实源。

```text
Session JSONL (canonical durable facts)
    |
    +--> Conversation projection --> context policies --> provider messages
    +--> Transcript projection ----> UI view model
    +--> Plan/process projection ---> runtime restore
    +--> Catalog/search projection -> optional SQLite
    +--> Metrics/eval projection
```

## 2. 当前优势与拆分目标

当前 `session/mod.rs` 已有 typed `SessionEvent`、envelope、schema、序号、单写者、尾部恢复、protocol validation、compaction 和 repair。目标是分物理 owner，不重写语义：

```text
session/
  protocol.rs      durable domain event、envelope、schema version
  codec.rs         serde wire type、encode/decode、limits、compatibility
  store.rs         append/read/sync/lock、head cursor
  projection/
    conversation.rs
    transcript.rs
    plan.rs
    catalog.rs
  recovery.rs      incomplete run/tool resolution
  repair.rs        diagnose/quarantine/rewrite
  artifact.rs      bounded artifacts
  fixtures/        versioned golden corpus
```

`SessionEvent` 与 wire `EventBody/Payload` 的重复目前可以接受：前者是领域 API，后者稳定 wire。先拆文件并测试，只有重复真的产生 drift 时才生成 codec/宏；不要为了省类型立即把 wire annotations 污染 domain。

## 3. Durable event 资格

一条信息只有满足至少一项才进入 session log：

- 重启后恢复正确行为所必需；
- 已经或将要进入模型 context；
- 用户需要在历史中审计；
- 外部副作用 intent/receipt；
- 计费/预算/terminal state 需要可靠统计。

以下默认不 durable：每 token delta、spinner、hover/focus、临时 layout、私有 reasoning、可重建 cache、每帧工具 tail。

### 3.1 Commit point

每类事件要定义“何时成为事实”：

| 事件 | commit point |
|---|---|
| UserSubmitted | app 接受并准备启动/排队，append 成功后 UI 显示 committed |
| AssistantMessageCommitted | provider response 已完整规范化和协议验证 |
| ToolRequested | assistant carrier 与 call identity 已确定 |
| ToolStarted(recovery) | 副作用前，recovery metadata 已生成 |
| ToolCompleted | handler terminal + output/effect 已规范化 |
| InboxInserted/Claimed（未来） | queue mutation 成功；若会进入模型必须可 replay |
| ChildStarted/Completed（未来） | child owner/session 建立；structured report 提交 |
| RunCompleted | 所有本 run started effects 有 terminal/recovery 表达 |

任何 projector 只能消费 committed envelope。

## 4. Versioning 和兼容

### 4.1 版本策略

- envelope structural format 改变才 bump major `schema`；
- 新 event type/optional field 不必自动 bump major，但 reader 必须知道 ignorable/required 规则；
- required unknown event 默认拒绝读取，防止投影静默漏掉模型事实；
- 明确标记 ignorable 的 telemetry/UI hint 可跳过；
- writer 永远只写当前版本；reader 支持批准的历史窗口；
- 每个旧版本 fixture 永久只读保留。

建议在 Phase 2 设计：

```rust
struct Envelope {
    schema: SchemaVersion,
    seq: EventSeq,
    event_id: EventId,
    session_id: SessionId,
    run_id: RunId,
    timestamp: Timestamp,
    compatibility: EventCompatibility, // Required | Ignorable
    body: WireEvent,
}
```

不要只因为增加字段就写一个全文件 migrator。优先 decode old -> current domain。只有新 writer 必须重写历史才能正确 append 时，才做显式 migration copy。

### 4.2 迁移安全流程

1. 只读扫描所有 fixture/用户选择的 corpus，统计版本和错误。
2. 写 decoder golden tests；当前程序和新 decoder 输出同一 domain sequence。
3. 如需迁移，写到**新文件**，每行校验、fsync、重读验证。
4. 原文件 rename 为带版本和 hash 的 backup；替换必须同目录原子操作。
5. 迁移记录含 source hash、target hash、tool version、timestamp。
6. 失败时原文件仍可打开；不允许 in-place 半重写。
7. 提供 `tpi doctor sessions --dry-run` 与显式 `--migrate`，启动时不静默批量改写。

## 5. Store contract

### 5.1 单写者与乐观 head

```rust
append(expected: SessionHead, event) -> Result<CommittedEnvelope, AppendError>
```

`SessionHead` 包含 last seq/event id。即使仍用 OS exclusive file lock，也用 expected head 表达调用者假设，避免未来 UI、agent、child projector 误以为可并发写。

一条 session 一个 writer owner。其他组件通过 command/channel 请求 append，不各自打开文件。

### 5.2 Durability barrier

不是每条 token 都 sync。明确等级：

```text
Buffered: 可随下一 barrier 落盘的低风险事实
Commit: 在向用户/模型宣称完成前 sync
PreEffect: 外部副作用前必须 sync
Shutdown: 正常退出 flush/sync
```

实际映射到 `write_all/flush/sync_data` 需在 Windows/Linux fault test 验证。不要承诺文件系统无法保证的断电语义。

### 5.3 尺寸与损坏

- physical line、JSON depth、string、tool payload、artifact ref 有独立上限；
- incomplete final line 可按当前规则截断/恢复；middle corruption 不能静默跳过；
- repair 先 diagnose，输出 byte/line/event context，再 quarantine 原文件；
- artifact 写入使用 partial 临时文件，完成后才 publish；
- 不可信历史文本不得作为 terminal escape 直接输出。

## 6. Projector contract

每个 projector：

```text
initial_state()
apply(state, committed_event) -> state/error
version()
checkpoint/watermark（可选）
rebuild(log)
```

必须满足：

1. deterministic：相同 event sequence 输出相同结果；
2. prefix-safe：处理任意合法前缀不 panic；
3. incremental == full rebuild；
4. 不发外部副作用；
5. 对 required unknown event fail loud；
6. cache version 变化可丢弃重建。

Proptest 生成合法 event state sequences，比较 incremental/full projection。

## 7. Context assembly

### 7.1 分层

```text
durable messages
  -> ConversationProjection
  -> ContextPolicy pipeline
       1 identity/system sections
       2 current user/steering/injected context
       3 compaction coverage selection
       4 tool result pruning/artifact windows
       5 token measurement/budget
  -> ProviderMessageConverter
  -> ModelRequest
```

context pipeline 的每个 policy 输入/输出是 domain message，不是 OpenAI JSON。顺序显式、有 snapshot tests；不要用任意 hook 让插件重排安全不变量。

### 7.2 Context cache

cache key 至少包含：session head、branch head、model capability/tokenizer identity、system prompt revision、active tool definition revision、context policy version、current plan revision。

缓存只保存可重建 projection。cache 命中错误的风险高于 miss，因此 schema/policy 变化默认 miss，不做模糊兼容。

### 7.3 Compaction

保留“lossy projection，完整 JSONL 仍在”的原则。Compaction event 明确覆盖 event range、摘要生成模型/策略版本、token estimates、源 head。不能覆盖：

- 未终止 tool protocol；
- pending user input；
- active child/workflow；
- recovery-required write；
- 摘要所依赖但尚未 durable 的 delta。

摘要失败、缩减不显著、provider 不可用是不同结果。不要 catch 后假装已 compact。

### 7.4 Model request reconstruction 与追踪边界

每次实际 dispatch 的模型请求必须能从 committed session prefix 和稳定 header 重建，而不是只能依赖 provider debug body：

```text
RequestHeader
  model/provider identity
  effective non-secret model config
  system/context policy revisions
  ordered tool definitions + schema digests
  tokenizer/budget revisions

RequestManifest
  request_id / attempt_id / source event range
  header digest / message digests / payload sizes
  compaction coverage / completeness flags
```

dispatch 前冻结 adapter 将发送的 request，独立 projector 从 session prefix 重建预期 request；二者比较 header、消息顺序、tool schema 和 source references。差异是 runtime invariant failure，不能只打一条 warning 后继续，因为这意味着“恢复出来的上下文”与“模型真正看到的上下文”已经分叉。

边界规则：

- `RequestHeader` 中只保存重建所需的稳定、非 secret 配置；API key/header 永不进入 session；
- 默认 trace 保存 ID、digest、计数、尺寸和结果，不复制完整 prompt；
- raw SSE/chunks 属于可选、限量、可删的 payload sidecar，不属于 canonical session truth；
- telemetry 从 committed envelope 投影，live 与 replay 使用相同 projector；
- trace 丢失不影响 session commit，session corruption 则必须让重建失败。

字段和完整性契约见 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)。

## 8. Branching 与 fork

借鉴 Pi 的 session tree，但延后到基本 event/projector 边界稳定后。

### 8.1 最小模型

- event 已有 `EventId`；增加 session/branch head 指向某个 event；
- append 以当前 head 为 parent；
- branch 只创建新 head/metadata，不复制整个 log；
- transcript/context 从 head 沿 parent chain 投影；
- compaction summary 绑定 covered ancestry，不能跨不相干 branch 复用。

如果单文件 tree 使恢复/锁/查找复杂，可选择“fork 新 session + parent session/event reference”。先写 ADR 并用 100k events benchmark 两种方案。

### 8.2 必测序列

- 在 user/assistant/tool boundary fork；
- 禁止在未终止 tool call 中间 fork，或定义完整继承规则；
- branch A compact 后 branch B 仍读原历史；
- parent 删除/归档不破坏 child；
- child session ID 与 artifact namespace 不冲突；
- concurrent UI branch selection 不改变 active writer owner。

## 9. Optional SQLite derived index

只有明确需要 session 列表性能、全文搜索、filter/sort 时启用。

建议表仅作示意：

```text
sessions(session_id, workspace_id, head_event, created_at, updated_at, title, status)
messages(session_id, event_id, role, text_excerpt)
tools(session_id, call_id, name, status, duration_ms)
projection_meta(name, version, watermark)
```

规则：

- 写 session 成功不依赖 index 成功；
- index worker 读取 committed JSONL；
- index 有 generation/version，事务更新 watermark；
- 不一致时 drop/rebuild；
- UI 展示索引结果后，打开 session 仍以 JSONL 验证；
- 不把 secret、完整大工具输出或未经处理内容重复复制到 FTS。

## 10. Session 与子代理

每个 child 默认有独立 session truth，parent log 只记录：

- child identity/provider/purpose/parent relation；
- delegated task 的有界摘要或 artifact ref；
- budget；
- terminal structured report/status；
- 用户可见的 approval/interaction transfer。

parent 不复制 child 全部 tool chatter 到自己的 context。用户需要深入时，通过 UI 打开 child session。

## 11. 测试矩阵

### Codec/golden

- 所有已发布 schema fixture decode；
- encode current -> decode equality；
- unknown required/ignorable；
- oversized line/depth/string；
- CRLF/LF、Unicode、截断尾部、中间 corruption。

### Protocol/state

- tool requested/started/completed cardinality；
- assistant carrier 与 tool calls 关系；
- run terminal 前无悬挂 effect；
- input requested/received 配对；
- inbox/child events 的合法序列。

### Crash matrix

在 append 前、write 后 sync 前、pre-effect sync 后、effect 后、terminal append 前、terminal sync 后注入 crash。重启结果只能是：确认未发生、确认已提交、或明确 unknown/recoverable；不能重复副作用或编造成功。

### Projection

- incremental == rebuild；
- compaction 与 uncompacted context 的关键事实等价；
- cache 丢弃后结果一致；
- branch ancestry；
- large log memory/time budget。

### Request/telemetry reconstruction

- 同一 committed prefix 的 live telemetry 与 replay telemetry 语义等价；
- frozen provider request 与 session-derived request manifest 等价；
- config/tool schema/context revision 漂移分别给出可定位 diff；
- Standard trace 无 prompt/tool body；Forensic payload 截断、脱敏、gap 均显式；
- trace sink failure/drop 不改变 session seq、模型请求或 run outcome。

## 12. 分步实现顺序

1. 建立用户/测试 session golden corpus 和只读 inspector。
2. 从 `session/mod.rs` 机械拆 `protocol` 与 `codec`，零 wire change。
3. 抽 `SessionStore` port，现有 JSONL 实现不变。
4. 把 conversation/transcript/plan projection 变成纯 projector。
5. 让 Agent 只依赖 store/projector ports，删除文件细节。
6. 引入 versioned projector/cache watermark。
7. 在需要时加入 inbox/child durable events。
8. 加 request header/manifest projector 与 dispatch-time invariant；先 shadow compare，不改请求。
9. branch spike；验证后再正式加入。
10. 只有指标支持时加入 SQLite derived index。

任何一步失败都能回到前一步的 JSONL reader；禁止先改 writer 再补 old fixtures。
