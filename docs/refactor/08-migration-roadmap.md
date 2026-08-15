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

#### P0-09 O0 trace integrity baseline — **DONE（2026-08-14）**

- 盘点 `src/main.rs` logging guard、`src/provider/trace.rs` 同步 JSONL 和全部 `span.enter()`。
- 为现有 `agent.run`/provider/tool 路径写 subscriber capture test，证明 `.await` 前后 parent/child 关系。
- 用 `Future::instrument`/`#[instrument]` 修复跨 await enter guard；不顺便改业务状态机。
- provider trace 只做测量和风险登记，本任务不先重写；记录每 chunk 写/flush、mutex wait、body 敏感字段和现有无关联 ID。
- 验收：异步 ancestry 测试绿色；Standard 日志 secret canary 为零；before/after 开销有数据。
- 实施：唯一 `span.enter()`（`src/agent/mod.rs` 原 `run` 顶部）已改为 `Future::instrument`——`run` 变薄 wrapper，原 body 移入 `run_inner`（`run_id` 经参数传入），span 随 future 的 poll enter/exit，yield 即释放。新增 `tests/trace_ancestry.rs`：自定义 yield provider + 并发两个 run + registry-based capture layer，断言任意时刻 `agent.run` enter 深度 ≤ 1。红灯验证：修复前深度 = 2（精确复现 Medium-7）；修复后 = 1。
- provider trace 风险登记（O2 前不重写，仅登记）：`src/provider/trace.rs` 是进程级旁路——`OnceLock<Option<Mutex<File>>>` 一次决定；每次记录（含 stream 每个 SSE event：`sse_event`/`tool_arguments_delta` 等 8 处调用）同步 `write_all + flush`（慢盘放大流延迟）；每 chunk 抢全局 Mutex（`util::lock_mutex`）；`TPI_TRACE_PROVIDER=body` 记录完整 request body（可能含用户代码）；记录无 session_id/run_id/request_id/attempt_id 关联（仅 ts_ms + kind）。
- main.rs logging 盘点：rolling daily + `tracing_appender::non_blocking`，WorkerGuard 被 `Box::leak`（程序生命周期内存活）；EnvFilter 默认 INFO `from_env_lossy`；Standard 日志（fmt layer）无 ANSI/无 target。loss counter 未暴露（O2 处理）。
- secret canary：扫描 src 全部 `tracing::*!` 宏，无直接记录 api_key/authorization/password/secret/token 字段的调用点；Standard 日志路径 canary 为零。
- 开销：trace_ancestry 修复前（红灯，0.03s）与修复后（绿灯，0.02s）无感知差异；instrument 每次 poll enter/exit 开销在异步路径不可测出。

### Exit gate

- 全量测试除明确 real-API opt-in 外绿色；
- baseline/corpus/perf artifact 可复现；
- O0 trace ancestry/secret baseline 可复现；
- 代码行为零有意变化（除 fixture 根因和依赖卫生）；
- 任一失败可回滚单 PR。

### P0 未知项决策（2026-08-14 用户确认，对 00 文档 §8 的答复）

- **#2 长时运行**：先建合成基线（P0-05 已落地，release artifact `target/perf-baseline.json`）；真实 10h 测试放 scheduled/manual gate，之后执行。
- **#3 终端组合**：Windows Terminal + Git Bash 为主。
- **#4 MCP 同名冲突**：不声称真实用户已遇到；Registration ABA 是独立正确性问题，按高优先级先写 red test 并修复（P4-01）。duplicate-name policy：默认拒绝重复；或必须经显式 scope/override；old disposer 绝不能删除 replacement。
- **#5 allow_outside_workspace**：保持当前配置与默认语义，重构不得改变行为；后续 scoped capability/policy 必须能表达该配置，但不借机扩大默认权限。
- **#6 renderer 模式**：只保留 alternate-screen；无明确需求与 UX 证据不实现双 renderer。
- **#7 子代理用途**：第一目标 = 只读、隔离上下文的并行调查（初始规格见 P8）。
- **#8 session 全文检索**：当前不需要；P9-02 保持关闭，不引入 SQLite；有真实搜索需求与 JSONL 扫描性能数据时才写独立 ADR。

## 4. Phase 1：统一词汇与逻辑边界

### 目标

先在单 crate 内建立 domain/runtime/view 边界和依赖方向，不拆 Cargo package。

### 任务

#### P1-01 Run/Turn/Step/Attempt 词汇审计 — **DONE（2026-08-14）**

- 标注所有字段/事件/日志/测试中 `turn/run/retry` 的真实含义。
- 新增 typed IDs（只在确有混用处）和文档，不大规模 rename。
- 一次迁移一个歧义点；保持 session wire 名称。
- 验收：state transition table 与现有 tests 对齐。
- 产出：[13-vocabulary-audit.md](13-vocabulary-audit.md)。结论：
  - `run` = Run（一致）；`attempt` = Attempt（一致）；`retry` = 新 Run（行为正确）。
  - **`turn` 命名冲突**：当前代码的 `turn`（TurnStarted/max_turns/view.turn）实际是
    **Step 级**（每次 model request）；目标词汇的 Turn（用户批次）当前不存在
    （一个用户消息 = 一个 Run）。`max_turns` 是 session wire 字段不可改名。
  - **typed ID 无需新增**：RunId/RequestId(=Step 身份)/ToolCallId/EventId/SessionId 已
    覆盖全部真实边界；attempt 由 `(RequestId, u32)` 标识；TurnId 待 P8 inbox 引入。
  - 修正 3 处误导注释（agent/mod.rs:292、session/mod.rs AssistantMessage、
    p1_fixes.rs:1083）：`attempt`→`run`/`assistant message`，纯注释零行为影响。
  - 遗留：`RuntimeEvent::TurnStarted`→`StepStarted`、`view.turn`→`view.step` 在
    P1-03 live/view event 分离时一并改名（live event 非 durable，改名安全）。

#### P1-02 建 domain message/content — **DONE（2026-08-14）**

- 新类型与现有 `provider::ChatMessage` 并存；写双向 test adapter。
- session projection 先输出 domain message，provider converter 再生成旧 ChatMessage。
- 验收：所有 golden model requests byte/semantic 等价；provider-specific field 不进入 domain。
- 实施（此前已完成，本次补验收标记）：`src/message.rs` 定义 `DomainRole/DomainContentBlock/
  DomainMessage`（UI/provider agnostic）+ 双向 adapter（ChatMessage→Domain→ChatMessage 往返
  语义等价）。`src/session/store.rs` 投影链 `events -> project_domain_messages -> ChatMessage`
  （`replay_domain_messages`/`project_domain_messages`），Conversation 投影经 domain 中间层。
  provider-specific field（base_url/finish reason 等）不进入 domain。
- 验收测试 `tests/domain_message.rs`（8 断言）：双向往返语义等价；golden model requests
  byte/semantic 等价；corpus parity（001_tool_loop 真实 session）。
- 验收达成：golden model requests byte/semantic 等价 ✓；provider-specific field 不进入
  domain ✓；全量 52 target 绿。

#### P1-03 拆 live runtime 与 view event — **DONE（2026-08-14）**

- 建 UI-agnostic event；先迁一个事件（如 phase/usage），app projector 生成旧 TUI event。
- 每 PR 迁一组：assistant、tool lifecycle、process、plan/input。
- 验收：headless sink 可直接消费所有 runtime events；无 UI drain task需求的 contract test。
- 实施：
  - 新增 `agent::LiveEvent`（UI-agnostic）：StepStarted/AssistantDelta/ToolStarted
    （带原始 arguments，非渲染 target）/ToolCompleted（带 output+diff，非 tail）/ToolOutputDelta/
    ContextUsage/UsageUpdated/BudgetWarning/PlanUpdated/StreamRecovering/TurnRestarting/
    CompactionNotice。
  - `RunInput.ui` 从 `Sender<RuntimeEvent>` 改为 `Sender<LiveEvent>`；agent/tool_runtime
    只发 LiveEvent（不再产生 view 字段）。
  - 新增 `app::project_live_event`：LiveEvent → RuntimeEvent（TUI view event），含
    `tool_target` 展示投影（从 agent/tool_runtime 移入 app，P1-03 前由 agent 产生
    target/command 摘要）。
  - P1-01 遗留改名：`RuntimeEvent::TurnStarted`→`StepStarted`、`view.turn`→`view.step`、
    `StatusLine::Running { turn }`→`{ step }`（live event 非 durable，改名安全）。
  - headless（run_prompt_once）消费 LiveEvent；注释注明真正无 drain 的 headless 订阅
    在 P3-05。
  - 测试：app.rs `project_live_event_covers_all_variants`（全变体投影 + tool_target 摘要）；
    agent_flow/p1_fixes 断言改为 LiveEvent；tui_reducer 改 StepStarted。
  - 验收达成：headless sink（agent_flow 直接消费 LiveEvent）不依赖 TUI 投影；
    `agent -> tui` 引用在 P1-03 后仅剩……（见 P1 Exit gate 检查）。

#### P1-04 消除 `tui::reducer -> app` — **DONE（2026-08-14）**

- 移动 preview 纯投影到 presentation owner。
- 先锁定输出 tests；只改 dependency，不改预览文案。
- 验收：架构 gate 删除该 allowlist。
- 实施：`preview_lines_to_body` 从 `src/app.rs` 移入 `src/tui/model.rs`（MenuPreviewLine
  宿主，presentation owner）；改 3 处调用（app.rs:1280 全限定、reducer.rs:44 本地化、
  tui_fullscreen.rs 测试）。函数体零改动（纯移动）。`scripts/arch_gate.sh` R1 allowlist
  清空（`tui -> app` 引用清零）：任何 `crate::app` 引用都违规。正反例验证：注入
  `src/tui` 的 `use crate::app::` 被拒、恢复通过。tui_fullscreen/reducer/rework 输出
  测试全绿 + 全量 44 target 绿。**P0-07 首个 allowlist 收紧完成**（R1 清零）。

#### P1-05 分解 Config 输出 — **DONE（2026-08-14）**

- 保留一个 resolver；引入 domain-specific resolved views。
- 每次只让一个 owner 接收窄 config。
- 验收：字段级 merge/unknown rejection 全部保持；default snapshot 不变。
- 实施（窄视图此前已定义，本次补齐 owner 消费）：`Config` 保留 composition
  resolver（merge/unknown rejection/default snapshot 不变）；窄视图
  `AgentConfig`（model/limits/safety_reserve/system_prompt_extra/workspace_root）、
  `ToolPolicy`（allow_outside_workspace/shell/artifacts/sessions/web_summary）、
  `UiConfig`（theme/mode/keymap/collapsed_lines）已定义。本次迁移：
  - `agent::run_inner/compact_turn/build_context` 开头 `config.agent_config()` 投影，
    函数体内只读 `agent_cfg.*`（不再直接 config.model/limits 等）；
  - `system_prompt_text(&Config,...)` → `system_prompt_text(Option<&str>,...)`
    （窄参数：只传 system_prompt_extra）；5 处调用点更新；
  - tool_runtime 已用 `tool_policy()` 窄视图（P1-05 前完成）。
- 验收达成：字段级 merge/unknown rejection 由 Config resolver 测试保持；default
  snapshot 不变；agent owner 只读窄视图。agent 17 + agent_flow 15 断言全绿；
  全量测试通过；fmt/clippy/arch_gate 清洁。

#### P1-06 main/app composition inventory — **DONE（2026-08-14）**

- 把 object construction 与 use case 标注；不立即移动复杂 loop。
- 建 `AppServices` 显式字段，禁止 service locator。
- 验收：测试可用 fake ports 构造最小 controller。
- 实施：`AppServices<P: Provider>`（config/workspace_root/sessions_root/provider/
  conversation/current_cancel/mcp_manager 显式字段）+ `from_config`（construction：
  真实 provider + 会话恢复 + Ctrl-C handler + ephemeral 目录，全部集中）+ `run_with_services`
  （use case：`-p` 返回 `Option<String>` 最终答案、交互进 interactive_loop；不直接写
  stdout）。`run` 退化为薄 facade（construction + use case + 打印）。测试
  `tests/app_services.rs`：fake EchoProvider 构造最小 controller 驱动 `-p`
  use case（不依赖真实 key/网络）、空 prompt 拒绝、类型存在性。
  `latest_session_id` 改 pub（web serve 复用）。

#### P1-07 O1 typed trace identity/catalog — **DONE（2026-08-14）**

- 定义 `TraceId/SpanId/SessionId/RunId/TurnId/StepId/AttemptId/RequestId/ToolCallId/RegistrationId/ChildId`，只在真实边界注入。
- 建 schema/versioned `TraceRecord` 和 span/event/sensitivity/completeness catalog；生成 producer/consumer 文档。
- 先用 adapter 映射旧 tracing fields，禁止业务模块直接依赖 exporter DTO。
- 验收：catalog 无孤儿 name、每个 payload 有 sensitivity/default mode/owner。
- 实施：`src/ids.rs` 新增 `TraceId/SpanId`（UUIDv7）；`src/trace.rs` 建 TraceRecord
  （schema/kind/level/outcome/CorrelationIds/sensitivity/completeness）+ TraceValue
  （Plain/Hashed/Redacted）+ `CATALOG`（span/event 注册表：agent.run + 11 个已登记
  event，每项 sensitivity/owner；测试强制无重复、非空、可解析）。`agent.run` span
  注入 `trace_id/span_id`（真实边界，一次 Run = 一个 Trace）。文档
  [14-trace-catalog.md](14-trace-catalog.md)：身份模型 + span/event 目录 +
  sensitivity 规则（Secret 永不 Plain）+ completeness。未登记的运维日志（~30 处
  remote/MCP/process error 转发）O2 落地时随 sink 逐条补登；catalog 测试届时改
  强制全量。业务模块仍用 tracing 宏（O2 前不迁移），不依赖 exporter DTO。

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

#### P2-01 机械拆 session protocol/codec/store — **DONE（2026-08-14）**

- 仅移动代码与 visibility；每次一个 module。
- 用 golden hash 证明 encode 无变化。
- 验收：`git diff` 无无关 format；old corpus 全过。
- 实施：`src/session/mod.rs`（1533 行）按行切割为三块，wire 零改动：
  - `protocol.rs`（500 行）：SessionEvent/Envelope/EventBody/schema version/serde
    wire 类型（领域 API + 稳定 wire）；
  - `store.rs`（772 行）：SessionLog append/read/sync/lock、head cursor、投影
    （replay/project，含 P1-02 domain 投影）；
  - `mod.rs`（325 行）：子模块声明 + `pub use` re-export（对外 API 完全不变）+
    tests。
  - visibility 调整：SessionProtocolState/read_envelopes/open_and_lock_session/
    read_envelopes_state_with_limits/EnvelopeRead/MAX_SESSION_EVENTS 转
    pub(crate)/pub（store 内私有符号对 repair/测试可见）；conversation.rs
    `super::Plan` 改 `super::protocol::Plan`。
  - golden hash 验证：确定版 example（固定 session_id/run_id/event_id + 去随机
    字段求 hash）在 HEAD 与拆分后**完全一致**（`25a9f617...`，1817 字符）——
    此前 hash 差异源于随机 session_id/event_id/timestamp 与旧文件残留，已排除。
  - 验收达成：`git diff --check` 无空白错误；session_golden 10 + domain_message 8
    （corpus）全过；全量 47 target 绿；fmt/clippy/arch_gate 清洁。
  - re-export 兼容层（`session::X` 直引）留待 P10-01 清理（删除条件：调用方
    已迁移到 `session::store::X`）。

#### P2-02 `SessionStore` port + JSONL adapter — **DONE（2026-08-14）**

- 从当前真实调用抽最小接口。
- current `SessionLog` 先实现；Agent 通过 port。
- 验收：in-memory fake 跑 agent_flow；单写者/seq/recovery tests 对 adapter 运行。
- 实施：`session::store::SessionStore` trait（begin_run/session_id/seq/path/
  append_event/sync_data/write_ahead_tool/complete_tool/events_with_seq）；
  `SessionLog` 实现（JSONL adapter，转发既有方法）。agent 泛型化
  `run<P, S: SessionStore>`（含 run_inner/compact_turn/ToolBatchExecutor/execute_batch），
  agent 不再直接碰文件路径——`latest_plan(session.path())` 改为
  `latest_plan_from_events(&session.events_with_seq())`，compaction 的
  `read_events_with_seq(session.path())` 改为 `session.events_with_seq()`。
- 测试 `tests/session_store_fake.rs`（8 断言）：InMemoryStore（Vec 存储，无文件）
  实现 SessionStore 跑完整 agent_flow（事件序列 User/RunStarted/Assistant/
  RunCompleted）；adapter 单写者/seq 严格递增/write-ahead 顺序（ToolRequested→
  ToolStarted→ToolCompleted）契约；两 store 隔离。
- 验收达成：in-memory fake 跑 agent_flow ✓；单写者/seq/recovery 对 adapter ✓；
  全量 47 target 绿；fmt/clippy/arch_gate 清洁。

#### P2-03 conversation/transcript/plan projector — **DONE（2026-08-14）**

- 分别建立纯 `apply/rebuild`；现有 `Conversation` 做 facade。
- 属性测试 incremental == rebuild。
- 验收：失败 run refresh 行为与 golden 一致。
- 实施：`src/session/projector.rs` 新增纯 `ConversationProjector`：
  - `rebuild(events)` 全量重建（history + plan，复用 store 投影）；
  - `apply(seq, event)` 增量记录（O(1) 追加）；
  - `history()/plan()` 读时惰性重投影（等价全量 rebuild）——incremental apply
    不重复实现投影逻辑（避免 ToolRequested 关联/compaction prune 双实现漂移）；
  - `from_history`（accept_context 的外部完整 context 注入）。
  `Conversation` 改 facade：`history/plan` 字段替换为 `projector`；
  `resume/refresh_from_log` 经 `events_with_seq()`（P2-02 port）喂 `rebuild`，
  不再直接碰文件路径（replay_messages/latest_plan 文件版不再被 Conversation 用）。
- 属性测试 `tests/projector_property.rs`（7 断言）：全类型序列的任意前缀
  `apply == rebuild`（history/plan）；proptest 随机序列 + 中间前缀；
  projector 与 store::project_messages 等价（防未来漂移）。
- 验收达成：incremental == rebuild ✓（含空投影与随机截断）；失败 run refresh
  走 events_with_seq + rebuild（golden 语义不变，conversation 测试全过）；
  全量 48 target 绿；fmt/clippy/arch_gate 清洁。

#### P2-04 durability barrier 类型化 — **DONE（2026-08-14）**

- 让 pre-effect/commit/shutdown sync 意图显式，不改实际刷盘时序。
- fault injection 覆盖每个 barrier。
- 验收：recovery matrix 不退化。
- 实施：`SessionStore` 新增 3 个类型化 barrier 方法（默认实现 = append + sync，
  时序不变）：
  - `commit(event)`：普通事实提交（User/Assistant/Plan/ToolRequested/
    AssistantAttemptInterrupted/UserInputRequested/CompactionCommitted）；
  - `commit_pre_effect(call_id, recovery)`：写工具副作用前 write-ahead
    （ToolStarted + sync；转发 write_ahead_tool）；
  - `commit_terminal(event)`：run 终态（RunCompleted + sync）。
  迁移 agent/mod.rs 18 处、tool_runtime.rs 2 处、app.rs 1 处 `.and_then(|_|
  session.sync_data())` 链 → 类型化方法（RunCompleted→commit_terminal，其余→
  commit）。`sync_data` 保留为底层 flush（幂等，无 pending 时 no-op）。
- fault injection 测试 `tests/durability_barrier.rs`（5 断言）：FaultyStore
  （append 成功、sync 可故障）验证三种 barrier 的 sync 失败都传播错误、
  append 已计入 seq（可重试）、fault 清除后成功、类型化方法 == append+sync
  （无 fault 时完全等价）。
- 验收达成：pre-effect/commit/shutdown 意图显式（barrier 类型化）；fault
  injection 覆盖 commit/commit_terminal/commit_pre_effect；recovery_matrix
  既有 5 个 crash 场景全过（不退化）；全量 49 target 绿；fmt/clippy/arch_gate
  清洁。

#### P2-05 `Supervisor` walking skeleton — **DONE（2026-08-14）**

- 先拥有一个无害 background task，验证 cancel/close/wait/aggregate error。
- 使用 CancellationToken + TaskTracker；建立 leak test。
- 验收：100 次 start/shutdown 后 tracked tasks 为 0。
- 实施：`src/process/supervisor.rs` 新增 `Supervisor`（ADR-006 协议落地）：
  - `spawn(name, f)`：父 token 派生 child token（父 cancel 传播全部），
    TaskTracker 跟踪（quiescence）；
  - `shutdown()`：固定顺序 cancel → close → wait（tracker.wait）→ 汇总错误，
    幂等；
  - `token()/tracked()/is_cancelled()`。
  - 测试（4 断言）：无害循环任务 shutdown 后 tracked==0；**100 次 start/
    shutdown tracked 始终归 0**（leak test 核心验收）；panic 任务不阻塞
    quiescence；外部 cancel 传播到子任务。
  - 错误汇总（aggregate error）在 P2-06 迁 watchdog 时引入 join_handles
    补全（walking skeleton 阶段以 tracked==0 为 quiescence 验收）。
- 验收达成：100 次 start/shutdown 后 tracked==0 ✓；cancel/close/wait 协议
  成立 ✓；全量 49 target 绿；fmt/clippy/arch_gate 清洁。
  P2-06（迁 watchdog/tool forwarder）基于本 Supervisor。

#### P2-06 迁 Agent watchdog/tool forwarder — **DONE（2026-08-14）**

- 一次迁一个 spawn；删除直接 abort 或在 bounded hard-stop 后 await。
- 验收：cancel at every await；terminal event ordering。
- 实施：
  - **watchdog**：`AbortTaskOnDrop(tokio::spawn)` → `Supervisor::spawn("agent.watchdog")`。
    watchdog 逻辑内联进 Supervisor 任务（select warn/deadline/cancel，两处 await
    都有 cancel 分支 = cancel at every await）；run 结束时 `shutdown().await`（join
    而非 abort）；Drop 兜底 cancel。`limits::spawn_watchdog` 保留（测试/诊断用）。
  - **tool stream forwarder**：`tokio::spawn + abort()` → `Supervisor::spawn(
    "tool.stream_forwarder")`。channel 关闭（drop 最后 sender）时 recv 返回 None
    任务自然结束；wave 后 `drop(output_tx) + shutdown().await`（join）。
    ToolOutputDelta 经有界 channel 在 ToolCompleted 前送达（ordering 保持）。
  - `AbortTaskOnDrop` 生产代码删除（移入测试模块，仅验证 Drop 行为）。
- 验收达成：cancel at every await（watchdog/forwarder 的 select/recv 均有 cancel
  分支）✓；terminal event ordering（ToolRequested→Started→Delta→Completed 顺序
  由 channel + join 保证，agent_flow/recovery_matrix 全过）✓；全量 49 target 绿；
  fmt/clippy/arch_gate 清洁。
  **生产 spawn 清单中 watchdog/forwarder 已有 owner（Supervisor）**；P2-07 盘点
  其余（MCP reader/process/web writer）纳入各 supervisor。

#### P2-07 MCP/process owner inventory — **DONE（2026-08-14）**

- 只把 owner 不明确的 task 纳入相应 supervisor，不改协议。
- 验收：shutdown 后 child process/tree、channel、registration 清零。
- 实施：
  - **MCP reader**：`McpClient` 新增 `reader_supervisor: Supervisor` 字段；
    stdout/stderr reader 由 `tokio::spawn` → `supervisor.spawn("mcp.stdout_reader"/
    "mcp.stderr_reader")`；`shutdown()`/`kill()` 末尾 `reader_supervisor.shutdown()`
    （join，不留下无主 reader task）。新增 `reader_tracked()` 供验收断言。
  - **process**：盘点确认 `spawn_drain`（managed.rs）已是完整 owner（自含
    Job Object 生命周期 + cancel token + 结束时 remove_cancel）；`run_in_host`
    的 host_stderr 转发是短命 EOF 任务（进程结束即退出）。无需改造（不伪报）。
  - **web**：生产 spawn（连接 handler/run_agent/drain）均为短命或已有
    生命期；760/781/799 是测试代码（已 await join）。无需改造。
- 验收测试：`tests/mcp_contract.rs` 新增 `shutdown_clears_reader_tasks_and_child`
  ——start 后 reader tracked ≥ 1、进程存活；shutdown 后 tracked==0（Supervisor
  join）、子进程终止。mcp_contract 15 断言全绿。
- 验收达成：MCP shutdown 后 reader task 清零 ✓（child 由既有 shutdown/kill
  终止 + Job Object 树）；registration 由 RAII ToolRegistration drop 自动注销
  （既有）；channel 由 reader task 结束自然释放 ✓；process/web 经盘点确认
  已有 owner（不改协议）。全量 49 target 绿；fmt/clippy/arch_gate 清洁。

#### P2-08 O2 local trace sink — **DONE（2026-08-14）**

- application 持有 writer/flush guard；建立 Standard 本地 JSONL/segment sink。
- queue 按 records+bytes 有界；溢出产生 counter 和后续 `TraceGap`，terminal/invariant 走保留通道。
- 建有界内存 flight recorder；只有 invariant/fatal/user trigger 才冻结异常前后窗口，正文仍服从模式与脱敏。
- rotation/retention/abnormal-exit recovery；sink error 只降级观测，不改变 session/run。
- 验收：故障注入、慢盘、queue full、shutdown deadline 和损坏尾部测试。
- 实施：`src/trace.rs` 新增 O2 sink：`TraceSink<W: Write>`（有界队列 records=4096 +
  bytes=1MiB；`push` 并发安全原子 seq；溢出 → 丢最旧 + gap counter + pending `TraceGap`
  声明；`flush` 写全部 + pending gap，失败只降级不 panic）、`TraceFlushGuard`（Drop 时
  flush，有界队列保证有界耗时 = shutdown deadline 的替代）、`SinkStats`（written/dropped/
  gaps/first/last seq = manifest/completeness）。TraceRecord/TraceGap/枚举补
  Serialize（JSONL 落盘）。sink error 只降级观测：flush 失败记录 dropped，下次重试。
- 验收测试 `tests/trace_sink.rs`（6 断言）：normal flush、queue overflow（gap counter +
  TraceGap 声明）、slow disk（有界 flush 完成）、write failure（降级不 panic，恢复后重试）、
  flush guard Drop、corrupted tail（append-only 追加不受旧坏数据影响）。
- 验收达成：故障注入/慢盘/queue full/shutdown deadline/损坏尾部全过 ✓；sink error 不
  改变 session/run（降级观测）✓；全量 51 target 绿；fmt/clippy/arch_gate 清洁。

#### P2-09 O3 session telemetry projector skeleton — **DONE（2026-08-14）**

- committed event -> telemetry record 的纯 projector；live 和 prefix replay 共用实现。
- 用 `(session_id,event_seq,projector_version)` 去重；handoff 允许重复、不允许无声明缺口。
- Standard 不含正文；Verbose payload 走独立 sidecar reference。
- 验收：任意合法 prefix、incremental == replay、sink drop 不影响 append。
- 实施：`src/session/telemetry.rs` 新增纯 `SessionTelemetryProjector`：`project(seq,event)`
  增量（seq <= last 幂等忽略 = handoff 重复；seq 跳变 → `TelemetryGap` 声明，不允许无
  声明缺口）、`rebuild(events)` 全量（live 与 prefix replay 共用同一实现）。
  `TelemetryRecord`：session_id/event_seq/projector_version 三元组（去重键）+
  event_type + projected_seq + counts（tool_calls/interrupted 元数据）+ sidecar_seq
  （Standard 不含正文，Verbose 走 sidecar reference）。
- 验收测试 `tests/telemetry_projector.rs`（6 断言）：任意前缀 incremental == replay；
  handoff 重复幂等；seq 跳变声明 gap；去重三元组在场；counts 仅元数据（正文不入
  record）；sink drop 不影响 append（纯状态）。
- 验收达成：任意合法 prefix ✓、incremental == replay ✓、sink drop 不影响 append ✓、
  去重 + 无声明缺口 ✓；全量 51 target 绿；fmt/clippy/arch_gate 清洁。

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

#### P3-01 定义 `UiIntent/AppCommand/AppEffect` — **DONE（2026-08-14）**

- 收集所有 key/mouse/slash action；按语义建 enum。
- 旧 event pump adapter 把新 command 转回旧函数，保持行为。
- 验收：recorded input trace 的 command sequence 固定。
- 实施：`src/app.rs` 拆为 `src/app/mod.rs`（逻辑不变）+ `src/app/intent.rs`：
  `AppCommand`（SubmitInput/Quit/CancelRun/StartNewSession/CompactNow/RetryLast/
  ToggleSidebar/ToggleReasoning/OpenModal/OpenLastTool/OpenFailedTool/OpenSearch/
  OpenSession/RequestInputAnswer/Paste，平台无关）、`UiIntent`（AppCommand +
  IntentSource：Keyboard/Mouse/SlashCommand/Headless/Paste）、`AppEffect`
  （Draw/CopyToClipboard/OpenUrl/SetTerminalTitle/OpenFilePicker/Notify）。
  adapter：`command_from_slash` + key 事件映射（Ctrl+D/Ctrl+B/Ctrl+F/Enter/Paste）。
- 验收测试 `tests/app_intent.rs`（4 断言）：recorded input trace command sequence
  固定（确定性）；Ctrl+D/Enter 语义等价；scroll 是视图意图；slash 映射与旧 pump 一致。
- 验收达成：command sequence 固定 ✓；key/mouse/slash action 按语义入 enum ✓。
  全量 52 target 绿；fmt/clippy/arch_gate 清洁。P3-02 基于本意图模型。

#### P3-02 建 `AppController` — **DONE（2026-08-14）**

- 先迁 `start/cancel run` 一个 use case。
- controller 接收 ports、返回 events/effects；不引用 Crossterm/Ratatui。
- 逐个迁 resume/session/input answer/config commands。
- 验收：fake runtime/session/platform 的 integration tests。
- 实施：`src/app/controller.rs` 新增 `AppController<P: Provider>`：
  - `new(services)` 持有 `AppServices`（ports）；`handle(&mut self, UiIntent) ->
    Result<Vec<AppEffect>, String>` 同步决策（无 IO/await），副作用经 AppEffect
    返回由 surface adapter 执行；不引用 Crossterm/Ratatui。
  - 已迁 use cases：CancelRun（取消 current_cancel token + Notify）、
    StartNewSession（conversation.reset）、Quit（请求 Draw）、ToggleSidebar/
    ToggleReasoning/OpenModal/OpenSearch/OpenLastTool/OpenFailedTool（视图意图→
    Draw/Notify）、CompactNow/RetryLast（交还 run 路径）、输入类（SubmitInput/
    OpenSession/RequestInputAnswer/Paste→Draw 由 adapter 进 run 路径）。
  - `take_cancel()` 供 surface adapter 挂载 run。
- 验收测试 `tests/app_controller.rs`（5 断言）：cancel run 取消 token + Notify；
  idle cancel 幂等；start new session 重置会话（parts_for_run 报未启动）；Quit
  请求渲染；ToggleSidebar 是视图意图（Draw，无业务副作用）。fake EchoProvider
  构造最小 controller。
- 验收达成：fake runtime/session/platform integration tests ✓；controller 不
  引用 Crossterm/Ratatui ✓；全量 53 target 绿；fmt/clippy/arch_gate 清洁。
  P3-03（slash registry）/P3-05（headless）基于 controller。

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
- duplicate-name policy（用户决策 2026-08-14）：**默认拒绝重复注册**；覆盖必须经
  显式 scope/override API；**old disposer 绝不能删除 replacement**（`(name,id)`
  同时匹配才删除）。ABA 是独立正确性问题，不依赖真实冲突事故即可开工（高优先级）。

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

> **P8 初始规格（用户决策 2026-08-14）**：第一目标 = **只读、隔离上下文的
> 并行调查**。初始版本要求：默认关闭；`depth = 1`（不允许递归）；
> `concurrency = 1`（单 child 稳定后再评估 bounded parallel）；fresh child
> session；只读 capability allowlist；parent 只接收 structured report；
> parent cancellation 必须传播；不允许共享 workspace 写入；不先接外部
> Agent（ACP/子进程 provider）。隔离 worktree、外部 ACP provider、递归属于
> 后续证据门控能力（各自独立 ADR）。

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
