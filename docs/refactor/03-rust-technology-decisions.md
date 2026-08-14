# 03. Rust 库、算法与协议选型

## 1. 选型门槛

“成熟先进”不等于“最新版全装”。任何新依赖必须填写：

```text
当前问题与 profiling/bug 证据：
现有实现为什么不足：
候选库的维护状态、MSRV、许可证：
跨 Windows/Linux 支持：
直接与传递依赖变化：
安全边界和 unsafe 情况：
最小 spike：
行为/性能验收：
撤回条件：
```

采用状态：

- **保留**：已经在 TPI 使用，继续作为基线；
- **采用**：问题明确、接口稳定、收益直接，可进入对应 Phase；
- **试验**：必须做短期 branch/spike，不通过即删除；
- **延后**：有潜在用途，但当前没有证据；
- **拒绝**：与目标不符或风险不值得。

## 2. 异步、服务与生命周期

| 库/机制 | 决策 | 用途 | 理由与边界 |
|---|---|---|---|
| Tokio 1.x | 保留 | runtime、channel、process、time | 当前全项目基础，不更换 runtime |
| `tokio_util::sync::CancellationToken` | 保留并标准化 | 层级取消 | 当前已用；所有 owner 从父 token 建 child token |
| `tokio_util::task::TaskTracker` | 采用 | agent/app/MCP task quiescence | 官方文档明确建议和 CancellationToken 组合：先 cancel，再 wait tracked tasks；替代散落 abort/detach |
| `futures-util` | 保留 | stream/future combinator | provider 归一化 stream 与有序并发结果 |
| Tower 0.5 | 定向试验 | provider request middleware；可能用于 handler 外层 | [`Service`](https://docs.rs/tower/latest/tower/trait.Service.html) 表达 Request -> Future<Response>，[`Layer`](https://docs.rs/tower/latest/tower/layer/trait.Layer.html) 装饰 service；适合 retry/timeout/telemetry/backpressure，不适合隐藏 session write-ahead 事务顺序 |
| `async-trait` | 暂时保留 | object-safe async ports | Rust 原生 async trait 的 dyn/object story 仍需按实际 toolchain 验证；不为“纯洁”一次改全部 |
| 自建全局 event bus | 拒绝 | — | 会掩盖 owner、顺序和背压；使用窄 typed sink/channel |

TaskTracker 依据：[官方 API](https://docs.rs/tokio-util/latest/tokio_util/task/task_tracker/struct.TaskTracker.html)。引入时写 `Supervisor` wrapper，禁止各模块直接发明不同 shutdown 顺序。

### 2.1 Tower spike 的通过标准

只选 provider path 做 spike：

1. retry 只在“未产生语义事件”前发生；
2. Retry-After 和总等待预算保持现有语义；
3. cancel 不被 timeout/retry layer 吞掉；
4. error 保持 Connection/Interrupted/Auth/RateLimited 分类；
5. 类型复杂度和 compile time 可接受；
6. layer 顺序有单元测试，因为 [`ServiceBuilder`](https://docs.rs/tower/latest/tower/struct.ServiceBuilder.html) 的 layer 顺序影响请求/响应路径。

不通过则保留显式 provider orchestrator。工具 pipeline 无论如何保持显式阶段，最多把无状态 handler 包装为 `Service<ToolInvocation>`。

## 3. TUI、布局、编辑与 Unicode

| 库 | 决策 | 解决的问题 | 注意事项 |
|---|---|---|---|
| Ratatui 0.30.x | 保留 | widgets、buffer、layout、backend | 当前 0.30.2；先使用内置 `Layout`、`Constraint`、`Flex`，不要另造通用布局引擎 |
| Crossterm + `EventStream` | 试验 | 用单一 async event loop 替代 blocking input thread | [`EventStream`](https://docs.rs/crossterm/latest/crossterm/event/struct.EventStream.html) 需 `event-stream` feature；Windows paste/IME/退出 parity 是硬门槛 |
| `unicode-width` | 保留 | terminal cell width | 只回答显示宽度，不能回答 grapheme cursor 或合法换行 |
| `unicode-segmentation` | 采用 | grapheme/word boundary | [实现 UAX #29](https://docs.rs/crate/unicode-segmentation/latest)；用于 editor cursor、delete、selection，不用 byte/char index 猜用户字符 |
| `unicode-linebreak` | 采用 | Unicode line break opportunities | [实现 UAX #14](https://docs.rs/unicode-linebreak/latest/unicode_linebreak/)；与 cell width 一起完成 CJK/标点 wrapping |
| `tui-textarea` | 试验 | 多行输入、undo/redo、search、selection | [0.7 文档](https://docs.rs/tui-textarea/latest/tui_textarea/) 显示功能成熟；只有通过 TPI IME、paste placeholder、history、keymap 和 Unicode parity 才替换自有 editor |
| `tui-scrollview` | 试验 | 通用 viewport/offset state | [官方文档](https://docs.rs/tui-scrollview/latest/tui_scrollview/)；buffer-based 内容可能不适合巨大 transcript，先和语义 anchor/缓存方案 benchmark |
| Taffy | 延后 | CSS flex/grid tree layout | [Taffy 文档](https://docs.rs/taffy/latest/taffy/)；只有插件定义嵌套响应式 UI 超过 Ratatui Layout 能力时再用，Phase 1–6 不加 |
| Ropey | 延后/默认拒绝 | 超大可编辑文本 rope | composer 有大小上限，transcript 主要只读；没有编辑性能 profile 不引入 |

Ratatui 0.30 的 [layout improvements](https://ratatui.rs/highlights/v030/) 和 [Flex 示例](https://ratatui.rs/examples/layout/flex/) 已足够表达多数 TPI shell/sidebar/composer 布局。

### 3.1 Unicode 算法合同

必须明确四种 index：

| 名称 | 单位 | 用途 |
|---|---|---|
| byte offset | UTF-8 bytes | 文件 IO、Rust string slicing（仅合法边界） |
| scalar index | Unicode scalar (`char`) | 少量语法处理，不等同用户字符 |
| grapheme index | extended grapheme cluster | editor cursor、backspace、selection |
| cell column | terminal display cells | layout、mouse hit test、hardware cursor |

换行算法：输入为 grapheme sequence + UAX #14 break opportunity + 每个 grapheme cell width；输出为 logical row mapping。禁止把 `chars().count()` 或 byte length 当 terminal width。

### 3.2 TUI 库 spike 不能遗漏的 cases

- ASCII/CJK/emoji/ZWJ/combining marks；
- CRLF/LF 与 multiline；
- Windows Terminal IME hardware cursor；
- bracketed paste 内含回车、tab、escape-like text；
- 10k/100k transcript lines；
- streaming append 时 history anchor；
- resize 160→40→160 columns；
- mouse cell 到 grapheme 的双向映射。

## 4. Schema、序列化和错误诊断

| 库 | 决策 | 用途 |
|---|---|---|
| Serde/serde_json | 保留 | durable codec、protocol、typed values |
| Schemars 1.x | 保留 | builtin input/output schema 生成 |
| `jsonschema` | 采用到外部边界 | MCP/Wasm/第三方工具 definition 和 invocation/result validation |
| `thiserror` | 保留 | domain/adapter typed errors |
| `miette` | 采用到 CLI/config | 带 code/help/source span 的用户诊断，不穿透核心所有层 |

[`jsonschema`](https://docs.rs/jsonschema/latest/jsonschema/) 支持复用 validator、meta-schema 检查和结构化 validation output。使用规则：

1. registration 时先验证 schema 自身；
2. 编译并缓存 validator，执行热路径不重复编译；
3. 调用前验证 input；
4. 外部工具完成后验证 canonical output；
5. validation error 有界输出，防止巨大恶意 schema/instance；
6. builtin Rust 类型仍以 serde deserialize 为权威，不做两套矛盾校验。

[`miette::Diagnostic`](https://docs.rs/miette/latest/miette/derive.Diagnostic.html) 只在需要人类修复的边界呈现；session 中存稳定 error code/category，不存 ANSI report。

### 4.1 Tracing、错误因果与可选导出

| 库/机制 | 决策 | 用途 | 边界 |
|---|---|---|---|
| `tracing` / `tracing-subscriber` | 保留并规范化 | 结构化 span/event、filter、subscriber composition | 语义事件目录和 typed IDs 由 TPI 定义；库不是业务事件模型 |
| `tracing-appender` | 保留但重写 owner | bounded non-blocking local writer/rotation | 明确 lossy/blocking 策略和 dropped counter；`WorkerGuard` 由 application lifecycle 持有并 flush，不再泄漏为 `'static` |
| `tracing-error` | 定向试验 | incident/fatal error 捕获 `SpanTrace` | 只在错误边界捕获，验证开销和脱敏；不能替代 typed error/source chain |
| `tracing-opentelemetry` + OpenTelemetry Rust | feature-gated 试验 | 用户显式启用的外部 trace/export adapter | 不进入 core contract；版本、语义稳定性、collector/backpressure/privacy 在引入时重新核验 |
| 自建同步逐 chunk JSONL writer | 拒绝 | — | 当前 provider trace 在 stream 热路径持 mutex、写入并 flush，既放大延迟又没有完整关联模型 |

异步 instrumentation 的硬规则：不能把 [`Span::enter`](https://docs.rs/tracing/latest/tracing/span/struct.EnteredSpan.html) 返回的 guard 跨 `.await` 持有；官方文档说明这会产生错误的 trace。异步 future 使用 [`Instrument::instrument`](https://docs.rs/tracing/latest/tracing/trait.Instrument.html) 或 `#[instrument]`，同步小区间才用 `in_scope`/enter。测试必须断言 parent/child 关系，而不只是“出现过日志”。

`tracing-appender` 的 non-blocking writer 使用固定队列并支持 lossy 行为；因此“不会拖慢主流程”不等于“不会丢”。TPI 的 Standard/Verbose/Forensic 每档要写明 queue size、溢出策略、`TraceGap`/dropped count、shutdown flush deadline。详细合同见 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)。

## 5. 存储与索引

| 方案 | 决策 | 用途 | 原因 |
|---|---|---|---|
| JSONL append-only | 保留为事实源 | session protocol | 可审计、可恢复、当前测试资产强 |
| SQLite + `rusqlite` | 条件采用 | rebuildable session catalog/search/index | 需要查询、排序、FTS、事务和成熟工具时更合适；永不成为 event truth |
| `redb` | 条件备选 | 纯 Rust embedded KV projection | [redb](https://docs.rs/crate/redb/latest) 提供 ACID KV；若需求只有 key/value 且明确拒绝 SQLite C 依赖才 spike |
| 自研 mmap/btree | 拒绝 | — | 没有必要承担 crash/corruption complexity |
| 把 session 主存储迁到数据库 | Phase 0–8 拒绝 | — | 风险高、没有当前收益证据、破坏可审计 JSONL |

SQLite 只在采样证明 session list/search 成本不可接受或明确需要全文检索时引入。派生 DB 必须带 projector version + watermark；删除数据库后能从 JSONL 全量重建。`rusqlite` 提供 SQLite hook/WAL API 的官方文档见 [hooks](https://docs.rs/rusqlite/latest/rusqlite/hooks/index.html)。

## 6. 搜索、diff、Markdown 和通用算法

### 6.1 保留当前成熟实现

- `ignore` + `grep` + `globset` + `regex`：继续作为 workspace 遍历/搜索基础；已经有 cursor、取消、binary、glob 和结果上限测试。
- `similar`：继续用于 unified diff；在大型文件上先 profile，再考虑 patience/histogram 等替代算法。
- `pulldown-cmark`：继续作为 Markdown parser；streaming 时按 entry revision 缓存 parse/layout。
- `syntect`/`two-face`：继续用于 syntax highlight；按 language/theme/content revision 缓存，不能每帧全量 parse。
- `blake3`：继续用于 revision/content fingerprint；不把 hash 当安全签名。

### 6.2 明确不提前引入

| 候选 | 当前结论 | 重新评估触发器 |
|---|---|---|
| `petgraph` | 延后 | workflow/subagent 真正需要动态 DAG、循环检查和拓扑调度 |
| LRU crate | 延后 | 当前 cache 行为不能用小型本地结构正确表达，且 profile 证实为热点 |
| fuzzy matcher | 延后 | session/tool/command palette 的数量导致子串/前缀匹配不可用 |
| rope/piece table | 延后 | composer 常见大小和编辑基准显示 Vec/String 为瓶颈 |
| parser combinator | 延后 | slash/config/protocol grammar 已复杂到手写解析反复出 bug |

算法选型必须从 invariants 和数据分布开始，而不是从 crate 名开始。例如子代理 workflow 若只是固定 fan-out + join，用 `FuturesUnordered`/semaphore 足够；只有用户定义依赖图出现时才需要 graph library。

## 7. 外部扩展协议

### 7.1 MCP

**采用并升级现有边界**。官方 [MCP Rust SDK](https://github.com/modelcontextprotocol/rust-sdk) 基于 Tokio，并提供协议实现。是否替换当前自有 MCP client，必须做 protocol/feature parity spike：初始化、capability negotiation、tools/list changes、取消、stderr、进程退出、schema limits、Windows stdio。

MCP 适合外部 tools/resources/prompts，不应承载 TPI 内部 session transaction。

### 7.2 ACP

**采用为外部 Agent 互操作候选**。[Agent Client Protocol](https://zed.dev/acp) 是面向 editor/client 与 coding agent 的开放协议。TPI 可在 Phase 8 后实现：

- ACP client：把 Codex/Claude/Pi 等外部 agent 当 subagent provider；
- ACP server：让其他 IDE surface 使用 TPI。

先选择一个方向，不同时实现。ACP transport 不能绕过 TPI 的 workspace/policy/budget 标记；外部 agent 若自行执行工具，UI 必须标注其安全边界。

### 7.3 Wasmtime Component Model

**延后到有真实第三方非 MCP 能力时试验**。[Wasmtime](https://docs.wasmtime.dev/) 支持 WebAssembly/WASI/Component Model、资源控制和可配置能力，适合作为跨语言受控扩展候选。但它增加体积、编译时间、WIT API 版本和 host capability 设计成本。

进入 spike 的前提：至少有两个需要本地计算/自定义 UI projection、又不适合 MCP 的第三方扩展。spike 必须验证 fuel/epoch interruption、memory cap、preopened dirs、network deny、component version handshake 和 Windows 分发。

### 7.4 Rust 动态库

**拒绝作为插件生态边界**。Rust Reference 明确说明 [Rust ABI 没有稳定保证](https://doc.rust-lang.org/reference/items/external-blocks.html)。即使使用额外 ABI 框架，async trait、panic、allocator、unload 和 toolchain version 仍扩大风险。可信内置能力静态链接；第三方走 MCP/ACP/Wasm/subprocess。

## 8. 安全与依赖治理

| 工具 | 决策 | 运行频率 |
|---|---|---|
| `cargo-deny` | 采用 | 每个 PR：advisories、licenses、bans、sources |
| `cargo machete` 或同类 unused-deps 检查 | 采用 | 每个 PR 或每周，人工确认删除 |
| `cargo tree -d` | 采用为审查报告 | 依赖 PR 必跑，不一定硬性禁止所有重复版本 |
| `cargo audit` | 可与 deny advisories 二选一 | 避免重复噪音；由 ADR 选择单 owner |
| 锁文件提交 | 保留 | application 必须固定解析版本 |

[cargo-deny 官方文档](https://embarkstudios.github.io/cargo-deny/)覆盖 advisories、license、crate bans 和 sources。策略必须维护例外到期时间，不能用 wildcard skip 清空警告。

当前专项：验证 `russh-keys` 是否真的未使用；若删除能合并重复 ssh-key/crypto 栈，运行 local/remote SSH 全套后再合并。

## 9. 测试和性能工具

| 工具 | 决策 | 说明 |
|---|---|---|
| cargo-nextest | 采用 CI runner | 进程级 test isolation、filter、timeout、slow test、retry 报告；doctest 仍单独 `cargo test --doc` |
| cargo-llvm-cov | 采用报告 | line/region coverage；critical modules 与 changed code 设门槛，不追求全仓虚高数字 |
| cargo-mutants | 定时采用 | session codec、state transition、policy、scroll math 的 mutation testing；不阻塞每个小 PR |
| Proptest | 保留并扩展 | event sequence、Unicode、path、scheduler ordering、projection invariants |
| Loom | 定向采用 | registry generation、shutdown、inbox claim 等小型同步模型；不运行整个 app |
| Insta | 定向采用 | TUI buffer、session envelope、诊断；snapshot 更新必须人工审阅 |
| Criterion | 采用 benchmark | context projection、Markdown layout、scroll wrapping、search、large session replay |
| Miri | 定时/变更触发 | 自有 unsafe 或复杂 FFI；当前无依据要求每 PR 全量 |
| cargo-semver-checks | 对外 crate 稳定后采用 | 只对承诺公共 SDK 的 crate，内部迁移阶段不背负虚假 semver |

资料：

- [cargo-nextest](https://nexte.st/) 支持 per-test isolation、filter、timeout、重试、记录/重放和 CI 分片；官方说明 doctest 需独立运行。
- [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) 包装 LLVM source-based coverage，支持 line/region、nextest 和阈值。
- [cargo-mutants](https://mutants.rs/) 用于验证测试是否真的能杀死逻辑变化。

## 10. 最终依赖原则

1. 新库优先解决边界问题，不用来掩盖所有权问题。
2. 同一领域只有一个主要 abstraction；不要同时保留两套 editor/scroll/runtime。
3. 高体积/高权限库（Wasmtime、SQLite、browser engine）默认 feature-off，且要有真实 consumer。
4. 直接依赖必须在源码中有 owner；仅因传递依赖“已经存在”不能直接使用未声明 API。
5. 升级库先看 release notes/MSRV/feature diff，再做 lockfile diff、`cargo tree -d`、平台测试。
6. 所有试验依赖在 spike 结束时只有两种结果：写 ADR 正式采用，或从 manifest 完全删除。
