# 09. 测试、观测、CI 与发布门禁

## 1. 测试原则

重构需要证明“旧的正确行为仍正确”和“新的边界真的能替换实现”。测试按风险建立，不按模块平均追 coverage。

```text
          少量 real-provider / manual terminal
        E2E product composition / recovery drills
      Adapter conformance / integration / fault tests
    State/property/model tests / golden projections
  Pure unit tests / parsers / algorithms / typed transitions
```

不要用大量 mock 验证“调用了某方法”。优先验证 durable、model-visible、user-visible outcome。

## 2. 测试类型与 owner

### 2.1 Characterization tests

在移动旧代码前冻结实际行为。命名带 `legacy_behavior_...` 也可以，但任务完成后要决定：这是长期 contract，还是新实现稳定后可删除的迁移 scaffolding。

适用：`app.rs` event flow、TUI buffer、provider request JSON、session encoding、slash command。

### 2.2 Unit tests

用于纯算法和局部状态：Unicode mapping、line wrapping、path canonicalization、schema validation、backoff、queue、state transition、projection apply。

### 2.3 Contract/conformance tests

对每个 port 提供 reusable suite：

```text
SessionStoreConformance
ModelAdapterConformance
ToolHandler/PipelineConformance
WorkspaceFile/Command/ProcessConformance
SubagentProviderConformance
Renderer/Component semantic contract
```

suite 接受 factory/harness。实现不能只复制 tests 后修改期望；公共 contract 在 capability crate 定义。

### 2.4 Integration tests

组合真实内部模块，mock only external nondeterminism：fake HTTP server、fake MCP server、temp workspace、recorded terminal。验证一次完整 use case，不调用 private function 拼装不存在的状态。

### 2.5 Golden/snapshot tests

适用：session wire、model request、TUI buffer、diagnostics、child report。规则：

- fixture 小且有名字表达场景；
- dynamic ID/time/path scrub 使用 typed normalizer，不用宽泛 regex 删除信息；
- snapshot update 必须阅读 diff；CI 不自动 accept；
- snapshot 不是唯一 assertion，同时检查关键 semantic fields；
- 旧 session fixture 永不被 current writer 原地更新。

### 2.6 Property/state-sequence tests

使用 Proptest 生成合法/非法 sequence：

- session protocol：每个 requested call 一个 terminal；seq 单调；
- registry：任意 register/override/drop sequence 后 active entry 正确；
- inbox：insert/claim/cancel/restart 不丢/重复 model-visible message；
- scroll：append/resize/fold/search 后 anchor 合法；
- editor：任意 grapheme edit 后 cursor 在合法 UTF-8/grapheme boundary；
- scheduler：冲突工具不重叠；result source-order；
- budget：消耗单调且不超过上限；
- branch：投影只含 ancestor events。

失败 case 保存 seed/minimal reproduction。

### 2.7 Concurrency model tests

Loom 只用于缩小后的同步核心：

- generation registry register/replace/drop；
- supervisor close/cancel/spawn race；
- inbox claim vs cancel/insert；
- one-shot terminal completion；
- once-published Agent handle。

真实 async integration tests 再覆盖 Tokio/process。不要尝试把 Reqwest/Ratatui/整个 Agent 放进 Loom。

### 2.8 Fault injection

在拥有副作用顺序的模块提供命名 fault points，仅测试/feature 可用：

```text
session.before_append / after_write / before_sync / after_sync
tool.after_write_ahead / before_effect / after_effect / before_terminal
agent.after_message_commit / before_tool_wave / before_run_terminal
mcp.after_spawn / after_handshake / before_publish / during_drain
subagent.after_session / after_scope / after_task / before_report_commit
```

fault 是返回错误/模拟 crash harness，不用 sleep。生产构建不能允许用户随意触发。

### 2.9 Mutation testing

定时运行 cargo-mutants，优先目录：session protocol/projectors、policy、registry、state transition、scroll math、path/SSRF。survivor 分类：

- equivalent mutant：记录/排除精确位置；
- test gap：补测试；
- irrelevant code：考虑删除；
- flaky/timeout：修 harness，不调大统一 timeout。

## 3. 关键矩阵

### 3.1 Agent x Provider x Tool

| 场景 | Fake | Recorded OpenAI | Live opt-in |
|---|---:|---:|---:|
| text stop | PR | PR | nightly/manual |
| pure tool call | PR | PR | optional |
| parallel tools | PR | PR | optional |
| invalid tool args | PR | PR | no need |
| pre-stream retry | PR | PR | optional |
| mid-stream interrupt | PR | PR | optional |
| cancellation each await | PR | selected | no need |
| context overflow/compaction | PR | recorded | optional |
| awaiting input/resume | PR | recorded | optional |

### 3.2 Workspace x OS

| Adapter | Windows | Linux | Fault/cancel |
|---|---:|---:|---:|
| Local files | PR | PR | PR |
| Local bash/process tree | PR | PR | PR |
| SSH files/commands | PR | PR | PR |
| Remote background process | PR | PR | PR |
| Worktree child（未来） | PR | PR | PR |

fixture 启动时列出 external prerequisites；缺失时 test framework 输出 structured skip，CI required job 必须安装/提供而不是全 skip。

### 3.3 TUI terminal matrix

自动 backend tests 每 PR；Windows Terminal/ConPTY/Linux terminal smoke 每 release candidate；IME/paste/mouse/resize/long streaming 人工 + recorded reproduction。

## 4. 性能基准

### 4.1 Micro/algorithm

- session decode/replay 1k/10k/100k events；
- context projection with/without compaction；
- Markdown parse/layout short/large/streaming；
- Unicode wrap/mouse mapping；
- search traversal/result paging；
- edit locate/diff on representative files；
- registry snapshot 10/100/1000 tools；
- tool schema validation cache。

### 4.2 End-to-end

- app start to first frame；
- submit to local receipt；
- fake provider delta to frame；
- tool start to running card；
- cancel to process/task terminal；
- resume large session；
- 4 parallel read-only children；
- shutdown with active provider/tool/MCP/child。

### 4.3 Anti-regression policy

- 同机器/CI runner 对 base/head 多次采样；
- noisy benchmark 先统计分布，不用一次结果 gate；
- 对关键复杂度设置输入倍增 test，例如 10x tokens 不能接近 100x time；
- 任何缓存优化同时测内存和 invalidation correctness；
- 性能 PR 必须保留行为 tests。

## 5. 资源泄漏与 soak

测试前后采样：

- Tokio tracked task count；
- OS threads；
- child processes/job objects；
- open session/artifact files；
- MCP transports/registrations；
- cache entries/bytes；
- channel queued/dropped；
- RSS/private bytes；
- temp/backup/worktree directories。

Soak scenarios：

1. 10 小时模拟 stream/scroll；
2. 1,000 次 short run；
3. 100 次 MCP restart；
4. 100 次 child start/cancel；
5. 100 次 session open/switch/close；
6. 工具输出持续超过 UI 消费速度；
7. remote disconnect/reconnect。

预期是资源回到 baseline 或 bounded cache 高水位，不要求分配器立刻归还所有 RSS，但必须解释。

## 6. 观测设计

本节只定义测试/发布门禁；字段、planes、级别、存储、隐私、重放和实施顺序以 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md) 为准。四个 plane 必须分开：Session Ledger 是恢复事实，Trace 是执行因果，Payload 是受控正文，Metrics 是低基数聚合。

### 6.1 Tracing spans

稳定 IDs 关联：

```text
session_id, run_id, turn_id, step_id, request_id,
attempt_id, tool_call_id, capability_id, registration_id,
child_id, provider_id, workspace_id
```

span hierarchy 对齐 ownership；异步边界用 explicit parent/link + instrumented future，禁止 enter guard 跨 await。不要把完整 prompt、工具输出、路径、env、token 作为默认字段。

每条 record 还必须声明：schema version、单调 seq/时间、kind/name/level/outcome、source location、sensitivity、completeness；被采样、截断、脱敏、queue drop 或 remote boundary 时生成 gap/flag，不把残缺数据显示成完整。

### 6.2 Trace contract 与 replay tests

PR gate 中运行纯内存 capture subscriber/sink：

1. **Ancestry**：`agent.run -> step -> request/attempt -> tool wave/call` 的 parent/link 与 await/并发后仍正确；
2. **Cardinality**：每个 start 恰有一个 completed/failed/cancelled/interrupted terminal；retry attempt 不覆盖 request identity；
3. **Ordering**：session seq 单调，单 producer trace seq 单调；跨 child/session 不虚构全序；
4. **Live/replay equivalence**：同一 committed prefix 的 telemetry records 在允许的 timestamp 差异外相同；
5. **Request reconstruction**：session-derived manifest 等于 frozen adapter dispatch；差异报告字段路径/source event；
6. **Backpressure**：slow sink/queue full 不阻塞 agent；drop count 与 `TraceGap` 一致，terminal/invariant 保留策略生效；
7. **Redaction**：credential/prompt/tool body/path/username canary 在 Standard、bundle、remote export 的预期位置为零；
8. **Replay safety**：inspector/replay 使用 fake/no-effect adapters，绝不重新执行工具、网络、进程或 session append；
9. **Shutdown/recovery**：guard 有 owner；正常退出 deadline 内 flush，异常/截断 segment 可扫描并标 incomplete；
10. **Disabled parity**：Off、Standard、无 subscriber 的业务 event/session/output 完全等价。

测试禁止只断言字符串日志；应 decode `TraceRecord` 后按 typed IDs、kind、outcome、sensitivity 和 completeness 断言。

### 6.3 Runtime invariants

每个 owner 随实现提供 companion checks；中央 registry 只调度和聚合，不了解全部领域规则。首批必须覆盖：

- session enclosure、tool call/result cardinality、run terminal 无悬挂 effect；
- frozen model request 与 session projection 一致；stream grammar/tool-argument merge 完整；
- inbox FIFO/claim/discard、awaiting input request/answer 配对；
- registry snapshot identity、policy-before-effect、registration precise dispose；
- task/process/MCP/child 在 terminal/shutdown 后无 orphan；
- TUI projection revision 单调，不能把旧 stream delta 应用到新 run。

开发/CI 的确定性 invariant failure 直接 fail test/run；release 中先结构化记录、停止危险 effect、返回 typed internal error。不能 panic 整个 terminal，也不能 warning 后继续写可能错误的事实。

### 6.4 Metrics

建议低基数：

- runs/steps/tool calls by status/category；
- provider request duration/retries/interrupted；
- tool queue/execution duration/effect class；
- session append/sync/replay duration/corruption；
- channel depth/coalesced/dropped delta；
- render/layout/parse duration/cache hit；
- inbox wait/claim/queue full；
- child queued/running/terminal/budget；
- task/process shutdown duration/forced kill。

工具名称、路径、session ID 不做 metrics label，避免高基数；放 trace/log 中并 scrub。

### 6.5 Diagnostics bundle

`tpi doctor --bundle`（未来）只在用户显式要求时创建：版本、config schema（secret redacted）、dependency/features、session diagnose summary、recent structured errors、terminal capabilities、trace manifest/gap/invariant summary。默认不含会话正文/API key/env。生成前展示内容清单、每类 sensitivity、缺口和目标路径；本地 Forensic capture 不自动等于允许远程上传。

## 7. CI pipeline

### Fast PR gate

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks
architecture/dependency gate
cargo deny check
cargo nextest run --profile ci
cargo test --doc
snapshot/golden cleanliness
trace schema/catalog + ancestry/cardinality/redaction tests
```

Nextest 不支持 doctest，因此 `cargo test --doc` 是独立必需步骤。

### Platform gate

- Windows stable/MSRV；Linux stable/MSRV；
- default features + all features；
- remote fixture required job；
- no-network/offline unit suite；
- feature powerset 可在 nightly/merge queue 运行。

### Scheduled gate

- llvm-cov report；
- cargo-mutants critical modules；
- Miri selected；
- dependency freshness/advisory；
- performance comparison；
- soak/fault matrix；
- live provider opt-in（有 secret 才运行，结果不泄漏）。
- trace sink slow-disk/backpressure/rotation/recovery 与 invariant fault matrix。

### Release gate

- release build/package/install/upgrade/downgrade rehearsal；
- session corpus migration；
- manual TUI matrix；
- SBOM/license；
- artifact signing/checksum（若已有发布流程）；
- known issues/rollback notes。
- Standard trace 开销/retention、secret canary、diagnostic bundle 预览和 disabled parity。

## 8. Coverage policy

不设“全仓 100%”这种会鼓励无意义测试的指标。建议：

- session codec/protocol/recovery、registry/state transition、policy：branch/region 高覆盖且 mutation score 门禁；
- adapters：conformance 的行为覆盖优先；
- TUI render：semantic assertions + targeted snapshots；
- changed code coverage 作为 review signal，关键路径低于约定阈值则阻塞；
- platform-specific unreachable code 要有对应平台 CI，不靠 `cfg` 排除后宣称覆盖。

阈值在 P0 采当前 coverage 后由 ADR 决定，逐步上升，不因重构突然设置无法达到的门槛。

## 9. Flaky test 政策

1. 第一次发现：保存 seed/log/trace，标 flaky issue；
2. 允许 nextest 对已知外部不稳定 test 做精确有限 retry，并报告 retry count；
3. 核心 state/session/registry test 不允许 retry 变绿；
4. quarantine 必须有 owner、原因、到期日，required coverage 由替代 test 保持；
5. 禁止全局提高 timeout、加 sleep、忽略 exit status；
6. 三次重复仍无法定位时缩小 harness/fake clock，而非永久跳过。

## 10. 发布与 rollout

### 10.1 Feature flags

迁移 flag 只用于短期选择实现，不应长期变成组合爆炸。每个 flag 有 owner、默认、telemetry/diagnostic、删除版本。

### 10.2 Shadow read/project

新 session projector、context builder、TUI view projector 可在 dev/CI shadow 运行：旧路径仍做决定，新路径只比较结果。禁止 shadow path 执行工具/写 session/启动进程。

### 10.3 Canary

个人项目也可按 session/profile canary：先对新 session 启用新 read-only projection；旧 session 仍旧路径。明确 opt-in，遇 mismatch 自动回旧 reader并保留诊断，不自动改数据。

### 10.4 版本兼容

release notes 必须列：

- session reader/writer schema；
- config rename/default change；
- tool/model surface change；
- keymap/UX change；
- new external protocol；
- downgrade 限制；
- backup/recovery命令。

## 11. 每阶段证据包

阶段 exit 时生成一个简短 evidence index：

```text
baseline/head commits
ADRs
changed invariants
trace schema/catalog version
trace completeness/drop/redaction result
test commands/results
coverage/mutation links
benchmark before/after
manual matrix
session migration/corpus results
known risks
rollback commit/flag
compatibility items remaining
```

没有 evidence index 就不能宣布阶段完成。
