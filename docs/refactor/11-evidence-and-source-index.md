# 11. 证据、版本与资料索引

## 1. 用途

本索引让执行者区分：

- 当前仓库的直接证据；
- 参考项目的设计证据；
- 候选 Rust 库/协议的官方能力说明；
- 尚未由 benchmark、威胁模型或真实 consumer 证明的提案。

链接可证明“库提供什么”，不能单独证明“适合 TPI”。正式引入仍需 [03-rust-technology-decisions.md](03-rust-technology-decisions.md) 的 spike gate。

资料在线状态与版本可能变化。每个 Phase 开始时只重新核验该 Phase 要采用的项目，并把日期、版本、MSRV、license 和 changelog 记入 ADR/依赖 PR。

## 2. 本地源码基线

| 项目 | 路径 | 审计 commit | 主要证据 |
|---|---|---|---|
| TPI | `C:\Users\tyd27\Desktop\tpi` | `2202887355174aa49b4796b2d86178b9c1dff9ef` | 当前 ownership、session、tool、TUI、tests、Cargo graph |
| DeepSeek Harness | `C:\Users\tyd27\Desktop\deepseek-harness` | `47f943859bef60e4160492346772ded9b24f765a` | capability seam、scoped registration、lifecycle、subagent family |
| Pi | `C:\Users\tyd27\Desktop\pi` | `b1efcf7d7c5d7394fbb12ede0174e04d39ee7004` | minimal harness、message conversion、steering/followup、TUI component |

审计 commit 不是 pin 依赖。参考上游变化时，执行者不得模糊写“最新 Pi/DSH 已这样做”，必须记录新的完整 hash 和具体文件。

## 3. TPI 直接证据入口

| 主题 | 位置 | 本计划中的结论 |
|---|---|---|
| 既有架构原则 | `README.md`、`docs/architecture.md` | Session truth/context projection/thin loop/effect owner 应保留 |
| composition/event pump | `src/main.rs`、`src/app.rs` | app 多 owner，需要分 composition/controller/terminal adapter |
| Agent state/stream | `src/agent/mod.rs` | runtime event 与 view data 混合；Run/Turn/Attempt需统一 |
| Tool orchestration | `src/agent/tool_runtime.rs` | builtin/external 分支、特殊 name、forwarder lifecycle |
| Tool contract/registry | `src/tool/mod.rs`、`src/tool/registry.rs` | global registry、ABA disposer、contract metadata不足 |
| Session protocol/store | `src/session/mod.rs`、`conversation.rs`、`recovery.rs` | JSONL truth和恢复测试资产强，应拆 owner 不换 truth |
| Provider | `src/provider/mod.rs`、`openai_compat.rs` | provider-specific wire已隔离，但 public stream 偏单实现/channel |
| Workspace/remote | `src/workspace.rs`、`src/remote/*`、`src/process/*` | 抽真实 File/Command/Process ports，保留 local/remote parity |
| TUI | `src/tui/*` | focus/layout/scroll/editor可组件化；已有大量行为测试 |
| Remote fixture failure | `tests/fixtures/remote_server.rs:410` | 未声明 `cygpath` 前提造成 5 targets/24 cases fail |

开始任务时以当前源码行号为准；本表只给符号入口。

## 4. DeepSeek Harness 证据入口

| 主题 | 本地文件 |
|---|---|
| everything-is-plugin 定位/包族 | `AGENTS.md`、`README.md` |
| capability seam 角色规则 | `packages/AGENTS.md`、`docs/glossary.md`、相关 Agent Note |
| Agent lifecycle/inbox claim | `docs/agent-lifecycle.md` |
| registration/disposer/effect | `AGENTS.md`、core/tool registry packages/tests |
| setup/publication/rollback | agent registry/bundle startup/ACP tests |
| tool pipeline/scopes | `packages/tools/*`、`packages/core/*` |
| subagent provider family | `packages/subagent/*` |
| composition/bundles | `packages/bundle/*`、boot/loader docs |
| strict product composition tests | `packages/AGENTS.md`、`docs/testing.md` |
| canonical session events/request header/raw chunks | `packages/session/types/src/*`、agent-loop/session integration source/tests |
| model request reconstruction invariant | agent-loop dispatch/request reconstruction source/tests |
| live/replay session telemetry | `packages/session/session-telemetry/README.md` 及 source/tests |
| OpenTelemetry modes/redaction warning | `packages/session/session-telemetry-otel/README.md` 及 source/tests |
| package-owned runtime invariants | `packages/runtime-diagnostics/invariants/README.md` 及 companion registrations/tests |
| event producer/consumer catalog | `docs/event-producer-consumer.md` 及生成脚本 |
| effect/fiber ownership tree | Cordis effect/fiber docs、`getEffects()` source/tests |

复核时优先读源码/tests 和 current architecture docs；`.agents/notes/archive` 是历史解释，不能凌驾当前实现。

## 5. Pi 证据入口

| 主题 | 本地文件 |
|---|---|
| minimal harness/core philosophy | `README.md` |
| AgentMessage -> LLM Message | `packages/agent/README.md` 及对应 source/tests |
| agent/turn/message/tool events | `packages/agent/*` |
| steering/followup/abort restore | `packages/coding-agent/README.md`、interactive source/tests |
| JSONL tree/branch/compaction | coding-agent session docs/source/tests |
| TUI components/focus/layout/render | `packages/tui/README.md`、source/tests |
| subagent extension | `packages/coding-agent/examples/extensions/subagent/*` |

Pi 的默认 full-access/security philosophy 不是 TPI 要采用的安全规格；本计划只借鉴边界和 UX。

## 6. 官方 Rust/协议资料

### 6.1 异步与 middleware

- Tokio-util [`TaskTracker`](https://docs.rs/tokio-util/latest/tokio_util/task/task_tracker/struct.TaskTracker.html)：tracked task、close/wait；与 CancellationToken 组合建立 quiescence。
- Tower [`Service`](https://docs.rs/tower/latest/tower/trait.Service.html)、[`Layer`](https://docs.rs/tower/latest/tower/layer/trait.Layer.html)、[`ServiceBuilder`](https://docs.rs/tower/latest/tower/struct.ServiceBuilder.html)：request/response abstraction、middleware 与顺序。
- Crossterm [`EventStream`](https://docs.rs/crossterm/latest/crossterm/event/struct.EventStream.html)：feature-gated asynchronous terminal event stream。

### 6.2 TUI 与 Unicode

- Ratatui [v0.30 highlights](https://ratatui.rs/highlights/v030/) 和 [Flex example](https://ratatui.rs/examples/layout/flex/)：当前 major/minor 的 layout primitives。
- [`unicode-segmentation`](https://docs.rs/crate/unicode-segmentation/latest)：UAX #29 grapheme/word/sentence boundaries。
- [`unicode-linebreak`](https://docs.rs/unicode-linebreak/latest/unicode_linebreak/)：UAX #14 line break opportunities。
- [`tui-textarea`](https://docs.rs/tui-textarea/latest/tui_textarea/)：multiline editing、undo/redo、selection/search 等候选行为。
- [`tui-scrollview`](https://docs.rs/tui-scrollview/latest/tui_scrollview/)：Ratatui scroll viewport/state 候选。
- [`taffy`](https://docs.rs/taffy/latest/taffy/)：flex/grid/block layout tree；当前仅延后候选。

### 6.3 Schema、诊断、存储

- [`jsonschema`](https://docs.rs/jsonschema/latest/jsonschema/)：JSON Schema validator、meta validation、reusable compiled validation。
- [`miette::Diagnostic`](https://docs.rs/miette/latest/miette/derive.Diagnostic.html)：带 code/help/source labels 的用户诊断。
- [`redb`](https://docs.rs/crate/redb/latest)：pure-Rust ACID embedded KV；当前只作 SQLite 的条件备选。
- [`rusqlite` hooks/WAL API](https://docs.rs/rusqlite/latest/rusqlite/hooks/index.html)：SQLite adapter 能力；只考虑派生索引。

### 6.4 扩展协议

- [MCP official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)：Tokio-based official protocol SDK；替换当前 client 前要做 parity。
- [Agent Client Protocol](https://zed.dev/acp)：editor/client 与 coding agent 的开放互操作协议；用于未来外部 subagent/surface 候选。
- [Wasmtime book](https://docs.wasmtime.dev/) 与 [Component API](https://docs.wasmtime.dev/api/wasmtime/component/struct.Component.html)：WASI/Component Model、受控 host capability 候选。
- Rust Reference 的 [external blocks/ABI](https://doc.rust-lang.org/reference/items/external-blocks.html)：Rust ABI 无稳定保证，是拒绝原生 Rust dylib 生态边界的主要依据。

### 6.5 质量工具

- [cargo-nextest](https://nexte.st/)：per-test process isolation、filters、timeouts、record/replay、CI integration；doctest 单独跑。
- [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov)：LLVM source-based line/region coverage、nextest integration、thresholds。
- [cargo-deny](https://embarkstudios.github.io/cargo-deny/)：advisories、license、ban、source policy。
- [cargo-mutants](https://mutants.rs/)：Rust mutation testing。

### 6.6 Tracing、导出与错误因果

- [`tracing::span::EnteredSpan`](https://docs.rs/tracing/latest/tracing/span/struct.EnteredSpan.html)：官方明确警告 enter guard 跨 `.await` 会生成错误 traces。
- [`tracing::Instrument`](https://docs.rs/tracing/latest/tracing/trait.Instrument.html)：把 future 的 poll 生命周期放入指定 span；异步 instrumentation 的首选边界。
- [`tracing-appender::non_blocking`](https://docs.rs/tracing-appender/latest/tracing_appender/fn.non_blocking.html) 与 [`WorkerGuard`](https://docs.rs/tracing-appender/latest/tracing_appender/non_blocking/struct.WorkerGuard.html)：固定队列、lossy/backpressure 取舍和退出 flush 所有权；实际采用前读取对应版本 builder/source 文档。
- [`tracing-error::SpanTrace`](https://docs.rs/tracing-error/latest/tracing_error/struct.SpanTrace.html)：在错误产生处捕获 span context 的候选；只作 typed error 的补充。
- [`tracing-opentelemetry`](https://docs.rs/tracing-opentelemetry/latest/tracing_opentelemetry/) 与 [OpenTelemetry Rust trace docs](https://docs.rs/opentelemetry/latest/opentelemetry/trace/)：可选 exporter bridge/trace API；不能成为 TPI 本地 schema 或 session truth。
- [OpenTelemetry language status](https://opentelemetry.io/docs/languages/): Rust 各 signal 的成熟度会变化；每次引入 exporter 时重新核验 signal/status/version，不在本计划冻结承诺。

## 7. 版本记录

审计时 TPI `cargo tree --depth 1` 的关键解析版本包括：Tokio 1.53.1、tokio-util 0.7.19、Ratatui 0.30.2、Reqwest 0.13.4、Serde 1.0.229、Schemars 1.2.2、Russh 0.62.6、Pulldown-cmark 0.13.4、Unicode-width 0.2.2、Thiserror 2.0.20。

在线调研时官方文档可见的候选版本包括 Tower 0.5.3、Crossterm 0.29、unicode-segmentation 1.13.3、unicode-linebreak 0.1.5、tui-textarea 0.7、tui-scrollview 0.6.7、jsonschema 0.49.2、miette 7.6.0、redb 4.1、tracing 0.1.44、tracing-appender 0.2.5、tracing-error 0.2.1、tracing-opentelemetry 0.33 和 OpenTelemetry Rust 0.32。**这些不是批准 pin**；实际引入必须重新检查兼容/MSRV/changelog/lockfile，尤其不能把某日 signal 成熟度当永久事实。

## 8. 成熟度判断表

| 证据 | 能说明 | 不能说明 |
|---|---|---|
| 官方文档/API | 功能和公开 contract | TPI 兼容、bug 数量、性能 |
| 当前维护/release | 项目仍活跃 | 没有 breaking change/安全风险 |
| 已被 Ratatui/Tokio 等生态采用 | 集成信号 | TPI 特定行为 parity |
| benchmark | 特定 fixture 性能 | 全部用户 workload |
| conformance suite | contract 等价 | 人工 UX 自然 |
| threat model/fault tests | 已覆盖风险 | 完全安全 |
| DeepSeek/Pi 已实现 | 设计存在真实 consumer | Rust/Windows/TPI 应照搬 |

正式决策至少需要“官方资料 + TPI spike/conformance + 依赖/安全审查”，而不是 star/download 数量。

## 9. 每次重新核验模板

```text
Date:
TPI commit/Rust toolchain:
Candidate/version/source URL:
License/MSRV/platform:
Relevant changelog/security advisories:
Features/default features:
Transitive diff/duplicates:
TPI consumer/problem evidence:
Spike commit:
Behavior tests:
Benchmark/resource result:
Threat/permission result:
Decision: adopt / reject / defer
ADR/link:
Revisit trigger:
```

没有填写完整就保持现状，不把“调研过”误写为“批准采用”。
