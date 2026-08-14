# 01. DeepSeek Harness 与 Pi：采用、改造、拒绝

## 1. 分析原则

参考项目不是规格。只迁移能解决 TPI 当前问题、能在 Rust/Windows/TUI 环境验证、且不会破坏现有 durable/security 特性的设计。

每一项使用三个结论：

- **采用**：问题和约束高度一致，可进入目标架构；
- **改造后采用**：原则正确，但实现机制或默认值不适合 TPI；
- **拒绝/延后**：没有当前 consumer、风险大于收益，或参考项目自身仍处 developer preview。

## 2. DeepSeek Harness

### 2.1 它真正强在哪里

DeepSeek Harness 的价值不是 TypeScript 包数量，而是把“能力可组合”落实到完整契约：

1. 一个 capability seam 同时有 service definition、provider、consumer；缺一不叫 seam。
2. registration 是 effect，register 返回 disposer，创建者拥有清理。
3. capability 可按 agent scope 可见；局部能力可 shadow root 能力。
4. Agent 在 setup window 完成能力组装，失败回滚，成功后才发布 handle。
5. session event、live agent event、capability event 分开，model-visible 内容必须可追溯。
6. Agent handle 明确拥有 send/followup/steer/inject/cancel/idle/dispose 生命周期。
7. inbox 区分 next-step 与 next-turn；claim、discard 等变化也是可观察事实。
8. tool pipeline 有 resolve、guard、execute、post-process、result 等阶段，且工具输出与 UI presentation 分离。
9. subagent 是 provider family，而不是硬编码一个递归函数；不同 provider 可共存。
10. 产品组合通过 bundle/profile 完成，核心 capability 不等同于默认开启。
11. 可调试性不是散落日志：durable session ledger、raw model chunks、live/replay telemetry、package-owned runtime invariants 和 effect/fiber ownership tree 互相校验。

这些原则可在本地以下来源复核：

- `C:\Users\tyd27\Desktop\deepseek-harness\AGENTS.md`
- `...\docs\architecture.md`
- `...\docs\agent-lifecycle.md`
- `...\packages\AGENTS.md`
- `...\packages\core\*`
- `...\packages\tools\*`
- `...\packages\subagent\*`

### 2.2 采用矩阵

| 设计 | 结论 | TPI 的实现方式 | 验证 |
|---|---|---|---|
| 完整 capability seam | 采用 | Rust trait/typed contract + provider + real consumer + conformance suite | 删除任一角色时测试/构建暴露孤立 seam |
| registration 是 effect | 采用 | `RegistrationId` + RAII handle + idempotent explicit dispose | old-handle ABA、double-drop、setup rollback 测试 |
| agent scoped capability | 改造后采用 | immutable root snapshot + session/agent overlay；避免任意树形 lookup | 两 Agent 同名工具隔离、显式 shadow 测试 |
| setup window | 采用 | `AgentBuilder::build()` 内注册；成功后返回 public handle；错误倒序 unwind | 任一 provider setup 失败后无任务/注册/session 泄漏 |
| durable/live/capability event 分层 | 采用 | durable `SessionEvent`、ephemeral `RuntimeEvent`、局部 lifecycle signal | 类型依赖和 replay 测试 |
| next-step / next-turn inbox | 改造后采用 | 对齐 TPI `AwaitingUserInput`、有界队列和用户可见丢弃政策 | claim 顺序、abort 恢复、重启 replay |
| 完整 tool pipeline | 采用 | pipeline 主干显式编码；只把可独立 middleware 化的层交给 Tower | 每阶段顺序、拒绝、取消、故障注入 |
| subagent provider family | 改造后采用 | 先 in-process isolated session，再 ACP/subprocess；默认关闭 | budget/cancel/workspace 冲突套件 |
| live + replay telemetry | 改造后采用 | 从同一 canonical event prefix 投影；本地 collector 接管队列、批处理、丢失计数和脱敏 | live/replay 等价、handoff 去重、sink 故障不阻塞 Agent |
| model request 可重建 | 采用 | durable request header + source event references；dispatch 前比较 frozen request manifest | history/header/tool schema/config 任一漂移立即报 invariant failure |
| raw model chunks | 分级采用 | Standard 只保留语义摘要；Verbose/Forensic 才写有界 payload sidecar，并显式标注完整性 | Unicode/SSE/tool-call merge、截断和 gap fixture |
| package-owned runtime invariants | 采用 | 每个 capability/runtime owner 随实现注册只读检查器，不建中央巨型 validator | 故意破坏 session/tool/inbox/stream/lifecycle 序列时定位到 owner |
| effect/fiber ownership tree | 改造后采用 | Rust `Supervisor`/registration/task/process 生成只读资源快照；不复制 Cordis runtime | shutdown 后空树、orphan/late event 检测 |
| generated producer/consumer catalog | 采用 | 从 typed event/trace registry 生成事件生产者、消费者、敏感级别和版本表 | CI 检测无 producer、无 consumer、未分类 payload |
| 配置 profile/bundle | 延后 | TPI 先用 typed resolved profiles；不引入 Cordis 风格 loader | 至少两个真实 product composition 后再评估 |
| “Everything is a plugin” | 拒绝 | 内核不插件化自己的不变量；只开放稳定 capability | dependency DAG gate |
| declaration merging 类型事件 | 拒绝 | Rust enum/trait/serde 版本协议更自然 | 编译期 exhaustiveness + codec fixtures |
| reactive/spatiotemporal service graph | 拒绝 | TPI 当前规模不需要通用依赖图/HMR | 若未来出现真实 live reconfiguration consumer 再 ADR |
| self-modifying harness | 拒绝 | 运行时不能加载/改写自己的可信核心 | 安全策略和打包签名 |

### 2.3 从 DeepSeek 吸收时最容易犯的错误

#### 错误 A：按能力创建几十个 crate

DeepSeek 的 npm package 组织服务于独立发布、loader 和 TypeScript composition。Rust crate 是编译、feature 和公开 API 边界。TPI 应先用模块证明依赖，再拆 6–7 个稳定 crate；小 consumer 可以留在 app crate。

#### 错误 B：建立通用 hook waterfall

任意 before/after hook 会让顺序、错误和取消变得不可推理。工具 transaction 顺序、session append 和权限检查应留在显式 orchestrator；只有独立、可交换、无隐藏状态的横切关注点适合 middleware。

#### 错误 C：把 disposer 当作“名字删除”

可逆 effect 的关键是撤销**自己产生的那一份 effect**，不是撤销“现在同名的东西”。必须用不可复用 identity。

#### 错误 D：把 scoped context 当 service locator

scope 不能让任意模块随时按类型查全局服务。核心 use case 仍通过构造参数接收窄接口；registry 只用于真实扩展集合。

#### 错误 E：把“全量追踪”理解为全量正文外发

DeepSeek 的 telemetry backend 明确要求异步入队，并允许由接收端按 `(session.id, event.seq)` 去重；OTel 适配器也警告未经规则处理的 prompt、工具参数/结果和文件内容可能离开本机。TPI 应吸收它的**因果完整性、重放和 invariant**，但默认只保留本地 Standard 元数据。正文进入单独 payload plane，Verbose/Forensic 必须显式开启、限时、限量、分级脱敏；远程导出还需第二次明确授权。

#### 错误 F：把 trace 当 session truth

Session ledger 决定恢复和模型可见事实；trace 解释一次执行为什么走到这里。trace 可以采样、截断或丢失，session commit 不得依赖 trace sink 成功。反过来，telemetry/replay 应读取 committed ledger，而不是另建一条会悄悄漂移的业务事实链。完整设计见 [12-debug-tracing-and-replay.md](12-debug-tracing-and-replay.md)。

## 3. Pi

### 3.1 核心理念

Pi 的 README 把产品定义为 minimal terminal coding harness，并明确“让 Pi 适应工作流，而不是让工作流适应 Pi”。默认只有 read/write/edit/bash 等少量核心工具；subagent、plan、权限弹窗、后台 bash 等属于扩展或选择性能力。

对 TPI 最重要的不是删功能，而是三条边界：

1. **Agent core 与 coding-agent application 分离**：消息转换、工具和 UI 组合属于应用。
2. **Agent message 与 LLM message 分离**：先 `transformContext`，再 `convertToLlm`，防止 provider 形态成为 session truth。
3. **TUI 是组件和 renderer**：组件按宽度渲染 lines，焦点和 overlay 有生命周期，布局与业务状态分开。

本地复核位置：

- `C:\Users\tyd27\Desktop\pi\README.md`
- `...\packages\agent\README.md`
- `...\packages\coding-agent\README.md`
- `...\packages\tui\README.md`
- `...\packages\coding-agent\examples\extensions\subagent\*`

### 3.2 采用矩阵

| 设计 | 结论 | TPI 的实现方式 | 验证 |
|---|---|---|---|
| 最小默认核心 | 采用 | 保留安全/恢复，但默认 capability roster 克制 | fresh config 的模型工具列表 snapshot |
| Agent message != LLM message | 采用 | durable/domain `Message` -> context policy -> provider `ModelMessage` | 同一 session 投影到两 provider；无 provider JSON 泄漏 |
| interactive/print/JSON/RPC/SDK 多 surface | 改造后采用 | 先 TUI + headless JSON；核心不要求 UI receiver | headless 不再需要 drain TUI channel |
| steering/followup 两队列 | 采用 | 与 DeepSeek inbox 合并设计，增加 durable receipt | 顺序、abort、queue full 测试 |
| JSONL session tree/branch | 延后采用 | 先稳定 event id/head projection，再加 parent/head | 老 session 兼容、分支/压缩属性测试 |
| source-order 持久化 | 采用 | 并行工具可完成乱序，session/model result 始终按 call index 提交 | randomized completion test |
| component TUI + focus | 采用 | Ratatui component contract、FocusStack、OverlayStack | recorded input + snapshot + cursor tests |
| differential render / synchronized output | 改造后采用 | 先测当前 Ratatui backend；必要时实现 renderer strategy | bytes/frame、flicker 手工矩阵 |
| 子代理使用独立进程与隔离上下文 | 改造后采用 | in-process provider 先实现同样隔离；外部 provider 用 ACP/subprocess | parent/child session、取消、输出 cap |
| 主进程 full-access，无权限机制 | 拒绝 | TPI 保留显式 ToolPolicy/workspace 限制 | security contract |
| subagent/plan 永不进核心 | 部分采用 | 不进入微内核；可作为官方 capability 随应用分发 | 默认关闭、单 Agent 路径无额外状态 |
| 所有扩展在进程内运行 | 拒绝作为生态边界 | 第三方优先 MCP/ACP/Wasm；内置信任扩展可静态链接 | protocol version/capability handshake |

### 3.3 Pi TUI 值得精确迁移的语义

- layout 使用 `basis/grow/shrink/min/max/visibility` 思维，而不是在 render 中散落减法；TPI 先用 Ratatui `Layout`/`Flex` 表达。
- overlay 拥有自己的 focus，并在关闭时恢复原 focus。
- main-screen 与 alt-screen 是 renderer 策略，不污染 widget 状态。
- editor 硬件光标位置是 IME contract，不是装饰。
- bracketed paste、autocomplete、history、semantic navigation 必须作为 editor 行为整体测试，不能只迁移文本 buffer。
- ScrollView 的 follow-end 与 history browsing 是显式状态；append 和 resize 不应偷偷改变用户锚点。
- 组件可缓存昂贵 render，但缓存 key 必须包含 width、content revision、theme/format options。

## 4. 综合目标：TPI 自己的路线

```text
DeepSeek Harness                         Pi
----------------------------------      -------------------------------
scoped capability + disposer       +    minimal core + application layer
setup/rollback/quiescence           +    agent message -> LLM projection
typed pipeline + provider families  +    direct terminal UX/components
durable model-visible facts         +    steering/followup + session tree
                 \                       /
                  \                     /
                   v                   v
        TPI: durable, secure, Windows-first microkernel
             + scoped capabilities
             + separately composed TUI/application
```

### 4.1 TPI 应有的“核心”

核心只拥有：

- stable IDs、domain message/content、run/turn/step vocabulary；
- durable session protocol 的领域类型；
- Agent state machine 和 inbox 语义；
- capability contracts 与 policy decision types；
- cancellation/terminal outcome 约束。

核心不拥有：

- Ratatui view state；
- OpenAI JSON/SSE；
- MCP transport；
- SSH/Windows 文件 API；
- built-in tool 的具体实现；
- slash command 和主题；
- 子代理某个具体 provider；
- SQLite/JSONL 的具体文件句柄。

### 4.2 TPI 应有的“扩展性”

扩展性用以下问题验收，而不是用插件数量验收：

1. 能否增加第二 provider，而不改 Agent loop 的 match？
2. 能否让某个 Agent 多一个工具，而不影响另一个 Agent？
3. 能否重载 MCP server，而旧 disposer 不会删除新工具？
4. 能否增加 headless JSON surface，而无需伪造 TUI consumer？
5. 能否增加一个子代理 provider，而不改变 parent loop？
6. 能否在工具执行前增加权限 policy，而不复制 write-ahead 顺序？
7. 能否重建所有 UI/context/index，而不修改 session truth？

若答案为是，即达到所需扩展性；无需追求“万物可热替换”。

## 5. 参考项目版本风险

- DeepSeek Harness 在其 README 中仍以 developer preview 定位。其设计可作为强证据，但不应把包划分、loader/HMR、动态 composition 当成熟稳定 API 直接复制。
- Pi 的“full access + 建议容器化”适合熟悉风险的个人 harness；TPI 已有更强恢复/安全合同，删除它们会是回退。
- 两个项目都在快速迭代。开始某个阶段时只复查与该阶段直接相关的文件，并记录新 commit；禁止无界追赶上游。

## 6. 每项借鉴的证据要求

执行者提出“参考 DSH/Pi”时，任务说明必须同时写：

```text
来源文件与 commit：
来源解决的问题：
TPI 当前相同问题的证据：
TPI 不同的约束：
采用/改造/拒绝：
最小实现：
验证方式：
```

缺少上述内容时，该参考不能作为重构理由。
