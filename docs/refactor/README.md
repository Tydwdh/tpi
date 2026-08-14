# TPI 大范围重构总计划

> 状态：规划基线；只定义迁移方法和验收条件，不授权一次性重写。
>
> 调研基线：TPI `2202887355174aa49b4796b2d86178b9c1dff9ef`、DeepSeek Harness `47f943859bef60e4160492346772ded9b24f765a`、Pi `b1efcf7d7c5d7394fbb12ede0174e04d39ee7004`。
>
> 编写日期：2026-08-14。以后执行时必须记录新的基线提交，不能把本文对源码的判断当成永远正确。

## 1. 这套计划解决什么问题

TPI 已经拥有不少正确且难得的基础：append-only JSONL session 是事实源、context 是投影、写工具有 write-ahead/recovery、工具结果有结构化内部表示、远端 workspace 与本地 workspace 共享主要语义、TUI 有流式输出和语义滚动测试。问题不是“完全没有架构”，而是能力快速增加后，边界没有同步长出来：

- `app.rs` 同时拥有终端输入、事件循环、会话选择、命令路由、Agent 调用、UI 副作用和部分业务投影；
- Agent runtime 直接产生带有 target、diff、tail 等展示字段的 TUI 事件；
- 工具虽有统一 trait，但 builtin 与 external 的调度、恢复和后处理仍分叉，部分能力靠工具名触发；
- 进程级全局工具注册表使 session/agent 隔离、测试隔离、按作用域扩展都很困难；
- 任务创建、取消、等待和清理散落在多个模块，正常路径成熟，组合生命周期仍不够显式；
- 单 crate 与几个超大文件扩大了修改面，让低阶模型很容易跨越错误边界。

因此目标不是“换一套更高级的文件名”，而是逐步建立以下产品结构：

```text
                      tpi-cli（composition root）
                    /            |             \
           tpi-tui /         tpi-agent       adapters
                  /          /        \       /  |  \
              tpi-core  tpi-session  tpi-capabilities
```

核心策略是：

1. **保留 TPI 的正确不变量**：session truth、显式恢复、安全边界、Windows-first 行为。
2. **吸收 DeepSeek Harness 的可扩展性**：完整 capability seam、作用域注册、精确 disposer、setup/rollback、可观察生命周期。
3. **吸收 Pi 的核心理念**：核心小而稳定、默认工作流直接、Agent message 与模型 message 分离、扩展留在应用层、TUI 是可组合组件。
4. **拒绝两个极端**：不把一切做成动态插件；也不因“极简”删除 TPI 已有的权限、持久化、恢复和后台任务语义。
5. **采用绞杀式迁移**：每一阶段都保持主分支可运行、旧 session 可读、关键路径可回退；禁止 big-bang rewrite。

统治代码库的基本构件是：

```text
Capability = 静态扩展与组合单位
Event      = 已发生的动态事实
Projection = 对事实的确定性解释
Command    = 请求系统执行某事的意图
Effect     = 有 owner、取消和终态的外部变化
Plugin     = Capability 的装配/交付容器，而非计算模型
```

架构宪法：

> Everything extensible is a Capability. Every committed fact is an Event. Every read model is a Projection. Every external change is an owned Effect.

## 2. 成功标准

重构完成不以“新目录存在”判断，而以可测的系统性质判断。

### 2.1 架构标准

- 核心 Agent 不依赖 Ratatui、Crossterm、具体 provider、MCP client、SSH 或 Windows API。
- TUI 不反向依赖 `app`；composition root 是唯一知道全部实现的地方。
- 注册表没有进程级隐式全局可变状态。相同名称的旧 registration drop 不能删除新 registration。
- 工具 pipeline 不以 `if name == "..."` 决定计划、挂起、摘要、恢复或 workspace epoch。
- 每个异步任务都有 owner、取消源、终态和 join/flush 边界。
- 每种事实只有一个权威来源；UI、context、统计和索引都是可重建投影。
- 第三方扩展的稳定边界是协议或受控组件，不是 Rust `dylib` ABI。
- 任意 model request、tool call、session event和child run都能通过 typed ID追踪完整因果链。
- 实际发出的模型请求在 dispatch 前与 durable session reconstruction一致；不一致时禁止发送。
- trace明确披露 gaps、redaction、payload缺失和crash，不把 best-effort伪装成完整。

### 2.2 用户体验标准

- 输入后 100 ms 内产生本地视觉反馈；模型或工具慢不表现为“没收到”。
- 用户在历史区阅读时，新内容不会把视图强制拉到底部；回到底部后自动恢复 follow。
- popup/overlay 打开、关闭、取消和 resize 后焦点确定且可测试。
- 中文、emoji、组合字符、CRLF、宽字符在编辑、选择、换行和鼠标映射中使用正确边界。
- 子代理不会淹没主 transcript；用户能看到父子关系、运行状态、预算、结果和取消传播。

### 2.3 工程标准

- Windows、Linux 至少各有一个 CI job；Windows 远端 fixture 不再隐式要求 `cygpath`。
- `fmt`、`check`、`clippy -D warnings`、单元/集成/契约测试是合并门禁。
- session codec、恢复、工具 pipeline、取消、滚动和输入编辑具备属性/故障测试。
- 性能预算有基线，不用“感觉更快”验收。
- 每一迁移 PR 都有行为前后对照、回滚方法和临时兼容层删除条件。

## 3. 文档导航

按下面顺序阅读；低阶模型不得只读单个任务条目便开始改代码。

1. [00-current-state-audit.md](00-current-state-audit.md)：当前事实、基线、耦合和风险。
2. [01-reference-design-analysis.md](01-reference-design-analysis.md)：DeepSeek Harness / Pi 的采用、改造、拒绝清单。
3. [02-target-architecture.md](02-target-architecture.md)：目标模块、依赖方向、事件与生命周期契约。
4. [03-rust-technology-decisions.md](03-rust-technology-decisions.md)：Rust 库与协议的采用、试验、拒绝矩阵。
5. [04-tool-extension-and-security.md](04-tool-extension-and-security.md)：工具 registry、pipeline、权限和扩展边界。
6. [05-session-context-storage.md](05-session-context-storage.md)：session、投影、压缩、分支和派生索引。
7. [06-agent-runtime-and-subagents.md](06-agent-runtime-and-subagents.md)：run/turn/step、inbox、supervisor、子代理。
8. [07-tui-ux-rearchitecture.md](07-tui-ux-rearchitecture.md)：TUI 组件、焦点、布局、滚动、Unicode 和性能。
9. [08-migration-roadmap.md](08-migration-roadmap.md)：阶段、依赖、任务、验收、回退与发布。
10. [09-testing-observability-release.md](09-testing-observability-release.md)：质量门禁、测试矩阵、观测和发布策略。
11. [10-low-level-execution-playbook.md](10-low-level-execution-playbook.md)：给低阶模型的逐任务操作规程。
12. [11-evidence-and-source-index.md](11-evidence-and-source-index.md)：参考源码、官方资料、版本与重新核验规则。
13. [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)：全链路因果追踪、runtime invariant、资源树、incident与安全回放。

## 4. 不可违反的总原则

### 4.1 先建立特征测试，再移动所有权

移动模块本身不能证明行为不变。每个阶段必须先把现有行为写成黑盒或契约测试，然后只移动一个 owner。若一个 PR 同时改变模块边界、协议格式和业务语义，必须拆分。

### 4.2 先在单 crate 中证明依赖方向，再拆 Cargo workspace

直接拆七个 crate 会产生大量 `pub`、feature、错误类型和循环依赖噪音。先把目标边界作为普通模块建立，并用静态检查禁止反向引用。只有连续两个阶段未出现跨边界回流后，才物理拆 crate。

### 4.3 兼容层必须有删除期限

允许 facade、adapter 和双读，但每个兼容层必须写明：

- 它兼容哪两个版本；
- 谁仍在调用旧路径；
- 哪项测试证明可以删除；
- 最迟在哪个阶段删除。

禁止“先留着以后再说”。

### 4.4 扩展性不是任意 hook

只有存在至少一个 provider、一个 consumer、明确 contract、生命周期和测试套件时，才建立 capability seam。一个只有内部单一调用者的接口通常应保持私有闭包或普通方法。

### 4.5 安全与隔离不能由 prompt 保证

工具权限、路径范围、网络范围、进程环境、子代理预算和外部协议验证都属于运行时职责。模型提示只能解释能力，不能成为安全边界。

### 4.6 默认行为保持克制

目标内核可以支持 MCP、子代理、分支、Wasm 等，但默认配置不应一次启用全部能力。Pi 值得保留的核心是：默认面窄、常用操作短、复杂工作流可选择组合。

## 5. 项目治理

重构期新增 `docs/adr/`，至少落地以下 ADR：

| ADR | 决策 | 必须在何时批准 |
|---|---|---|
| ADR-001 | 极小内核与 dependency DAG | Phase 0 |
| ADR-002 | durable/live/UI 三类事件 | Phase 1 |
| ADR-003 | JSONL 继续作为事实源 | Phase 0 |
| ADR-004 | 作用域注册与精确 disposer | Phase 3 |
| ADR-005 | 工具 pipeline 的显式阶段 | Phase 3 |
| ADR-006 | task ownership、cancel、quiescence | Phase 2 |
| ADR-007 | 外部扩展使用 MCP/ACP/Wasm，不使用 Rust dylib | Phase 7 |
| ADR-008 | TUI component/focus/scroll contract | Phase 6 |
| ADR-009 | 子代理预算与工作区隔离 | Phase 8 |
| ADR-010 | Ledger/Trace/Payload/Metrics 四平面与追踪完整性 | Phase 0 |
| ADR-011 | 模型请求可重建、诊断脱敏与安全回放 | Phase 1–3 |

每个 ADR 必须包含 context、decision、alternatives、consequences、migration、rollback、evidence。ADR 只记录已经决定的方向，不替代对应测试。

## 6. 何时停止重构

满足以下条件后停止结构性重构，转为普通演进：

- 所有“成功标准”有自动证据或明确人工验收；
- 目标 dependency DAG 已稳定两个发布周期；
- 旧 facade、双写、旧事件 adapter 全部删除；
- P95/P99 延迟、内存和 session 恢复指标不劣于基线；
- 新增一个 provider、工具来源或 UI component 不需要修改核心 switch；
- 子代理是可选 capability，不影响单 Agent 默认路径；
- 没有为了达到文件大小目标而继续拆分高内聚模块。

重构结束后仍可能存在大文件或复杂算法；只要所有权清楚、测试充分且没有实际问题，不应继续为“纯洁度”重写。
