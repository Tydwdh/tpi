# 08. 从当前代码到落地的迁移路线

## 1. 路线总览

```text
P0 Baseline/Fences
  -> P1 Vocabulary + dependency direction
  -> P2 Session ports/projectors + supervisor
  -> P3 App/runtime/TUI event separation
  -> P4 Tool registry/pipeline/capability scope
  -> P5 Provider/context/workspace adapters
  -> P6 TUI components/scroll/input/performance
  -> P7 Cargo workspace + external extension SDK/protocol
  -> P8 Inbox + subagents
  -> P9 Branch/index/advanced capabilities (evidence-gated)
  -> P10 Compatibility removal + stable release
```

这是一条依赖顺序，不是强制单线程。只允许并行处理不共享 owner/文件、验收互不依赖的任务。任一时刻在制结构迁移最多两个，防止兼容层叠加。

每个任务建议 1 个行为变化或 1 个机械移动，目标 diff 小于约 400 行业务代码（测试/fixtures 可额外）。超过时先拆任务，不为了数字强拆高内聚函数。

### 1.1 横切 Observability Track

调试能力不能等 P10 才补。以下 O-track 与主阶段同行，完整规范见 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)：

| Track | 最早阶段 | 结果 | 阻塞条件 |
|---|---:|---|---|
| O0 trace integrity baseline | P0 | 修正 async span ancestry；记录现有 trace 开销、隐私和缺口 | parent/child 关系测试不过 |
| O1 identity + catalog | P1 | typed IDs、TraceRecord envelope、span/event/sensitivity 目录 | 新事件未分类、同一词多义 |
| O2 local sink + flight recorder | P2 | bounded queue、rotation、drop/gap、owned flush guard、异常窗口 | sink 故障影响 run/session |
| O3 ledger telemetry + request invariant | P2/P5 | live/replay projector、RequestHeader/Manifest、dispatch compare | 模型实际请求不可重建 |
| O4 capability/effect ownership | P4/P5 | registration/tool/process/task 生命周期树与 package-owned invariants | terminal 后仍有 orphan |
| O5 inspector + incident | P6 | timeline/tree/filter/detail、incident bundle、只读资源树 | inspector 产生业务副作用 |
| O6 safe replay | P7 | no-effect adapters、manifest、typed scrub、离线导出预览 | replay 可触发 effect 或 secret canary |
| O7 optional OTel adapter | P9+ | feature-off exporter，与本地合同解耦 | collector/privacy/backpressure 未验证 |
| O8 subagent/remote propagation | P8 | parent/child trace link、remote boundary/gap | child 无 lineage/terminal |

O-track 不是第二套 event bus。它只观察既有 command/event/effect/owner 边界；任何为了“方便打点”而改变业务顺序、添加全局 mutable registry 或同步 flush 热路径的实现都不合并。

## 2. 全阶段 Definition of Done

每个任务完成前必须：

1. issue/任务卡写触发条件、根因、正确行为、范围和非目标；
2. 先有失败测试或明确 characterization；
3. 只修改声明文件；发现额外问题新建任务，不顺手扩张；
4. formatter/check/clippy/相关测试通过；
5. review `git diff --check`、新增依赖、public API、错误/日志/secret；
6. 更新 ADR/本文状态和兼容层删除条件；
7. 给出回滚方法；
8. 若改 UI，记录人工矩阵结果；若改性能，给 before/after 原始数据。
9. 新增或改变异步/IO/lifecycle 边界时，更新 trace catalog、敏感度和因果关系测试；不得用日志存在性代替行为测试。

阶段完成还需全量 gate 和 migration rehearsal。

## 3. Phase 0：冻结事实与安全网

### 目标

在移动任何 owner 前建立可重复基线、架构 gate 和真实 session/terminal fixtures。

### Entry

- 选择干净基线提交；保存/提交当前用户修改。
- 确认支持平台、Rust MSRV、默认 feature 和 session 兼容窗口。

### 任务

#### P0-01 建 baseline manifest — **DONE（2026-08-14，见 [baseline-2202887.md](baseline-2202887.md)）**

- 新增 `docs/refactor/baseline-<commit>.md` 或机器可读 JSON。
- 记录 commit、rustc/cargo、OS/terminal、features、依赖树摘要、测试数量、失败项。
- 运行 `cargo metadata`、`cargo tree -d`、`cargo fmt --check`、check/clippy/test/doc test。
- 验收：另一台 CI 机器能复现命令；环境缺失和代码失败分开。

#### P0-02 修 remote fixture 的 `cygpath` 隐式前提 — **DONE（2026-08-14）**

- 先加 Windows test 证明没有 `cygpath` 时 fixture setup 失败的当前行为。
- 用 Rust 路径逻辑或显式 capability probe 修根因；不要逐个 skip case。
- 验收：当前受影响的 `agent_remote`、`remote_bash`、`remote_contract`、`remote_files`、`remote_traverse` 全通过；有 Git Bash/无 Git Bash 都有明确行为。
- 回滚：仅恢复 fixture adapter，不改 production remote semantics。
- 实施：`tests/fixtures/remote_server.rs` 新增导出 `win_to_posix()`（纯 Rust，`cygpath -u` 最小等价：盘符→`/c`、UNC→`//`、POSIX 原样），server 与 4 个 remote 测试改用同一转换；新增 4 个黄金对拍单测。5 个 remote target 全部通过（agent_remote 6 / remote_bash 9 / remote_contract 10 / remote_files 10 / remote_traverse 9）；fmt/clippy 清洁。无 Git Bash 环境不再依赖 cygpath（生产侧 `cygpath -w` 已有 fallback，不在本轮范围）。

#### P0-03 建 session golden corpus

- 收集每个 schema/特殊 lifecycle：普通对话、纯 tool call、stream interrupted、write crash recovery、compaction、awaiting input、corrupt tail/middle。
- fixture 必须 scrub secret/path/user text；保存 expected domain events/context/transcript。
- 验收：current reader replay 全过；fixture 文件有 hash/来源说明。

#### P0-04 建 recorded UI trace corpus

- 输入 trace：key/mouse/resize/paste/focus；runtime trace：assistant/tool/process/input。
- 固定 terminal sizes 和 expected semantic state/buffer snapshot。
- 验收：现有 TUI 可 replay；不依赖 wall clock/network。

#### P0-05 建性能与资源基线

- fixtures：1k/10k messages、1MB streaming message、100 tool cards、10h 模拟 append。
- 指标：session replay、context build、Markdown/layout/render、keypress latency、RSS/cache/task/process counts。
- 验收：结果存 artifact，不要求先优化。

#### P0-06 依赖卫生 — **DONE（2026-08-14）**

- 引入 cargo-deny 配置、unused dependency 检查、`cargo tree -d` review。
- 单独确认并处理疑似未使用 `russh-keys`；不得夹带版本大升级。
- 验收：licenses/advisories/sources policy 有带到期日的例外。
- 实施：`cargo machete`（0.9.2）确认 `russh-keys` 是唯一未使用依赖；源码扫描 src/tests 零直接 use；russh 0.62.6 自带 keys 模块（ssh-key =0.7.0-rc.11）且不依赖 russh-keys。删除后重复栈消失（ssh-key 0.6.7 / aead 0.5.2 / ed25519 3.0.0）；SSH 测试全绿；machete 复查零未使用。cargo-audit 已在 CI（audit job）。cargo-deny 未引入（licenses/advisories 无现存例外，P0-06 验收项待 P10-04 统一补，不阻塞）。

#### P0-07 架构依赖 gate v1 — **DONE（2026-08-14）**

- 用小脚本/测试禁止 `tui -> app` 新引用、`agent -> tui` 新类型、更多 `global_registry()` 调用。
- 初始可对既有违规做精确 allowlist；每消除一项即收紧。
- 验收：故意加违规 import 时 CI 失败且提示可理解。
- 实施：`scripts/arch_gate.sh`（rg 扫描 + 路径规范化 + 精确 allowlist "path|needle"，只减不增）。R1 tui->app 1 处、R2 agent->tui 6 处、R3 global_registry 9 处登记。CI check job 新增 `bash scripts/arch_gate.sh` 步骤。正反例验证通过：基线 OK；故意注入 3 个违规文件（src/tui、src/agent、src 根各一）全部被拒且提示含规则/文件/行。反例验证后临时文件已删除。

#### P0-08 批准 ADR-001/003/006

- microkernel/DAG、JSONL truth、task ownership。
- 验收：每个 alternatives/consequences/rollback 完整。

#### P0-09 O0 trace integrity baseline

- 盘点 `src/main.rs` logging guard、`src/provider/trace.rs` 同步 JSONL 和全部 `span.enter()`。
- 为现有 `agent.run`/provider/tool 路径写 subscriber capture test，证明 `.await` 前后 parent/child 关系。
- 用 `Future::instrument`/`#[instrument]` 修复跨 await enter guard；不顺便改业务状态机。
- provider trace 只做测量和风险登记，本任务不先重写；记录每 chunk 写/flush、mutex wait、body 敏感字段和现有无关联 ID。
- 验收：异步 ancestry 测试绿色；Standard 日志 secret canary 为零；before/after 开销有数据。

### Exit gate

- 全量测试除明确 real-API opt-in 外绿色；
- baseline/corpus/perf artifact 可复现；
- O0 trace ancestry/secret baseline 可复现；
- 代码行为零有意变化（除 fixture 根因和依赖卫生）；
- 任一失败可回滚单 PR。

## 4. Phase 1：统一词汇与逻辑边界

### 目标

先在单 crate 内建立 domain/runtime/view 边界和依赖方向，不拆 Cargo package。

### 任务

#### P1-01 Run/Turn/Step/Attempt 词汇审计

- 标注所有字段/事件/日志/测试中 `turn/run/retry` 的真实含义。
- 新增 typed IDs（只在确有混用处）和文档，不大规模 rename。
- 一次迁移一个歧义点；保持 session wire 名称。
- 验收：state transition table 与现有 tests 对齐。

#### P1-02 建 domain message/content

- 新类型与现有 `provider::ChatMessage` 并存；写双向 test adapter。
- session projection 先输出 domain message，provider converter 再生成旧 ChatMessage。
- 验收：所有 golden model requests byte/semantic 等价；provider-specific field 不进入 domain。

#### P1-03 拆 live runtime 与 view event

- 建 UI-agnostic event；先迁一个事件（如 phase/usage），app projector 生成旧 TUI event。
- 每 PR 迁一组：assistant、tool lifecycle、process、plan/input。
- 验收：headless sink 可直接消费所有 runtime events；无 UI drain task需求的 contract test。

#### P1-04 消除 `tui::reducer -> app`

- 移动 preview 纯投影到 presentation owner。
- 先锁定输出 tests；只改 dependency，不改预览文案。
- 验收：架构 gate 删除该 allowlist。

#### P1-05 分解 Config 输出

- 保留一个 resolver；引入 domain-specific resolved views。
- 每次只让一个 owner 接收窄 config。
- 验收：字段级 merge/unknown rejection 全部保持；default snapshot 不变。

#### P1-06 main/app composition inventory

- 把 object construction 与 use case 标注；不立即移动复杂 loop。
- 建 `AppServices` 显式字段，禁止 service locator。
- 验收：测试可用 fake ports 构造最小 controller。

#### P1-07 O1 typed trace identity/catalog

- 定义 `TraceId/SpanId/SessionId/RunId/TurnId/StepId/AttemptId/RequestId/ToolCallId/RegistrationId/ChildId`，只在真实边界注入。
- 建 schema/versioned `TraceRecord` 和 span/event/sensitivity/completeness catalog；生成 producer/consumer 文档。
- 先用 adapter 映射旧 tracing fields，禁止业务模块直接依赖 exporter DTO。
- 验收：catalog 无孤儿 name、每个 payload 有 sensitivity/default mode/owner。

### Exit gate

- `agent -> tui` 引用清零；`tui -> app` 清零；
- domain message -> provider request parity；
- session wire 零变化；
- O1 catalog、typed identity 与现有 session wire adapter 对齐；
- single-crate build/test/perf 无显著退化。

## 5. Phase 2：Session ports、纯投影与结构化任务

### 目标

让 Agent 不知道 JSONL 文件细节，application/agent tasks 有统一 owner。

### 任务

#### P2-01 机械拆 session protocol/codec/store

- 仅移动代码与 visibility；每次一个 module。
- 用 golden hash 证明 encode 无变化。
- 验收：`git diff` 无无关 format；old corpus 全过。

#### P2-02 `SessionStore` port + JSONL adapter

- 从当前真实调用抽最小接口。
- current `SessionLog` 先实现；Agent 通过 port。
- 验收：in-memory fake 跑 agent_flow；单写者/seq/recovery tests 对 adapter 运行。

#### P2-03 conversation/transcript/plan projector

- 分别建立纯 `apply/rebuild`；现有 `Conversation` 做 facade。
- 属性测试 incremental == rebuild。
- 验收：失败 run refresh 行为与 golden 一致。

#### P2-04 durability barrier 类型化

- 让 pre-effect/commit/shutdown sync 意图显式，不改实际刷盘时序。
- fault injection 覆盖每个 barrier。
- 验收：recovery matrix 不退化。

#### P2-05 `Supervisor` walking skeleton

- 先拥有一个无害 background task，验证 cancel/close/wait/aggregate error。
- 使用 CancellationToken + TaskTracker；建立 leak test。
- 验收：100 次 start/shutdown 后 tracked tasks 为 0。

#### P2-06 迁 Agent watchdog/tool forwarder

- 一次迁一个 spawn；删除直接 abort 或在 bounded hard-stop 后 await。
- 验收：cancel at every await；terminal event ordering。

#### P2-07 MCP/process owner inventory

- 只把 owner 不明确的 task 纳入相应 supervisor，不改协议。
- 验收：shutdown 后 child process/tree、channel、registration 清零。

#### P2-08 O2 local trace sink

- application 持有 writer/flush guard；建立 Standard 本地 JSONL/segment sink。
- queue 按 records+bytes 有界；溢出产生 counter 和后续 `TraceGap`，terminal/invariant 走保留通道。
- 建有界内存 flight recorder；只有 invariant/fatal/user trigger 才冻结异常前后窗口，正文仍服从模式与脱敏。
- rotation/retention/abnormal-exit recovery；sink error 只降级观测，不改变 session/run。
- 验收：故障注入、慢盘、queue full、shutdown deadline 和损坏尾部测试。

#### P2-09 O3 session telemetry projector skeleton

- committed event -> telemetry record 的纯 projector；live 和 prefix replay 共用实现。
- 用 `(session_id,event_seq,projector_version)` 去重；handoff 允许重复、不允许无声明缺口。
- Standard 不含正文；Verbose payload 走独立 sidecar reference。
- 验收：任意合法 prefix、incremental == replay、sink drop 不影响 append。

### Exit gate

- Agent 只依赖 `SessionStore`/projector API；
- session golden/crash suites 全过；
- production spawn 清单每项有 owner；
- shutdown leak tests 绿色。
- local trace sink 可限界、可 flush、可声明 gap；live/replay telemetry 等价。

## 6. Phase 3：Application controller 与 Surface 解耦

### 目标

把 `app.rs` 拆为 composition、use cases、terminal adapter、view projection；支持真正 headless。

### 任务

#### P3-01 定义 `UiIntent/AppCommand/AppEffect`

- 收集所有 key/mouse/slash action；按语义建 enum。
- 旧 event pump adapter 把新 command 转回旧函数，保持行为。
- 验收：recorded input trace 的 command sequence 固定。

#### P3-02 建 `AppController`

- 先迁 `start/cancel run` 一个 use case。
- controller 接收 ports、返回 events/effects；不引用 Crossterm/Ratatui。
- 逐个迁 resume/session/input answer/config commands。
- 验收：fake runtime/session/platform 的 integration tests。

#### P3-03 Slash command registry

- 仅把命令 parse/dispatch 从 giant match 移到 typed command definitions；危险命令保留确认 policy。
- 命令 registration 暂时静态，不急于开放第三方。
- 验收：help/completion/dispatch 来自同一 snapshot；旧命令 golden。

#### P3-04 Platform effects adapter

- clipboard/open URL/terminal title/file picker 等通过 `AppEffect`。
- error 反馈回 controller，不 `let _ =` 静默失败。
- 验收：Windows/Linux fake + 少量人工。

#### P3-05 Headless JSON surface

- 直接订阅 semantic runtime/durable terminal，不创建 TUI channel/drain task。
- 定义 versioned JSON output；取消/await input 明确退出码/事件。
- 验收：与 TUI 对同一 fake provider 得到等价业务终态。

#### P3-06 Terminal input adapter ownership

- 先给现有 blocking thread 添加 start/shutdown/join contract。
- EventStream spike 延后到 Phase 6，避免同时改 app 和 paste。
- 验收：退出/terminal error/ctrl-c 不遗留线程。

### Exit gate

- `app.rs` 只保留薄 facade/composition 或显著缩小；
- headless 无 TUI drain workaround；
- platform effects 可 fake；
- recorded input/app flow parity。

## 7. Phase 4：工具 capability 与统一 pipeline

### 目标

消除全局 registry、ABA 和 tool-name protocol；builtin/external 共享 contract。

### 任务

#### P4-01 RegistrationId 最小修复

- 先写 ABA red test；entry/id 精确删除；重复名规则显式。
- 不同 PR 再加 override API。

#### P4-02 composition root 注入 registry

- 为现有 constructor 增 registry 参数；测试用 fresh registry。
- 新调用禁止 `global_registry()`，逐 consumer 迁移后删除。

#### P4-03 immutable `ActiveToolSet`

- prompt descriptors/execute lookup 共用 snapshot。
- Step 内 reload stability test。

#### P4-04 分离 ToolDefinition/Handler

- builtin adapter 保持 facade；schema/origin/execute 不改。
- registration 时验证 name/schema/limits。

#### P4-05 pipeline skeleton

- 显式 stage result，先包一个 Pure builtin；再 read、process、write。
- 每次迁移后跑 existing scheduler/recovery suites。

#### P4-06 canonical output

- 从无副作用工具开始；投影回现有 ToolOutcome 保持模型/session/UI。
- bounded diagnostics/artifacts/output schema。

#### P4-07 替换四个 tool-name protocol

- 顺序：workspace effect -> plan -> request input -> web finalizer。
- 每项独立 typed directive/effect test，删除对应 name branch。

#### P4-08 scoped overlays/setup transaction

- root/session/agent lookup；setup fault rollback。
- 默认静态 roster snapshot 保持。

#### P4-09 MCP generation reload

- private setup -> atomic publish -> drain old -> precise dispose。
- 新 setup 失败不破坏 old active。

#### P4-10 conformance suite

- builtin/fake MCP/fake extension 共用；覆盖 cancel/policy/output/ordering/reload。

#### P4-11 O4 capability/tool invariants

- 每个 pipeline stage 产生 paired start/terminal，关联 capability/registration/tool call IDs。
- registry/pipeline owner 注册只读 invariant：snapshot definition == execution lookup、policy before effect、exactly one terminal、dispose 精确匹配。
- 暴露只读 registration/effect snapshot，不允许 inspector 修改 runtime。

### Exit gate

- `global_registry` 删除；
- runtime 特殊工具名 match 清零；
- builtin/external scheduler 来源分支清零或只存在 adapter 内；
- write recovery crash matrix、MCP tests、tool contracts 全绿。
- invariant companion 能定位故意注入的 registry/pipeline 违规，terminal 后无 effect orphan。

## 8. Phase 5：Provider、Context 与 Workspace adapters

### 目标

支持第二 provider/子代理前先稳定 object-safe model port；让 workspace transport 不泄漏进工具。

### 任务

#### P5-01 normalized model stream handle

- OpenAI adapter 先实现；移除 caller-supplied UI channel。
- retry/interrupted semantics 用现有 tests characterization。

#### P5-02 provider conformance suite

- fake adapter + OpenAI recorded adapter；protocol/error/cancel/usage。
- 再实现第二个真实 provider 或独立 mock wire adapter，证明接口不是单实现自画像。

#### P5-03 provider registry/model catalog

- typed provider/model IDs；resolved capabilities/context limits。
- scope 和 secret references；不做热切换直到真实需求。

#### P5-04 context policy pipeline

- domain message -> policies -> provider converter；顺序显式。
- tool definition revision、plan、compaction、token measurement 纳入 cache key。

#### P5-05 Workspace ports

- 从 read/write/bash/process 的真实 consumer 分别抽窄接口。
- Local/Remote adapter 跑同一 contract；不建 mega VFS。

#### P5-06 process lifecycle 纳入 supervisor

- foreground/background/remote cancel terminal 对齐。
- output tail/backpressure/kill tree。

#### P5-07 policy profile

- actor/effect/resource/interactive 输入；Allow/Deny/RequireApproval。
- 先保持 current default，新增显式 strict profile。

#### P5-08 O3 request reconstruction + provider spans

- committed prefix + `RequestHeader` 重建 `RequestManifest`；实际 frozen adapter request dispatch 前 shadow compare。
- request/attempt/retry/stream/tool-fragment/usage/terminal span 关联；raw chunk 仅 Verbose/Forensic payload。
- provider secret/header/body typed scrub；transport gap、retry discontinuity、partial UTF-8 明确记录。
- 验收：recorded provider fixtures 可逐字段定位 header/message/tool schema drift；compare 失败在开发/CI fail loud。

### Exit gate

- Agent crate/module不依赖具体 provider/workspace；
- 两个 adapter implementation 通过同一 suite；
- local/remote tool semantics parity；
- no UI channel in provider。
- 每个模型请求可由 session prefix 重建并与真实 dispatch manifest 对比。

## 9. Phase 6：TUI 组件与性能

### 目标

在 semantic event/app boundary 稳定后组件化，不改变 Agent/Session。

### 任务

#### P6-01 FocusStack/OverlayStack

- 迁一个 overlay，recorded trace；逐个迁。
- property test focus always visible/enabled。

#### P6-02 LayoutPolicy

- Ratatui Flex/Constraint；0/1/narrow/wide property tests。
- 不引入 Taffy。

#### P6-03 Transcript component + semantic anchor

- 保留 old offset adapter；双模型对 recorded scroll traces。
- append/resize/expand/search tests。

#### P6-04 render cache/cadence

- 先 measurement，再按 entry revision/width/theme cache。
- coalesce deltas、terminal never dropped。

#### P6-05 Editor component

- 集中 grapheme/cell mapping；引入 unicode-segmentation/linebreak。
- 自有 vs tui-textarea spike，行为优先于代码量。

#### P6-06 Crossterm EventStream spike

- Windows IME/paste/repeat/exit parity；不通过就保留 owned thread。

#### P6-07 renderer strategy spike

- current main/alt behavior characterization；只在用户需求明确时双 renderer。

#### P6-08 long-run/manual matrix

- 10h simulated/soak；Windows Terminal/SSH/Linux。

#### P6-09 O5 trace inspector/incident

- TUI/CLI 提供 timeline、ownership tree、ID/filter、gap/completeness、payload consent 状态。
- inspector 读取冻结 snapshot/segment，禁止通过 debug UI 触发 tool/session mutation。
- 读取 O2 flight recorder；incident bundle 关联 invariant、resource snapshot、segment/gap，不复制未授权 payload。
- 验收：100k records 分页/筛选不卡主 TUI；历史滚动不被 trace append 拉到底部。

### Exit gate

- scroll/focus/Unicode/input invariants 全绿；
- P95 input/render 与基线不退化；
- app/TUI/Agent dependency DAG 达标；
- 人工矩阵签字。
- inspector 对 incomplete/redacted/dropped 数据不伪装完整，且不产生业务 effect。

## 10. Phase 7：物理 crate 与扩展边界

### 目标

把已稳定逻辑边界变成 Cargo workspace；发布最小扩展 SDK/协议。

### 任务

#### P7-01 依赖 DAG dry run

- 使用 cargo metadata 脚本模拟 crate edges；检查 public 类型归属和 cycles。
- 连续两个阶段无反向 import 才开始。

#### P7-02 依次拆 crate

- 顺序：core -> session -> capabilities -> agent -> TUI -> adapters/CLI。
- 每个 PR 只拆一个 crate；零行为变化；必要 re-export 有删除阶段。

#### P7-03 feature audit

- platform/provider/remote features 由 composition crate控制；禁止 feature unification 意外启用高权限能力。
- `cargo hack` 或等价 feature matrix。

#### P7-04 public SDK 最小化

- 只公开 Tool/Provider/Subagent contracts 与 DTO；internal modules sealed。
- semver policy、MSRV、examples、conformance harness。

#### P7-05 MCP SDK parity spike

- 当前 client vs 官方 Rust SDK；选择一个，迁移有 wire fixtures。

#### P7-06 ACP/Wasm decision spikes

- 必须有真实 consumer；否则写“延后”ADR，不加依赖。

#### P7-07 O6 safe replay/diagnostics bundle

- `doctor --bundle` 先生成 manifest/预览，再经用户确认写出。
- typed sensitivity waterfall：drop -> hash/tokenize -> truncate -> allow；逐字段规则可测试。
- secret canary corpus、路径/用户名归一化、payload 引用缺失/gap 报告。
- 外发 exporter 与本地 capture 分开授权；默认 bundle 不含正文/env/credential。
- recorded provider/tool outcome + no-effect workspace/process/session adapters；replay 时禁止真实网络、进程、写盘和 append。
- 对 event/projection/request digest 做 parity；损坏/恶意 trace 只返回诊断，不 panic 或执行 payload。

### Exit gate

- workspace 全 feature matrix build/test；
- core dependency tree干净；
- public SDK 示例和 semver checks；
- 无永久 re-export shim。
- diagnostic bundle 通过 secret canary，manifest 能说明所有省略、脱敏与缺口。

## 11. Phase 8：Inbox 与子代理

### 目标

在 supervisor/scope/session/runtime event 成熟后增加可选子代理。

### 任务

#### P8-01 next-step/next-turn inbox

- 先 fake Agent state tests，再迁 pending messages。
- receipt、queue limits、cancel release、durable claim。

#### P8-02 typed answer/approval lifecycle

- request ID、expiry、answer route；普通 composer不能误投。

#### P8-03 SubagentProvider + fake

- contract/conformance；fresh child session/report。

#### P8-04 in-process read-only child

- concurrency 1、depth 1、default off；parent cancel。

#### P8-05 bounded parallel children

- global/per-parent semaphore、source order、rate fairness、output cap。

#### P8-06 child TUI

- summary card/tree/details/cancel；不把 raw stream默认灌主 transcript。

#### P8-07 isolated worktree provider

- 只有只读版本稳定且用户明确需要并行写时；创建/验证/cleanup/recovery。

#### P8-08 ACP/subprocess external provider

- handshake/capability/trust/disconnect；同一 suite。

#### P8-09 O8 parent/child trace links

- child 新 trace + parent span link；durable lineage 与 trace link 双向引用。
- 跨进程只传 opaque correlation context；不能传播时标 `remote_boundary`/gap。
- parent cancel、child terminal、report commit 的因果链可查询。

### Exit gate

- default single-agent path性能/工具 snapshot 不变；
- 100 次 spawn/cancel 无泄漏；
- budget/security/workspace isolation tests；
- UI/用户可取消、可审计。
- 从 spawn 到 report 的 trace/session lineage 完整；外部断点被诚实标记。

## 12. Phase 9：证据门控的高级能力

这些任务不能因为前面顺利就自动开始：

- P9-01 session branching/fork；
- P9-02 SQLite derived catalog/FTS；
- P9-03 Wasm component extensions；
- P9-04 user-defined workflow DAG；
- P9-05 recursive subagents depth > 1；
- P9-06 dynamic capability/profile reload。
- P9-07 O7 OpenTelemetry exporter（仅在确有 collector/远程诊断需求时）。

每项需独立 ADR，包含真实用户场景、现有方案失败证据、威胁模型、性能/可用性验收和卸载方案。

## 13. Phase 10：清理与稳定发布

### 任务

#### P10-01 compatibility inventory burn-down

- 搜索 `legacy/compat/deprecated/TODO(remove)` 和 allowlist；逐项列 consumer、删除 test。
- 没有 consumer 的 adapter 删除；不留双写。

#### P10-02 session migration rehearsal

- 用户选择的副本 corpus；dry run、migrate、rollback、old binary read behavior。

#### P10-03 performance/resource comparison

- 对 P0 fixtures 逐项 before/after；解释任何退化并批准预算。

#### P10-04 security/dependency review

- deny/advisory/license/source、secret scrub、path/SSRF/process/subagent threat tests。

#### P10-05 UX release candidate

- 全人工矩阵；新手/熟练用户关键任务动作数；docs/help/keymap一致。

#### P10-06 stable API/release notes

- session compatibility、config changes、feature defaults、known risks、downgrade路径。

#### P10-07 tracing compatibility/performance sign-off

- 验证 Standard 默认长期运行开销、retention、异常退出恢复和 shutdown flush。
- 检查 span/event schema、diagnostic bundle 与 optional exporter 兼容窗口。
- 运行 secret canary、trace replay、invariant fault、100k inspector 和无观测后端降级测试。

### Exit gate

- 旧 facade/全局状态/特殊名协议清零；
- 所有成功标准有 evidence link；
- 两个 release cycle 无 dependency backflow；
- 发布后停止结构迁移，回归常规小步演进。
- 默认 Standard 模式不会泄露正文，trace gap/drop 可见，禁用 tracing 时业务语义完全等价。

## 14. 回退策略

### 14.1 普通任务

通过 revert 单 PR 回退；fixture/测试可保留。禁止用 `git reset --hard` 处理共享工作树。

### 14.2 双路径迁移

若旧/新 projector/runtime 短期并存：

- 用 config/dev flag 选择 reader/renderer；
- 只允许一个 writer/side-effect path；
- shadow 新路径只读比较；
- mismatch 记录 bounded diagnostic，不自动修数据；
- 切换后保留一个 release 的读取 fallback，随后删除。

### 14.3 Session/protocol

永远先备份/新文件写/重读验证再原子替换。发布 notes 明确 downgrade 是否只能读取旧 backup。任何 migration failure 停止该 session，不继续写半迁移文件。

### 14.4 外部 capability

新 MCP/ACP/Wasm provider 以 profile/feature disable 回退；root registry 和 builtin path 不依赖它启动成功。

## 15. 禁止合并信号

出现任一项必须停止：

- 测试通过依赖 sleep/加 timeout；
- 通过 catch-all/刷新 UI 隐藏竞态；
- 新 session writer 不能读取 P0 corpus；
- 兼容层没有删除阶段；
- 同一 PR 同时改 session wire、Agent state、TUI 展示；
- “为了未来插件”但没有 provider/consumer/conformance；
- 子代理没有 budget/cancel/session lineage；
- 新依赖无 lock/tree/license/MSRV review；
- trace 使用同步逐 chunk flush、跨 await enter guard、无界队列或未分类正文；
- 基准退化但只解释为“架构更干净”；
- 用户未提交修改被覆盖、格式化或回滚。
