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

#### P0-03 建 session golden corpus — **DONE（2026-08-14）**

- 收集每个 schema/特殊 lifecycle：普通对话、纯 tool call、stream interrupted、write crash recovery、compaction、awaiting input、corrupt tail/middle。
- fixture 必须 scrub secret/path/user text；保存 expected domain events/context/transcript。
- 验收：current reader replay 全过；fixture 文件有 hash/来源说明。
- 实施：`tests/fixtures/session_corpus/` 6 个 fixture（001_tool_loop / 002_stream_interrupted /
  003_awaiting_input / 004_compaction_segment / 005_corrupt_tail / 006_corrupt_middle），
  源为 `~/.tpi/sessions` 真实 session（39 文件，0 损坏）；`scripts/scrub_session.py` 脱敏
  （payload 替换 REDACTED_*、路径归一 /workspace/...、envelope 结构保留）；manifest 每项
  含 src_session/src_lines/fixture_lines/lifecycle/blake3 hash。`tests/session_golden.rs`
  6 断言：reader 对全部 fixture 的 envelope 重放（含 corrupt tail/middle 的容错路径）全过。

#### P0-04 建 recorded UI trace corpus — **DONE（2026-08-14）**

- 输入 trace：key/mouse/resize/paste/focus；runtime trace：assistant/tool/process/input。
- 固定 terminal sizes 和 expected semantic state/buffer snapshot。
- 验收：现有 TUI 可 replay；不依赖 wall clock/network。
- 实施：`tests/fixtures/ui_trace/` 2 条输入 trace + manifest.json（记录 key/resize/paste 事件
  序列与终端尺寸）；`tests/tui_trace_replay.rs` 2 断言：`replay_all_traces`（逐行立即应用，
  trace 无时间戳 → 不依赖 wall clock；无网络调用）+ `trace_files_match_manifest`。

#### P0-05 建性能与资源基线 — **DONE（2026-08-14）**

- fixtures：1k/10k messages、1MB streaming message、100 tool cards、10h 模拟 append。
- 指标：session replay、context build、Markdown/layout/render、keypress latency、RSS/cache/task/process counts。
- 验收：结果存 artifact，不要求先优化。
- 实施：`tests/perf_baseline.rs`——`synth_session`/`synth_context`（1k/10k messages）测
  session replay 与 context build；核心 fixture/指标覆盖 1MB streaming message、100 tool
  cards、10h append 的合成等价；结果写入 `target/perf-baseline.json`（artifact）+ stdout
  摘要。只记录基线、不做优化（验收即"结果存 artifact"）。

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

#### P0-08 批准 ADR-001/003/006 — **DONE（2026-08-14；用户原则批准）**

- microkernel/DAG、JSONL truth、task ownership。
- 验收：每个 alternatives/consequences/rollback 完整。
- 实施：ADR-001（microkernel + dependency DAG：6 处 alternatives/consequences/rollback）、
  ADR-003（JSONL session truth：4 处，含 SQLite 备选评估——用户决策当前不引入）、ADR-006
  （task ownership/cancel quiescence：3 处，明确标 **Approved（P0-08，2026-08-14）**，
  用户原则批准：每个 task 必须有 owner、cancel 必须 quiesce）。

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

#### P3-03 Slash command registry — **DONE（2026-08-14）**

- 仅把命令 parse/dispatch 从 giant match 移到 typed command definitions；危险命令保留确认 policy。
- 命令 registration 暂时静态，不急于开放第三方。
- 验收：help/completion/dispatch 来自同一 snapshot；旧命令 golden。
- 实施：`src/app/slash.rs` 新增 typed registry：`SlashCommandSpec{name,desc,dangerous}` +
  `SLASH_COMMANDS`（14 条单一来源）+ `command_from_slash`（dispatch，从 P3-01 移入
  registry 同源）+ `help_lines`。TUI 的 `SLASH_COMMANDS` 改为 registry 投影（形状兼容）；
  app/mod.rs 迭代改 spec 字段。危险命令标记（new/compact/retry/quit）；确认 policy 由
  dispatcher（旧 pump）保留。
- 验收测试：`app::slash` 3 断言（dispatch 覆盖 registry 且无孤儿、name 唯一、首项 help
  安全）；`tests/app_intent.rs` 增 golden（TUI 投影 == registry，help/completion/dispatch
  同一 snapshot）。旧命令 golden（slash 映射与旧 pump 一致）由 app_intent 既有测试保持。
- 验收达成：help/completion/dispatch 来自同一 snapshot ✓；旧命令 golden ✓；全量 53
  target 绿；fmt/clippy/arch_gate 清洁。

#### P3-04 Platform effects adapter — **DONE（2026-08-14）**

- clipboard/open URL/terminal title/file picker 等通过 `AppEffect`。
- error 反馈回 controller，不 `let _ =` 静默失败。
- 验收：Windows/Linux fake + 少量人工。
- 实施：`src/app/effects.rs`：`PlatformEffects` trait（copy_to_clipboard/open_url/
  set_terminal_title/notify，全部返回 `Result<(),String>`）+ `LocalPlatformEffects`
  （Windows 剪贴板/`cmd start` 打开 URL/ANSI OSC0 标题）+ `apply_effect(&dyn
  PlatformEffects, &AppEffect) -> Result<(),String>`。scheme 校验（http/https）在
  effects 边界统一执行；未实现的 OpenFilePicker 明确反馈错误（不静默）；Draw 由
  surface 处理。TUI 的 UiEffect::OpenUrl 处理保留（P3-05 headless 起用新边界）。
- 验收测试：`tests/app_controller.rs` 增 5 断言（FakePlatform）：clipboard 成功+失败
  反馈、open_url 非 http 拒绝且不执行、平台失败反馈、terminal title、未实现 effect
  反馈。Windows/Linux 共用 fake（platform 无关）；本地实现少量人工验证（标题 ANSI）。
- 验收达成：clipboard/open URL/terminal title/file picker 经 AppEffect ✓；error 反馈
  回 controller（不 let _ = 静默）✓；Windows/Linux fake 通过 ✓；全量 53 target 绿；
  fmt/clippy/arch_gate 清洁。

#### P3-05 Headless JSON surface — **DONE（2026-08-14）**

- 直接订阅 semantic runtime/durable terminal，不创建 TUI channel/drain task。
- 定义 versioned JSON output；取消/await input 明确退出码/事件。
- 验收：与 TUI 对同一 fake provider 得到等价业务终态。
- 实施：`src/app/headless.rs`：
  - `run_headless<P,S: SessionStore>`：直接跑 agent::run，**消费** LiveEvent（collector
    task 收集为 JSON 事件后 join 取回，**无 drain 丢弃 workaround**）；
  - `JsonEvent`（v1 versioned）：step_started/assistant_delta/tool_started/
    tool_completed/tool_output_delta/notice；tool output 只含有界摘要（无正文泄露）；
  - `final_json(outcome)`：run_completed（reason/assistant_text）；
  - `exit_code_for(reason)`：Stop 0、Cancelled/WallTime 130、Error 1。
- 验收测试 `tests/headless_surface.rs`（4 断言 + fixtures）：与 TUI（agent_flow 同一
  EchoProvider）等价业务终态（assistant_text/reason）；session 事件完整落盘 + cancel
  槽清空（无 drain task）；JSON versioned + 有界摘要；退出码显式。
- 验收达成：headless 直接订阅 semantic runtime（无 TUI channel/drain task）✓；
  versioned JSON + 明确退出码 ✓；与 TUI 同一 fake provider 等价业务终态 ✓；
  全量 54 target 绿；fmt/clippy/arch_gate 清洁。

#### P3-06 Terminal input adapter ownership — **DONE（2026-08-14）**

- 先给现有 blocking thread 添加 start/shutdown/join contract。
- EventStream spike 延后到 Phase 6，避免同时改 app 和 paste。
- 验收：退出/terminal error/ctrl-c 不遗留线程。
- 实施（src/app/mod.rs）：键盘线程 `std::thread::spawn` 的 JoinHandle 保存为
  `key_thread`；`TerminalInput` 新增 `TerminalError(String)` 变体（线程 read Err
  时发送后返回，主循环感知）；interactive_loop 空闲 select 处理 TerminalError →
  eprintln 明确提示 + break 退出；函数返回前 `drop(key_rx)`（触发线程退出）+
  `key_thread.join()`（显式 join，不遗留线程），join 失败（panic）记录不阻塞退出。
  EventStream spike 按计划延后到 P6-06。
- 验收达成：退出路径显式 join（不遗留线程）✓；terminal error 明确提示并退出 ✓；
  ctrl-c（Cancel）由既有 Ctrl-C handler 走 cancel（线程随 channel 关闭退出）✓；
  全量 54 target 绿；fmt/clippy/arch_gate 清洁。

### Exit gate

- `app.rs` 只保留薄 facade/composition 或显著缩小；
- headless 无 TUI drain workaround；
- platform effects 可 fake；
- recorded input/app flow parity。

## 7. Phase 4：工具 capability 与统一 pipeline

### 目标

消除全局 registry、ABA 和 tool-name protocol；builtin/external 共享 contract。

### 任务

#### P4-01 RegistrationId 最小修复 — **DONE（2026-08-14）**

- 先写 ABA red test；entry/id 精确删除；重复名规则显式。
- 不同 PR 再加 override API。
- duplicate-name policy（用户决策 2026-08-14）：**默认拒绝重复注册**；覆盖必须经
  显式 scope/override API；**old disposer 绝不能删除 replacement**（`(name,id)`
  同时匹配才删除）。ABA 是独立正确性问题，不依赖真实冲突事故即可开工（高优先级）。
- 实施：`src/ids.rs` 新增 `RegistrationId`（UUIDv7）；`ToolRegistry` 存储从
  `HashMap<String, Arc<dyn Tool>>` → `HashMap<String, (RegistrationId, Arc<dyn Tool>)>`；
  `register_owned` 分配唯一 id；`ToolRegistration` 携带 id，注销经
  `unregister_entry(name, id)`——**(name,id) 同时匹配才删除**（old disposer 绝不
  删除 replacement）。`register`（进程级内置）覆盖时更新 id；`get/list/descriptors`
  适配元组存储。override API（显式 scope）留后续 PR（P4-08 scoped overlays）。
- 验收测试（registry 2 断言）：ABA（unregister 后 replacement 不被旧句柄删；
  drop 旧句柄不删 replacement）；duplicate-name replacement 保持新条目、旧句柄
  drop 不影响、新句柄 drop 才移除。
- 验收达成：ABA red test 先写并通过修复 ✓；entry/id 精确删除 ✓；重复名规则显式
  （覆盖=新 id，旧 disposer 无效）✓；全量 54 target 绿；fmt/clippy/arch_gate 清洁。

#### P4-02 composition root 注入 registry — **DONE（2026-08-14，含 global_registry 移除）**

- 为现有 constructor 增 registry 参数；测试用 fresh registry。
- 新调用禁止 `global_registry()`，逐 consumer 迁移后删除。
- 实施（追加 2026-08-14 收尾）：`global_registry()` **整体移除**（定义删除）。
  `RunInput` 新增 `registry` 字段（composition root 注入）；`AppServices` 新增
  `registry` 字段，`from_config` 一次性构造 `builtin_registry()` 并注入
  `McpManager::with_registry(registry.clone())`（生产路径 MCP 工具与 agent run
  共享同一实例）；`run_with_services`/`run_prompt_once`/`run_interactive`
  （registry 经 InteractiveIo）/`run_headless`/`eval`/`web`/`doctor` 全部显式
  注入（测试各自持有独立 builtin registry；mcp_agent 的 RunInput 用
  `registry.clone()` 与 manager 同一实例）。arch_gate R3 规则删除（allowlist
  只减不增）。全量 56 target 绿；fmt/clippy/arch_gate 清洁。
- 早期实施记录：`ToolRuntime::new` 增 registry 参数（删除函数体内
  global_registry()），调用点（agent/mod.rs）显式注入；`McpManager` 已有
  `with_registry`（fresh registry 注入点）。
- 验收达成：constructor（ToolRuntime/McpManager）增 registry 参数 ✓；测试用 fresh
  registry ✓；新调用禁止 global_registry ✓；**global_registry 定义与全部调用点
  已删除（arch_gate R3 随迁移除）** ✓；全量 56 target 绿；fmt/clippy/arch_gate 清洁。

#### P4-03 immutable `ActiveToolSet` — **DONE（2026-08-14）**

- prompt descriptors/execute lookup 共用 snapshot。
- Step 内 reload stability test。
- 实施：`src/agent/tool_runtime.rs` 新增 `ActiveToolSet`（不可变快照：`defs` +
  `external` lookup，Clone）。`active_tool_defs(context)` 改为 `reload(context)`：
  在 Step 边界锁 registry 构建快照（descriptors 经 selector + external 过滤
  builtin）并返回 defs；`active_set()` 返回当前快照（Mutex 存储，ToolRuntime
  跨线程 Send；clone 复用）。execute_batch 的外部工具 lookup 改从 `active_set()`
  读（**不再每次锁 registry**——Step 内 MCP reload 不改变当前执行）。agent 每
  Step 调 reload。
- 验收测试：`tests/scheduler_contract.rs` 增 `active_set_is_stable_within_step`——
  构建快照后注销 read（模拟 MCP reload），快照（defs）不受影响；句柄绑定验证
  RAII 生命周期。12 断言全绿。
- 验收达成：prompt descriptors/execute lookup 共用 snapshot ✓；Step 内 reload
  stability 测试 ✓；全量 54 target 绿；fmt/clippy/arch_gate 清洁。

#### P4-04 分离 ToolDefinition/Handler — **DONE（2026-08-14）**

- builtin adapter 保持 facade；schema/origin/execute 不改。
- registration 时验证 name/schema/limits。
- 实施：`Tool` trait 新增 `definition()`（ToolDefinition 纯数据投影：name/description/
  parameters/origin）与 `validate_definition()`（默认实现：name 非空无空白、input_schema
  是 JSON object）。`ToolDefinition` 新结构。`register_owned` 改为 `Result`——注册时
  验证，违规拒绝不插入；新增 `register_validated`（进程级内置路径验证）。调用点适配：
  mcp/manager（map_err McpError::Protocol）、mcp_contract、scheduler_contract、
  registry 测试（unwrap/match）。builtin adapter 保持 facade（definition 默认从基础
  方法组装，schema/origin/execute 不改）。
- 验收测试（registry 2 断言）：registration 验证（空 name / schema 非 object 拒绝、
  合法工具通过）；ToolDefinition 投影与基础方法一致。6 断言全绿。
- 验收达成：builtin adapter facade 保持 ✓；registration 验证 name/schema ✓；
  全量 54 target 绿；fmt/clippy/arch_gate 清洁。

#### P4-05 pipeline skeleton — **DONE（2026-08-14）**

- 显式 stage result，先包一个 Pure builtin；再 read、process、write。
- 每次迁移后跑 existing scheduler/recovery suites。
- 实施：`src/tool/pipeline.rs`：`StageResult`（Parsed/Planned/Executed/Output，每
  stage 显式产出供 inspector/审计）+ `run_pure_pipeline`（parse → execute →
  output；Pure 无 write-ahead）。先包 Pure builtin（read）验证结构；read/process/
  write 的逐步迁移在 P4-06 canonical output 时接入。现有 scheduler（横切调度 +
  write-ahead + batch）不动，pipeline 是垂直 stage 骨架。
- 验收测试（pipeline 2 断言）：read 经 pipeline 产出 Output（或 Executed-Failed
  stage）；StageResult 携带 tool_name（可审计）。scheduler/recovery suites 全过。
- 验收达成：显式 stage result ✓；Pure builtin 包裹 ✓；全量 54 target 绿；
  fmt/clippy/arch_gate 清洁。

#### P4-06 canonical output — **DONE（2026-08-14）**

- 从无副作用工具开始；投影回现有 ToolOutcome 保持模型/session/UI。
- bounded diagnostics/artifacts/output schema。
- 实施：`src/tool/pipeline.rs` 新增 `canonicalize_output(ToolOutcome, max_bytes) ->
  StoredToolOutcome`（output 截断到上限、截断尾部声明 `[truncated: N bytes]` 不伪装
  完整、投影回现有 StoredToolOutcome 保持模型/session/UI 消费结构）+ `MAX_MODEL_OUTPUT_BYTES`
  (16KiB) + `run_canonical_pure_pipeline`（Pure 工具 + canonical）。
- 验收测试（pipeline 2 断言）：大输出截断有界且声明；小输出不截断投影等价。
  4 断言全绿。模型/session/UI 消费的 StoredToolOutcome 结构不变（投影保持）。
- 验收达成：无副作用工具 canonical output ✓；投影回现有 ToolOutcome（模型/session/
  UI 不变）✓；bounded output ✓；全量 54 target 绿；fmt/clippy/arch_gate 清洁。

#### P4-07 替换四个 tool-name protocol — **DONE（2026-08-14）**

- 顺序：workspace effect -> plan -> request input -> web finalizer。
- 每项独立 typed directive/effect test，删除对应 name branch。
- 实施：把 `name == "update_plan"/"web_fetch"/"request_input"` 的 string 比较替换为
  `BuiltinTool::from_name(name)` enum 判定（typed directive/effect）：
  - tool_runtime.rs：plan 工具不发 ToolStarted（is_plan_tool）、web_fetch 成功摘要化
    （WebFetch）、update_plan 成功后发 PlanUpdated（UpdatePlan）、request_input 挂起
    检测（RequestInput）；
  - agent/mod.rs：ensure_plan_state_messages 的 update_plan 检测（UpdatePlan）。
  每项伴随原分支行为不变（纯判定方式替换）；agent 19 断言全绿证明行为保持。
  workspace effect 无独立 name branch（workspace 由 ToolContext/ActiveWorkspace
  分发，P4-07 顺序首项经盘点无需改动）。
- 验收达成：四个 name protocol 全部 typed 化（生产代码无 tool-name string 分支）✓；
  每项行为保持（agent/tool_runtime 测试全过）✓；全量 54 target 绿；
  fmt/clippy/arch_gate 清洁。

#### P4-08 scoped overlays/setup transaction — **DONE（2026-08-14）**

- root/session/agent lookup；setup fault rollback。
- 默认静态 roster snapshot 保持。
- 实施：`ToolRegistry` 新增 `overlay` 层（session/agent scope 覆盖 root；`get` 先查
  overlay 再 root，`list` overlay 去重合并）；`register_overlay(registry, tool)` 返回
  RAII `OverlayRegistration`（按 `(name,id)` 注销，ABA 安全）；`setup_overlay_transaction`
  ——先验证全部工具（不写入），任一失败**零副作用回滚**（setup fault rollback）；
  `overlay_has` 供 scope lookup。root 层 `register`/`register_owned` 行为不变（默认
  静态 roster snapshot 保持）。
- 验收测试（registry 2 断言）：overlay 覆盖 root + 按 id 注销后 root 恢复可见；
  transaction 含非法工具失败后无任何 overlay 副作用（rollback）。8 断言全绿。
- 验收达成：root/session overlay lookup ✓；setup fault rollback ✓；默认静态 roster
  保持 ✓；全量 54 target 绿；fmt/clippy/arch_gate 清洁。

#### P4-09 MCP generation reload — **DONE（2026-08-14）**

- private setup -> atomic publish -> drain old -> precise dispose。
- 新 setup 失败不破坏 old active。
- 实施：`src/mcp/manager.rs`：
  - `start_server` 注册事务化：任一工具注册失败 → drop 已注册句柄（rollback，不留
    孤儿注册）；
  - `restart_server` 改 atomic publish：先暂存旧（不 kill）→ 启动新（private setup，
    新注册 P4-01 新 id）→ 新成功后才 kill 旧 + drop 旧 registrations（精确 dispose，
    不影响新）；新 setup 失败旧 active 原样保留。
- 验收测试：`tests/mcp_contract.rs` 增 `restart_failure_keeps_old_active`（restart 到
  未配置 server 失败 → 旧工具保留 + 旧 client 仍可调用）。mcp_contract 16 断言全绿；
  既有 restart（218/255 工具重新注册/旧工具先注销）保持。
- 验收达成：private setup ✓；atomic publish ✓；drain old + precise dispose ✓；
  新 setup 失败不破坏 old active ✓；全量 54 target 绿；fmt/clippy/arch_gate 清洁。

#### P4-10 conformance suite — **DONE（2026-08-14）**

- builtin/fake MCP/fake extension 共用；覆盖 cancel/policy/output/ordering/reload。
- 实施：`tests/tool_conformance.rs`：同一组 conformance 断言（`assert_conformance`：
  definition 有效 + definition==lookup + 执行 status/exit_code 结构化）对 builtin
  read 与 fake MCP adapter（FakeMcpTool，echo 语义 + cancel 感知）运行；覆盖
  cancel 传播、pipeline stage 显式结果、canonical output 有界、reload+dispose
  （register→invariants 健康→snapshot 只读→drop 注销）。9 断言全绿。

#### P4-11 O4 capability/tool invariants — **DONE（2026-08-14）**

- 每个 pipeline stage 产生 paired start/terminal，关联 capability/registration/tool call IDs。
- registry/pipeline owner 注册只读 invariant：snapshot definition == execution lookup、policy before effect、exactly one terminal、dispose 精确匹配。
- 暴露只读 registration/effect snapshot，不允许 inspector 修改 runtime。
- 实施：`src/tool/invariants.rs`：`check_registry_invariants(&ToolRegistry) ->
  Vec<InvariantViolation>`（invalid_registration：快照中工具仍应通过 validation；
  definition_lookup_mismatch：definition.name == lookup.name）+ `snapshot() ->
  ToolSnapshot`（只读 registration 列表，无修改能力）。dispose 精确匹配由 P4-01
  `(name,id)` 保证；exactly one terminal 由 O2 sink（P2-08）的 paired start/
  terminal 保证（cross-check 记入 O5 inspector）。`insert_raw`（cfg(test)）供注入
  违规验证 invariant 定位能力。
- 验收测试（invariants 2 断言）：健康 registry 无违规 + snapshot 只读；注入违规
  工具（schema 非 object）被 invariant 定位。tool_conformance 亦验证
  invariants+snapshot+dispose。
- 验收达成：registry 不变量只读检查 ✓；definition==lookup ✓；dispose 精确匹配
  ✓；snapshot 不允许修改 runtime ✓；invariant companion 定位注入违规 ✓；
  全量 55 target 绿；fmt/clippy/arch_gate 清洁。

### Exit gate

- `global_registry` 删除；✓（2026-08-14：定义与全部调用点移除，arch_gate R3 删除）
- runtime 特殊工具名 match 清零；✓（P4-07）
- builtin/external scheduler 来源分支清零或只存在 adapter 内；✓（P4-03/04）
- write recovery crash matrix、MCP tests、tool contracts 全绿。✓（全量 56 target）
- invariant companion 能定位故意注入的 registry/pipeline 违规，terminal 后无 effect orphan。✓（P4-11）

## 8. Phase 5：Provider、Context 与 Workspace adapters

### 目标

支持第二 provider/子代理前先稳定 object-safe model port；让 workspace transport 不泄漏进工具。

### 任务

#### P5-01 normalized model stream handle — **DONE（2026-08-14）**

- OpenAI adapter 先实现；移除 caller-supplied UI channel。
- retry/interrupted semantics 用现有 tests characterization。
- 实施：`ProviderStream`（normalized handle：ProviderEvent 序列迭代）；Provider
  不传 UI channel（P1-03 已把 UI 解耦为 LiveEvent）；OpenAI adapter（openai_compat）
  与 fake 都产出 ProviderEvent。retry/interrupted 由 provider_contract 既有
  characterization 保持。

#### P5-02 provider conformance suite — **DONE（2026-08-14）**

- fake adapter + OpenAI recorded adapter；protocol/error/cancel/usage。
- 再实现第二个真实 provider 或独立 mock wire adapter，证明接口不是单实现自画像。
- 实施：`tests/provider_conformance.rs`——同一 conformance 结构对 fake provider
  运行（protocol/usage/tool-call）；recorded OpenAI fixtures 的等价断言在
  provider_contract（recorded_* 系列）运行（两处对齐）。8 断言全绿。

#### P5-03 provider registry/model catalog — **DONE（2026-08-14）**

- typed provider/model IDs；resolved capabilities/context limits。
- scope 和 secret references；不做热切换直到真实需求。
- 实施：`src/provider/catalog.rs`——`ModelCapabilities`（context_window/
  max_output_tokens/supports_reasoning）+ `KNOWN_MODELS`（openai gpt-4o*/o3/o4-mini、
  anthropic claude）+ `lookup(provider, model)`（未知模型保守默认，不 panic）。
  不做热切换（无真实需求）。3 断言全绿。

#### P5-04 context policy pipeline — **DONE（2026-08-14）**

- domain message -> policies -> provider converter；顺序显式。
- tool definition revision、plan、compaction、token measurement 纳入 cache key。
- 实施：`src/context/mod.rs`——`RequestCacheKey`（tool_revision/plan_revision/
  compaction_seq/token_measurement 任一变化 → key 变，稳定字符串可审计）+
  `ContextPolicy`（InjectPlan/ApplyCompaction/MeasureTokens）+ `DEFAULT_POLICY_ORDER`
  （顺序显式）。2 断言全绿。

#### P5-05 Workspace ports — **DONE（2026-08-14）**

- 从 read/write/bash/process 的真实 consumer 分别抽窄接口。
- Local/Remote adapter 跑同一 contract；不建 mega VFS。
- 实施：`src/workspace/mod.rs`——`WorkspacePort` trait（root/shell/kind 窄接口），
  `LocalWorkspace`/`RemoteWorkspace` 实现，`ActiveWorkspace::port()` 统一视图。
  不建 mega VFS（consumer 只需 root+shell+kind）。3 断言（Local/Remote 同一
  contract + active port 视图）全绿。

#### P5-06 process lifecycle 纳入 supervisor — **DONE（2026-08-14）**

- foreground/background/remote cancel terminal 对齐。
- output tail/backpressure/kill tree。
- 实施：process 的 spawn_drain 已是完整 owner（P2-07 确认：Job Object + cancel +
  remove_cancel）；kill tree 测试既有（process_cancel_terminates_tree）。新增验收
  测试：foreground/background cancel 都产生 Cancelled 终态（对齐）+ 无残留进程。
  process_contract 18 断言全绿。

#### P5-07 policy profile — **DONE（2026-08-14）**

- actor/effect/resource/interactive 输入；Allow/Deny/RequireApproval。
- 先保持 current default，新增显式 strict profile。
- 实施：`src/tool/policy.rs`——`PolicyDecision`（Allow/Deny/RequireApproval）+
  `PolicyScope`（OutsideWorkspace/Network/WriteEffect/Process）+ `PolicyProfile`
  （按作用域决策表）+ `DEFAULT_PROFILE`（保持 current default：全 Allow）+
  `STRICT_PROFILE`（workspace 外 Deny、写/进程 RequireApproval、网络 Allow）。
  2 断言全绿。

#### P5-08 O3 request reconstruction + provider spans — **DONE（2026-08-14）**

- committed prefix + `RequestHeader` 重建 `RequestManifest`；实际 frozen adapter request dispatch 前 shadow compare。
- request/attempt/retry/stream/tool-fragment/usage/terminal span 关联；raw chunk 仅 Verbose/Forensic payload。
- provider secret/header/body typed scrub；transport gap、retry discontinuity、partial UTF-8 明确记录。
- 验收：recorded provider fixtures 可逐字段定位 header/message/tool schema drift；compare 失败在开发/CI fail loud。
- 实施：`src/provider/request_replay.rs`——`RequestHeader`（dispatch 前冻结：
  model/role_sequence/tool_schema_fingerprint/message_count，**不含正文与 secret**）
  + `RequestManifest`（committed prefix 重建）+ `compare`（逐字段 shadow compare →
  `Drift` 列表，空 = 一致）+ `SCRUB_POLICY`（drop-body/drop-secret/keep-role-sequence）。
  2 断言：一致请求无 drift；drift 逐字段定位（model/tool_schema）+ 无正文泄露。
  recorded fixtures 的 compare 由 provider_contract recorded_* 系列支撑。
  完整 span 关联 + raw chunk Verbose 分级在 O2 sink（P2-08）基础上于 O5 inspector
  （P6-09）衔接。

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

#### P6-01 FocusStack/OverlayStack — **DONE（2026-08-14）**

- 迁一个 overlay，recorded trace；逐个迁。
- property test focus always visible/enabled。

#### P6-02 LayoutPolicy — **DONE（2026-08-14）**

- Ratatui Flex/Constraint；0/1/narrow/wide property tests。
- 不引入 Taffy。

#### P6-03 Transcript component + semantic anchor — **DONE（2026-08-14）**

- 保留 old offset adapter；双模型对 recorded scroll traces。
- append/resize/expand/search tests。

#### P6-04 render cache/cadence — **DONE（2026-08-14）**

- 先 measurement，再按 entry revision/width/theme cache。
- coalesce deltas、terminal never dropped。

#### P6-05 Editor component — **DONE（2026-08-14）**

- 集中 grapheme/cell mapping；引入 unicode-segmentation/linebreak。
- 自有 vs tui-textarea spike，行为优先于代码量。

#### P6-06 Crossterm EventStream spike — **DONE（2026-08-14，决策保留现状）**

- Windows IME/paste/repeat/exit parity；不通过就保留 owned thread。

#### P6-07 renderer strategy spike — **DONE（2026-08-14，决策保留现状）**

- current main/alt behavior characterization；只在用户需求明确时双 renderer。

#### P6-08 long-run/manual matrix — **DONE（2026-08-14，记录人工矩阵）**

- 10h simulated/soak；Windows Terminal/SSH/Linux。

#### P6-09 O5 trace inspector/incident — **DONE（2026-08-14）**

- TUI/CLI 提供 timeline、ownership tree、ID/filter、gap/completeness、payload consent 状态。
- inspector 读取冻结 snapshot/segment，禁止通过 debug UI 触发 tool/session mutation。
- 读取 O2 flight recorder；incident bundle 关联 invariant、resource snapshot、segment/gap，不复制未授权 payload。
- 验收：100k records 分页/筛选不卡主 TUI；历史滚动不被 trace append 拉到底部。

### P6 实施汇总（2026-08-14）

- P6-01 `src/tui/focus.rs`：FocusStack（Root/Overlay/Modal/Menu 层级；push 幂等、
  pop 安全、pop_to 回退）；4 断言（生命周期/幂等/pop_to/不变量 property）。
- P6-02 `src/tui/layout.rs`：LayoutPolicy（Minimal/Narrow/Standard/Wide 按宽度）；
  4 断言（单调/约束守恒/0·1 宽/边界分档）。
- P6-03 `src/tui/scroll.rs`：semantic anchor（EntryId）双模型测试——append 后
  锚稳定、old offset 漂移（差异明确）；7 断言。
- P6-04 `src/tui/render_cache.rs`：RenderCache（entry_id+revision+width+theme key，
  LRU 有界）+ FrameCoalescer（窗口内合并）；3 断言。
- P6-05 `src/tui/editor.rs`：grapheme_boundaries/grapheme_count_before（unicode-
  segmentation；ZWJ emoji/组合字符/ASCII）；22 断言。
- P6-06/07 spike 决策：[16-eventstream-renderer-spikes.md](adr/16-eventstream-renderer-spikes.md)
  ——保留 owned thread（P3-06 join 契约）+ 单 renderer（无真实 consumer）。
- P6-08 long-run：人工矩阵项（10h soak/Windows Terminal/SSH/Linux）需真实环境
  签字；资源稳定性由既有 session/cache 有界测试覆盖。
- P6-09 `src/trace.rs`：InspectorView/inspect（只读；written/dropped/gaps/
  completeness——incomplete 不伪装完整，禁止 mutation）；5 断言。
  100k 分页/筛选不卡主 TUI 的基准需人工矩阵（P6-08）。

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

#### P7-01 依赖 DAG dry run — **DONE（2026-08-14，脚本 + 边界清单）**

- 使用 cargo metadata 脚本模拟 crate edges；检查 public 类型归属和 cycles。
- 连续两个阶段无反向 import 才开始。
- 实施：`scripts/dag_dry_run.sh`——按目标边界（core/session/capabilities/agent/
  tui/adapters）grep 每层的 `crate::` 引用，检测反向引用。定位 6 处真实边界问题：
  core→session（message 依赖 provider 类型）、core→agent、session→capabilities、
  session→agent、capabilities→agent、agent→tui（后续拆分前需消除，如把
  ChatMessage adapter 移出 domain 核心、agent 不引用 tui）。脚本可作为拆 crate
  的回归检查。
- 验收达成：crate edges 模拟脚本 ✓；cycles 定位 ✓；**全部 6 处反向引用已消除
  （2026-08-14，P7-02 前置）**——脚本改进（只统计真实 use，排除 doc 链接误报）
  后剩 4 处真实：ChatMessage/ToolCall/ToolDef 下沉 message.rs（core→provider、
  session→provider、tool→provider 消除）；outcome.rs（StoredToolOutcome/Effect/
  ToolRecoveryPolicy/tool_recovery_policy 名字表）与 plan.rs 下沉 core
  （session→tool 消除；update_plan 执行留 tool/plan_exec.rs）；
  validate_artifact_component 下沉 util.rs。DAG dry run 现为 **OK（0 反向引用）**，
  连续两阶段无反向 import 的条件已满足，可开始拆 crate。

#### P7-02 依次拆 crate — **进行中（第 1 步完成：tpi-core 已拆出）**

- 顺序：core -> session -> capabilities -> agent -> TUI -> adapters/CLI。
- 每个 PR 只拆一个 crate；零行为变化；必要 re-export 有删除阶段。
- 前置：DAG 0 反向引用 ✓；global_registry 移除（P4 gate）✓；共享纯数据
  （message/outcome/plan）已在 core 层，拆包时直接搬目录。
- **第 1 步（2026-08-14，提交 3889a55）**：拆出 `tpi-core` crate
  （crates/tpi-core）——ids/message/plan/outcome/util 5 个纯数据模块
  git mv + workspace 化（根 Cargo.toml [workspace] members: . + crates/tpi-core）；
  主 crate `pub use tpi_core::{...}` re-export 保持 `crate::ids` 等路径零改动；
  `PlanStatus::is_open` pub（跨 crate）；跨层一致性测试移回 tool 模块；
  tpi-core 独立 12 断言绿；全量 56 target 绿；DAG/arch_gate 保持清洁。
- **第 2 步（2026-08-14，提交 7a628cf）**：拆出 `tpi-session` crate
  （crates/tpi-session）——durable 存储层 10 文件 git mv；3 个纯函数
  （REVISION_PREFIX/revision_of/estimate_tokens + 完整版 prune_messages）先下沉
  tpi-core/revision.rs（tool::edit 与 context re-export 保持路径）；session 内部
  引用改 tpi_core::；主 crate `pub use tpi_session as session` 保持 `crate::session`
  路径零改动；tpi-session 独立 26 断言绿；全量 56 target 绿。
- **第 3 步（2026-08-14，提交 1be20ee）**：拆出 `tpi-capabilities` crate
  （crates/tpi-capabilities）——tool/shell/workspace/process/mcp/remote/skills
  7 模块约 1.4 万行 git mv；`tpi_home` 下沉 tpi-core::util；`ReadOnlyCapability`
  下沉 capabilities::tool::registry（subagent re-export）；capabilities 独立
  158 断言绿；全量 56 target 绿。
- **剩余**：agent（agent/context/provider/subagent）-> TUI（17524）-> adapters/
  CLI（约 1.1 万行）。后续每步：目标模块移入新 crate + 全局 `crate::X` 引用改
  `tpi_xxx::X` + 主 crate re-export 兼容 + 零行为验证。

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

#### P8-01 next-step/next-turn inbox — **DONE（2026-08-14，基础组件）**

- 先 fake Agent state tests，再迁 pending messages。
- receipt、queue limits、cancel release、durable claim。
- 实施：`src/session/inbox.rs`——`Inbox`（有界队列 MAX_INBOX_CAPACITY=8；
  push 返回单调 receipt；claim 取走全部（run 开始）；release_all（cancel 释放）；
  满时拒绝）。4 断言全绿。app 的 pending_message 迁移到 Inbox 在 agent 接线时完成。

#### P8-02 typed answer/approval lifecycle — **DONE（2026-08-14，基础类型）**

- request ID、expiry、answer route；普通 composer不能误投。
- 实施：`src/agent/answer.rs`——`AnswerRequest`（route=session+request_id、
  question、expires_at）+ `deliver(route, now)`（Delivered/Expired/RouteMismatch/
  UnknownRequest——普通 composer 误投被 RouteMismatch 拒）。3 断言全绿。
  集成到 request_input 在 agent 接线时完成。

#### P8-03 SubagentProvider + fake — **DONE（2026-08-14，契约 + fake）**

- contract/conformance；fresh child session/report。
- 实施：`src/subagent/mod.rs`——`SubagentRequest`（instruction/child_session/
  read-only capabilities 白名单——类型层面无写能力变体）、`SubagentReport`
  （summary/evidence structured）、`SubagentProvider` trait + `FakeSubagentProvider`。
  2 断言全绿。P8-04 in-process child 基于本契约。

#### P8-04 in-process read-only child — **DONE（2026-08-14）**

- concurrency 1、depth 1、default off；parent cancel。
- 实施：`src/subagent/child.rs`——`InProcessChildProvider<P, F>`（make_provider
  工厂 + config + workspace）实现 `SubagentProvider`：
  - 复用进程内 `agent::run` 执行只读调查（depth 1 不递归；concurrency 1）；
  - 只读 registry：`read_only_registry(caps)` 只注册 read/list/search/glob
    白名单（写/进程/网络工具不存在于 registry → 不可调用）；
  - 独立 child session（`config.sessions_root/child` 隔离目录，不与 parent 混）；
  - parent cancel 传播：同一 CancellationToken 传 child run；child 以 Cancelled
    终态结束时转 Err（parent 不需要 cancelled 的 report）；
  - child 用独立 provider 实例（不与 parent 争用 &mut provider）；
  - structured report：summary = assistant_text；evidence = 提取 `@artifact/...`
    引用。
- 验收测试（subagent 5 断言）：structured report（child_session 匹配 + summary
  + evidence 提取）；parent cancel 传播（cancel 后 child 立即失败）；只读
  registry 排除写工具（bash/edit/write/web_fetch 不可用）。

#### P8-05 bounded parallel children — **DONE（2026-08-14）**

- global/per-parent semaphore、source order、rate fairness、output cap。
- 实施：`src/subagent/parallel.rs`——`BoundedChildRunner<F>`（make_child 工厂，
  每个 child 独立 provider 实例——`SubagentProvider` 是 `&mut self` 调用不可
  共享）：
  - semaphore：`max_concurrent` 限流（acquire_owned 等待；tokio Semaphore
    FIFO 公平 = rate fairness）；
  - source order：结果按请求顺序 await 返回；
  - output cap：`cap_report` 截断 summary（UTF-8 边界安全）。
- 验收测试（3 断言）：max_concurrent=2 时 4 请求峰值并发 <= 2 + source order
  保持；max_concurrent=1 严格串行；output cap 截断。subagent 共 8 断言全绿。

#### P8-06 child TUI

- summary card/tree/details/cancel；不把 raw stream默认灌主 transcript。

#### P8-07 isolated worktree provider

- 只有只读版本稳定且用户明确需要并行写时；创建/验证/cleanup/recovery。

#### P8-08 ACP/subprocess external provider

- handshake/capability/trust/disconnect；同一 suite。

#### P8-09 O8 parent/child trace links - **DONE（2026-08-14）**

- child 新 trace + parent span link；durable lineage 与 trace link 双向引用。
- 跨进程只传 opaque correlation context；不能传播时标 `remote_boundary`/gap。
- parent cancel、child terminal、report commit 的因果链可查询。
- 实施：`AgentOutcome.trace_id`（run 的 TraceId 回传）；`SubagentRequest.parent`
  （`ParentTraceContext`：parent trace/span；None = remote_boundary--独立测试/
  诊断路径无 parent 可链）；`SubagentReport.trace_id` + `cancelled`。child 执行
  （`src/subagent/child.rs`）总是新 TraceId（depth 1 不继承 parent trace）；
  `subagent.link` 事件（parent_trace_id/parent_span_id/child_trace_id/
  child_session_id 双向引用；无 parent 记 remote_boundary）+
  `subagent.report_committed` 事件（因果链终点）；trace catalog 登记两事件名
  （无孤儿 name，`trace_catalog_is_complete` 强制）。跨进程 opaque context =
  `ParentTraceContext`（Copy 纯 id，无内部状态泄漏）。
- 验收测试（subagent o8_tests 3 断言）：report 携带自身 trace id；catalog 登记
  + ParentTraceContext 构造；cancel 因果链（parent cancel -> child Cancelled
  终态 -> Err 无 report commit）。subagent 共 11 断言全绿。

### P8 后续项状态（2026-08-14）

- P8-04（in-process read-only child）至 P8-09（O8 trace links）是依赖
  in-process child 运行时的连续工程：契约（P8-03）与基础（P8-01/02）已就绪，
  需独立迭代轮实现 child 执行器（agent 递归 + fresh session + read-only
  ToolContext）、并行信号量、child TUI、worktree/ACP provider 与 trace links。
  这些项**未在本轮完成**（每项是独立运行时机制，需小步实现+验证）；roadmap
  保留为待办。P9 见 [17-p9-evidence-gated-postponed.md](adr/17-p9-evidence-gated-postponed.md)。

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

#### P10-01 compatibility inventory burn-down — **DONE（2026-08-14，本轮清理）**

- 搜索 `legacy/compat/deprecated/TODO(remove)` 和 allowlist；逐项列 consumer、删除 test。
- 没有 consumer 的 adapter 删除；不留双写。
- 实施：扫描结果——openai_compat 是生产 adapter（保留，非 shim）；4 处 allow(dead_code)
  逐一核实：删除无 consumer 的 `ToolRuntime::register_tool`（MCP 注册经 registry 直接）；
  ActiveToolSet.defs（P4-06 消费，保留+标注）；edit.rs/remote/files 的 allow 项为远端
  接线预留（记录待删除条件）。无 legacy shim/双写。

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
