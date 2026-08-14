# 00. 当前状态审计与风险基线

## 1. 审计范围与方法

本次只建立模型和计划，不改业务代码。审计覆盖：

- 根 README、AGENTS、Cargo manifest、目录和核心入口；
- 用户输入到 TUI、Agent、provider、工具、workspace、session 的主路径；
- 状态拥有者、同步/异步边界、注册与清理、持久化与恢复；
- TUI reducer、编辑、滚动、流式输出和交互测试；
- DeepSeek Harness 与 Pi 的本地源码和文档；
- 当前 Rust 依赖图和官方候选库资料。

下文区分三种结论：

- **事实**：可从当前源码或命令复现；
- **风险**：结构已允许错误发生，但不声称现在必然触发；
- **提案**：目标设计，必须通过后续 spike/测试验证。

## 2. 当前产品模型

### 2.1 主数据流

```text
keyboard/mouse/terminal resize
        |
        v
app event pump ------ slash commands/session selection
        |                         |
        v                         v
TUI reducer/model <------ SessionLog / Conversation
        ^                         |
        |                         v
RuntimeEvent <---- Agent run -> context builder -> Provider stream
                                  |
                                  v
                         tool selection/scheduler
                                  |
                       ToolRuntime / ToolContext
                        /        |          \
                 local/remote  process      MCP
                        \        |          /
                         ToolOutcome -> session JSONL
```

### 2.2 正确且必须保留的不变量

1. `SessionLog` 是 durable truth；transcript、context、统计是投影。
2. assistant 完整消息提交与中断 attempt 是不同事实，不能把部分流伪装成完整消息。
3. tool call 有 requested/started/completed 协议；写工具在副作用前记录恢复信息。
4. `Conversation` 将 log 与缓存 history 作为一个状态单元，并在失败后从 log 刷新。
5. provider adapter 吸收 SSE、参数增量和 finish reason 差异，Agent 不应读取 provider JSON。
6. tool output 有模型 payload、session metadata、状态和 artifact 信息，不能退回纯字符串错误。
7. 后台进程具有有限 tail、明确终态和取消测试。
8. 编辑工具对 revision、CRLF、BOM、Unicode、备份和原子替换已有大量测试。
9. TUI 已有 follow/history、resize、选择、CJK cell width 和流式输出测试资产。
10. 配置是字段级合并并拒绝未知/非法值；重构后不能退回“最后一个 block 全覆盖”。

## 3. 规模与热点

审计时 Rust 源码与测试约 62,800 行。该数字只用于基线，不作为质量目标。高修改压力文件包括：

| 文件 | 约行数 | 当前职责 |
|---|---:|---|
| `src/app.rs` | 2,910 | app 启动、输入线程、事件泵、命令、session、Agent、UI effect |
| `src/tui/model.rs` | 2,211 | UI 状态、投影、选择/滚动相关状态 |
| `src/agent/mod.rs` | 1,756 | run/turn、provider stream、恢复、context、控制流 |
| `src/agent/tool_runtime.rs` | 1,246 | 工具 wave、write-ahead、执行、流转发、特殊工具后处理 |
| `src/tool/mod.rs` | 1,199 | builtin 类型、上下文、dispatch、schema、工具共用协议 |
| `src/session/mod.rs` | 1,488 | 事件、codec、log writer/reader、协议校验 |
| `src/tool/edit.rs` | 2,584 | 编辑算法、恢复素材与测试；大但高内聚程度高于 `app.rs` |
| `src/tui/mod.rs` | 3,261 | 渲染与主 TUI 组合 |
| `src/tui/tests.rs` | 3,261 | 渲染回归测试；与 `mod.rs` 行数相同只是巧合，哈希不同 |

不要把“文件大”等同于“必须拆”。优先处理跨层决策和时序耦合。`edit.rs` 的算法复杂度可能合理，`app.rs` 的多 owner 混合则是实质风险。

静态 import 扫描看到的主要依赖压力：

```text
app -> tui       62 references
app -> session   41
tui -> tool      59
agent -> tool    47
agent -> provider 27
agent -> context 16
agent -> session 12
mcp -> tool       8
agent -> tui      6
tui::reducer -> app 1 reverse edge
```

数字受导入写法影响，不作精确架构度量；它足以显示 `app`、`tool`、`tui` 是当前扇入/扇出热点。

## 4. 已确认的问题与根因

### High-1：应用层是时序耦合中心

- **位置**：`src/app.rs`。
- **表现**：终端输入、bracketed paste 处理、异步事件循环、session flow、slash command、鼠标命中、Agent 启动/取消和 UI effect 同处一个 owner。
- **触发条件**：任何涉及“输入 + session + runtime + UI”的新功能都要修改同一文件，并理解多个异步状态。
- **根因**：composition root、application use case、platform input adapter 和 presentation controller 没有分开。
- **影响**：局部修改容易改变无关时序；低阶模型无法建立有限上下文；测试倾向依赖内部函数。
- **目标修复**：先提取 command/effect 协议和 `AppController`，再提取 terminal adapter；`main` 只组装依赖。
- **验证**：`app` integration tests 只通过 fake runtime/session/UI port；TUI reducer 不引用 app；输入 adapter 可独立 replay recorded events。

### High-2：核心 runtime event 携带展示决策

- **位置**：`src/agent/mod.rs::RuntimeEvent`。
- **表现**：工具事件带 `target`、`command`、`tail`、`diff` 等 TUI 需要的字段。
- **根因**：live domain event 和 UI view event 是同一个枚举。
- **影响**：headless/RPC/未来 GUI 也被迫消费 TUI 表示；工具展示变化需要改 Agent；测试不能区分业务事实与格式化。
- **目标修复**：建立 UI-agnostic `RuntimeEvent`，由 app projection 生成 `UiEvent/ViewEvent`。
- **验证**：`tpi-agent` 的依赖树无 Ratatui/TUI；同一 recorded runtime stream 可投影为 TUI、JSON 和测试 trace。

### High-3：所谓统一工具接口并不真正统一

- **位置**：`src/tool/registry.rs`、`src/agent/tool_runtime.rs`。
- **表现**：`BuiltinToolAdapter` 额外暴露 `execute_with_plan`；scheduler 使用 `PreparedKind::Builtin/External` 分支；external 默认缺少 workspace effect/recovery 语义。
- **根因**：`Tool` contract 只描述 schema 和 execute，没有把执行模式、访问足迹、effect/recovery、输出协议纳入 definition/pipeline。
- **影响**：MCP/未来扩展工具不能获得 builtin 等价的安全、调度和恢复；新能力继续在 runtime 加分支。
- **目标修复**：把 definition、handler、policy metadata、typed result 分开，由单一 resolver 和显式 pipeline 执行。
- **验证**：builtin/MCP/fake extension 运行同一 conformance suite；scheduler 不匹配来源类型。

### High-4：工具名承担隐藏协议

- **位置**：`src/agent/tool_runtime.rs`。
- **表现**：`web_fetch` 触发模型摘要，`update_plan` 触发 durable plan event，`edit|write` bump workspace epoch，request input 触发 suspend。
- **根因**：工具的业务效果没有 typed directive/effect report；pipeline 只能从字符串名称猜测语义。
- **影响**：工具重命名、override 或外部实现会静默失去行为；扩展性表面统一、实质封闭。
- **目标修复**：工具返回 canonical result 与可枚举 `ControlDirective`/`EffectReport`；计划、交互和内容后处理由显式 capability consumer 拥有。
- **验证**：重命名 fake tool 不影响语义；源码 gate 禁止 runtime 出现已登记的特殊工具名判断。

### High-5：全局 mutable registry 破坏作用域

- **位置**：`src/tool/registry.rs::global_registry`，由 Agent、MCP manager、doctor 共享。
- **表现**：`OnceLock<Arc<Mutex<ToolRegistry>>>` 跨 session/测试存在；无法明确表达 root/session/agent scope。
- **根因**：注册表被当作进程 service locator，而非 composition root 持有的依赖。
- **影响**：并行测试和多 Agent 能互相看到注册；权限、工具集合和生命周期难以隔离。
- **目标修复**：composition root 创建 root registry snapshot；每个 session/agent 使用 overlay；通过构造参数注入。
- **验证**：两个 Agent 注册同名局部工具互不影响；drop 一个 Agent 后 root 和另一个 Agent 不变。

### High-6：registration 存在名字级 ABA 删除风险

- **位置**：`ToolRegistry::register_owned` / `ToolRegistration::unregister`。
- **触发序列**：A 以名称 `x` 注册并取得 handle；B 覆盖名称 `x`；A handle drop；当前代码按名称删除，于是把 B 删除。
- **根因**：registration 身份只有名称，没有 slot generation/token。
- **影响**：MCP 重连、override、reload 或并发 scope teardown 可意外移除新工具。
- **最小可靠修复方向**：registry entry 带不可复用 `RegistrationId`；disposer 只在 `(name,id)` 同时匹配时删除。重复注册默认拒绝，显式 override 才替换。
- **验证**：精确复现上述序列的失败测试；并发/属性测试验证旧 disposer 永不删除新 entry。

### Medium-1：TUI 到 app 的反向依赖形成层次环

- **位置**：`src/tui/reducer.rs` 引用 `crate::app::preview_lines_to_body`。
- **根因**：会话预览格式化没有明确 presentation owner。
- **影响**：TUI 不能作为独立 crate；app 与 TUI 互相知道实现细节。
- **修复方向**：将纯 projection 移入 TUI presentation 或 app-view-model 模块，调用者依赖它而非反向。

### Medium-2：provider 接口偏向单实现和 channel

- **位置**：`src/provider/mod.rs::Provider`。
- **事实**：返回 `impl Future`，事件通过调用者提供的 mpsc sender；注释明确在第二个真实 adapter 出现时再提取边界。
- **风险**：运行时 provider registry、每个子代理不同模型、headless stream composition 会需要泛型扩散或额外 task/channel。
- **修复方向**：第二 provider 或子代理落地前，使用 object-safe adapter 返回规范化 `Stream`/response handle；请求 middleware 可独立组合。
- **验证**：OpenAI-compatible 与 fake/第二 provider 运行统一 protocol suite；取消前/中/后语义一致。

### Medium-3：异步任务所有权不统一

- **位置**：Agent watchdog、tool stream forwarder、app drain task、MCP reader、managed processes 等。
- **事实**：已有 `AbortTaskOnDrop` 和 `CancellationToken`，说明局部代码理解 detach 风险；但没有进程/agent 级 supervisor。
- **具体风险**：tool stream forwarder 执行后直接 `abort()` 未 join；非交互 run 为避免 UI channel 堵塞而额外启动 drain task，说明 core 与 presentation channel 耦合。
- **修复方向**：分层 `CancellationToken` + `TaskTracker`；owner 的 `shutdown()` 必须 close input、cancel、await tasks、flush durable data，且幂等。

### Medium-4：workspace transport 关注点泄漏

- **位置**：`ActiveWorkspace`、tool dispatch、remote executor/files/process。
- **风险**：新增 workspace 类型或隔离方式时，多个工具和 scheduler 同时分支。
- **修复方向**：抽取当前真实消费者需要的窄 ports：file ops、command ops、process ops、identity；不要先造通用 VFS。

### Medium-5：配置解析正确但 resolved config 过宽

- **位置**：`src/config.rs` 及调用方。
- **事实**：字段级 merge、未知字段拒绝是优点。
- **风险**：单个大 resolved config 在各层透传，使每个组件读到不属于自己的设置。
- **修复方向**：保留一个显式 resolver 和 provenance，将输出拆为 `AgentConfig`、`ToolPolicy`、`UiConfig`、`StorageConfig`、`ProviderConfig`；不建立到处读取的全局 config service。

### Medium-6：跨平台测试 fixture 隐含外部工具

- **位置**：`tests/fixtures/remote_server.rs:410`。
- **表现**：Windows 全量测试因找不到 `cygpath` 失败。
- **根因**：fixture 将 Git/Cygwin 工具当作未声明前提，且多组 remote 测试共用它。
- **影响**：开发者无法区分代码回归与环境缺失；CI 可移植性差。
- **修复方向**：用 Rust/标准库完成已知 Windows 路径转换，或启动 fixture 时做能力探测并明确 skip 原因；不能把增大 timeout 当修复。

### Medium-7：当前异步 span 会产生错误父子关系

- **位置**：`src/agent/mod.rs` 的 `agent.run` span。
- **表现**：`let _enter = span.enter()` 的 guard 覆盖后续大量 `.await`。
- **根因**：同步 thread-local enter/exit scope 被当成 async task scope；future yield 后，同一 executor thread上的其他 task可能被错误归入该 span。
- **影响**：并发 run/tool/provider 日志可能串到错误父 span，正好破坏用户希望依赖的因果调试证据。
- **修复方向**：使用 `Future::instrument`、`#[instrument]` 或显式 parent；只对不含 await 的同步片段使用 `in_scope`。
- **验证**：并发两个带不同 RunId 的 future，subscriber记录的 event ancestry不能交叉；官方 [`Span::enter` async warning](https://docs.rs/tracing/latest/tracing/span/struct.EnteredSpan.html)作为 contract依据。

### Medium-8：Provider trace 是孤立、同步、进程级旁路

- **位置**：`src/provider/trace.rs` 与 `src/provider/openai_compat.rs`。
- **表现**：`TPI_TRACE_PROVIDER` 通过 `OnceLock<Option<Mutex<File>>>` 决定一次；记录缺少 session/run/request/attempt关联；stream路径每条记录同步 `write_all + flush`；body模式可能保存完整用户代码。
- **根因**：provider trace 作为临时局部日志实现，没有接入系统 identity、owner、backpressure、retention和sensitivity模型。
- **影响**：并发请求难以区分；慢盘可能放大流延迟；运行时无法安全启停；文件增长、完整性和隐私状态不可查询。
- **修复方向**：迁入注入式 `TraceSink/TraceContext`，使用有界 writer、gap counter、typed sensitivity和per-run模式；环境变量只作短期配置adapter。
- **验证**：两并发run完全关联；slow/disk-full sink不改变provider结果；trace manifest披露drop；secret seed扫描为零。

### Low-1：直接依赖疑似冗余并造成重复加密栈

- **位置**：`Cargo.toml` 的 `russh-keys`。
- **事实**：源码扫描未找到直接使用；`cargo tree -d` 显示它与 `russh` 当前栈带来多个 ssh-key/crypto 版本。
- **修复方向**：Phase 0 用 `cargo machete` 加人工确认，若确实未使用则删除并运行全部 SSH 测试。不能只凭扫描删除。

### Ergonomics-1：输入线程和 UI event loop 分裂

- **位置**：`src/app.rs` 的 blocking crossterm read OS thread 与 async pump。
- **风险**：shutdown、paste 状态、resize 和键盘事件存在两套生命周期；TUI 退出时难证明线程安静结束。
- **提案**：对 Crossterm `EventStream` 做行为等价 spike。只有 Windows IME、bracketed paste placeholder、key repeat 和退出测试全部通过才替换。

### Ergonomics-2：pending message 是单一有界队列

- **事实**：上限 16，满时丢最旧并可见提示。
- **风险**：运行中的“立即影响下一步”与“本轮结束后再执行”没有语义区分。
- **提案**：借鉴 Pi/DeepSeek 的 next-step steering 与 next-turn followup，但必须有 receipt、持久化和取消恢复语义。

## 5. 当前验证基线

在 Windows PowerShell、当前用户工作树上执行：

| 检查 | 结果 | 说明 |
|---|---|---|
| `cargo fmt --all -- --check` | PASS | 当前用户未提交修改也满足格式 |
| `cargo check --all-targets --all-features` | PASS | 所有 target 构建检查通过 |
| `cargo clippy --all-targets --all-features -- -D warnings -D clippy::undocumented_unsafe_blocks` | PASS | 无 Clippy error |
| `cargo test --all-targets --all-features --no-fail-fast` | FAIL | 5 个 integration targets、24 个 cases 失败：`agent_remote` 2、`remote_bash` 5、`remote_contract` 6、`remote_files` 6、`remote_traverse` 5；全部在同一 fixture 因找不到 `cygpath` 失败。其余 target 继续执行并通过 |
| 人工 TUI 验证 | 未执行 | 本轮目标是计划，且工作树含用户修改，不应混入交互修复 |

首次普通全量运行会在第一个失败 target 后停止；本次又用 `--no-fail-fast` 枚举了全部 target，证明这是一个共享 fixture 前提导致的 24 个失败，而不是五种独立 production regression。链接器输出“正在创建库 …”被 Rust 1.97 作为 `linker_messages` warning 展示，但当前 Clippy gate 未把这些外部 linker stdout 当代码 lint 失败。Phase 0 应记录并决定 CI 是否降噪，不应 suppress 真实 linker diagnostics。

## 6. 工作树保护

审计时以下文件已有用户修改，计划文档没有触碰它们：

- `src/agent/tool_runtime.rs`
- `src/app.rs`
- `src/tui/model.rs`
- `src/tui/model/model_tests.rs`
- `src/tui/reducer.rs`
- `src/tui/tests.rs`

开始任何 Phase 前必须重新执行 `git status --short`，把未知修改视为用户资产。迁移任务若需要触碰同一文件，先基于新提交重做审计，不能覆盖或回滚这些变化。

## 7. 风险排序

| 排名 | 风险 | 概率 | 影响 | 首个控制阶段 |
|---:|---|---|---|---|
| 1 | app/runtime/TUI 同时拆导致行为漂移 | 高 | 高 | Phase 0-2 特征测试 + 单 owner 迁移 |
| 2 | session schema/恢复回归造成历史丢失 | 中 | 极高 | Phase 0 golden corpus，Phase 2 双读 |
| 3 | 工具统一化削弱 edit/write crash safety | 中 | 极高 | Phase 3 conformance + crash matrix |
| 4 | registry reload/override ABA | 中 | 高 | Phase 3 tokenized registration |
| 5 | 子代理放大并发、成本和文件冲突 | 高 | 高 | Phase 8 严格预算，默认关闭 |
| 6 | TUI 组件化破坏 follow/IME/paste | 高 | 中高 | Phase 6 recorded event parity |
| 7 | 过早拆 crate 导致 pub API 和循环依赖泛滥 | 高 | 中 | 先单 crate 模块边界 |
| 8 | 新库堆积、重复依赖和编译体积上升 | 中 | 中 | 依赖引入 gate + `cargo tree -d` |
| 9 | 动态插件 ABI/不可信代码进入进程 | 中 | 高 | 禁止 dylib；协议/沙箱边界 |
| 10 | 迁移周期过长，兼容层永久化 | 高 | 中 | 每阶段删除条件和 WIP 上限 |

## 8. Phase 0 必须重新确认的未知项

这些不是当前事实，不允许执行者擅自假定：

- 用户真实 session 文件的最大尺寸、历史版本分布和损坏率；
- 10 小时运行时的内存曲线、stream repaint 频率和 CPU 热点；
- Windows Terminal、ConPTY、SSH 服务端和 Git Bash 的实际组合；
- MCP server 数量、同名工具冲突是否已发生；
- `allow_outside_workspace = true` 的用户依赖程度；
- 用户是否需要主屏 scrollback、alternate screen 或两者可切换；
- 子代理主要用途是并行调查、隔离上下文、专长代理还是外部 agent 互操作；
- 是否真的需要 session 全文检索，从而决定 SQLite 派生索引是否有价值。

Phase 0 通过采样、匿名指标或用户指定 fixtures 回答；回答前不采购相应复杂度。
