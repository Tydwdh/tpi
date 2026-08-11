# 第四章审阅：《深入理解 AI Agent》× TPI

- **书本章节**：《深入理解 AI Agent》第 4 章「工具」（`ai-agent-book/book/chapter4.md`，782 行全文）
- **审阅对象**：TPI `src/tool/`（mod.rs / search.rs / files.rs / edit.rs / command.rs / web.rs / plan.rs / outcome.rs）、`src/agent/scheduler.rs`（waves 调度）
- **审阅日期**：2026-XX-XX（随当前 checkout 快照）
- **总体结论**：工具层是 TPI 最强的模块，与书中「工具设计通用原则 + 执行工具安全 + 调度/可观测性」高度对齐；**最大差距是提议者-审核者/Sidecar 第二视角审查完全缺失**（与第一章高风险操作确认同源），另有 ACI 描述改进空间（长输出截断与持久化已核实为完整实现）。

## 1. 对照总览

| # | 书中主题 | TPI 对应 | 符合度 |
|---|---|---|---|
| 1 | 五类工具（感知/执行/协作/事件触发/用户沟通） | 感知 ✅（read/list/search/glob/web）执行 ✅（edit/write/bash）；协作/事件触发/用户沟通 ❌ 无 | ⚠️ 覆盖 2/5 |
| 2 | 能力表达：专用 vs Skill+通用执行器 | 10 专用工具 + 静态分发（无 registry）；Skills 缺失 | ⚠️ 偏专用侧 |
| 3 | 工具粒度权衡 | 10 工具粒度合理（read/list/search/glob 分离）；无文档类整合需求 | ✅ |
| 4 | 通用性设计（通用优于专用） | bash 作为唯一执行器 = 通用工具范式 | ✅ 符合 |
| 5 | 工具描述艺术（何时用/边界/示例/代价） | 描述偏功能说明；边界部分有（SSRF/搜索预算）；**无调用示例、无执行代价说明** | ⚠️ 差距 |
| 6 | 参数传递保真性（防静默转换） | 无静默参数注入（bash 纯透传）；edit 显式 normalize_lf 并说明 | ✅ 符合 |
| 7 | MCP 协议 | 无 MCP（不依赖外部生态，合理） | — 合理缺席 |
| 8 | MCP 安全（工具描述投毒/遮蔽/凭证） | 无第三方工具注入面（内置工具自审计） | ✅ 天然免疫 |
| 9 | 感知工具：输出截断/分页/offset | search/list 分页 cursor ✅；read 截断显式标注 ✅ | ✅ 强 |
| 10 | 执行工具：输入验证/权限控制 | 参数校验 ✅ 路径边界 ✅（resolve_write_path/SSRF）**黑名单 ❌**（危险命令无检测） | ⚠️ 部分 |
| 11 | 提议者-审核者 / Sidecar | **无第二视角审查**（无独立模型审批/无 Sidecar） | ❌ 差距 |
| 12 | 自动验证闭环（write→linter） | 无 harness 级 linter 钩子 | ❌ 差距 |
| 13 | 长输出截断与持久化 | bash ArtifactWriter 落盘 + read @artifact 读取（第五章核实） | ✅ |
| 14 | 执行环境隔离/沙盒 | 进程级（本地用户权限），书中「本地开发用进程级即可」✅ | ✅ |
| 15 | 可观测性 | session 日志（耗时/参数/结果）+ RuntimeEvent 流（ToolStarted/Completed/OutputDelta）✅ | ✅ 强 |
| 16 | 幂等性/取消语义 | 取消 = cancel token（安全点语义）✅；**幂等性无**（write-ahead 恢复近似） | ⚠️ 部分 |
| 17 | 事件驱动异步架构 | 单会话同步 ReAct + cancel token；无事件队列/异步工具 | — 合理缺席（单用户终端） |
| 18 | 工具发现（上百工具/动态发现） | 10 工具平铺，无需动态发现 | — 合理缺席 |
| 19 | 工具分类路由（事件/紧急度） | 无 | — 合理缺席 |

## 2. 亮点（工具层最强模块）

1. **描述里含"何时用"与反例**：web_search「Results are for discovery only; never opens a browser and never calls a summary model」、bash「This is the only execution tool」——直接回应书中「让 LLM 知道什么时候用、做不到什么」。
2. **参数保真性**：bash 纯透传无静默注入；edit 的 `normalize_lf` 是**显式**规范化（书中：如需规范化必须在描述中说明），非静默转换。无 Cursor 弯引号类系统性偏差。
3. **感知工具输出控制（书中逐条命中）**：search/list 分页 + cursor（翻页不重扫，UUIDv7 稳定快照）；read 截断显式标注「已显示 X 行，共 Y 行」；search 报 scanned_files/scanned_bytes/stop_reason——静默截断被系统性避免。
4. **执行安全分层**：输入校验（schemars + ArgsError 反馈）→ 权限控制（路径边界 resolve_write_path、SSRF 拒绝私网/loopback、输入 16KB）→ 调度隔离（Write/WorkspaceUnknown 串行）。虽缺 Sidecar，但前两层扎实。
5. **可观测性**：session JSONL 记录每次调用参数/耗时/结果 + RuntimeEvent 实时流（ToolStarted/ToolOutputDelta/ToolCompleted 含 diff）——书中「日志/审计/性能」三要素齐备。
6. **取消语义 = 安全点**：cancel token 在工具边界/流边界检查，不在任意时刻强掐——符合书中「取消在安全点」的纪律。
7. **天然免疫 MCP 安全风险**：不接第三方工具，无工具描述投毒/遮蔽/凭证外泄面。

## 3. 差距与建议（按优先级）

### 3.1 提议者-审核者 / Sidecar 缺失（与第一章同源，最高优先）

- **书中要求**：不可逆/高风险操作事前审批（Proposer-Reviewer，独立模型、不同家族、能力相近）；或执行时 Sidecar 独立分类（轻量模型、只看结构化字段 `{tool, command}`，不看主模型自由文本）。
- **TPI 现状**：无第二视角。`bash("rm -rf ...")`、`write` 覆盖、批量删除等无任何审查；第一章已记录「高风险操作确认缺失」。
- **书中红线**：Sidecar 输入必须隔离主模型自由文本（否则提示注入话术可操纵审查）；拒绝熔断器（连续拒绝 → 回退人工）。
- **建议最小切入**：①规则级风险分级（危险命令模式检测：`rm -rf /`、`dd`、`curl | sh`、`git push --force` 等）→ 高风险 interactive 模式确认；②可选：轻量 LLM Sidecar 只读结构化字段。前者无需额外模型，先做。

### 3.2 工具描述改进（ACI 打磨，低成本高收益）

- **书中要求**：①「何时用」+ 边界反例（TPI 已有部分）；②**参数用具体例子**（RFC3339 带示例）；③**返回值描述**；④**执行代价**（"大型网站 5-10 秒"）；⑤每个工具 1-5 个调用示例（基准上准确率 72%→90%）。
- **TPI 现状**：description 是功能说明为主（"Read the contents of a file..."），参数 doc 注释简短（如 BashArgs 无示例），无执行代价标注。参数 schemars 只表达类型。
- **建议**：为高频易错参数补 doc 示例（如 search 的 regex 写法、bash 的 timeout_ms 语义、web_fetch 的 URL 格式）；描述里补「何时用/何时不用」；无需工具级示例存储，可在描述里内联 1 个典型用法。

### 3.3 自动验证闭环缺失

- **书中要求**：write/edit 后自动按文件类型跑 linter，返回结构化错误列表作为工具结果。
- **TPI 现状**：edit/write 返回 diff + revision，无语法校验；验证依赖模型自觉调 bash。第一章 3.2 已记录，此处补充工具层落点：可在 `write_tool_plan` 后置轻量校验（.rs 用 rustfmt --check / JSON 用 serde 解析）。
- **建议**：最小切入 = edit/write 对已知类型（.rs/.json/.toml）做廉价校验，失败作为模型可见的 `verification` 字段附加，不阻塞提交。

### 3.4 危险命令黑名单缺失

- **书中要求**：命令执行维护禁止命令黑名单（`rm -rf /`、`dd if=/dev/zero`）；黑名单只是基础层，需结合语义解析。
- **TPI 现状**：bash 无任何危险命令检测（注释「stderr 不等于失败」等规范有，但无拦截）。
- **建议**：与 3.1 的风险分级合并实现（规则层先做，语义解析后续）。

### 3.5 长输出截断持久化（✅ 已实现，第五章核实时修正）

- **书中要求**：输出超阈值 → 头 50 + 尾 50 行 + 中间提示 + 完整输出落盘可 read 读取。
- **TPI 现状（修正后）**：bash 每次执行创建 `ArtifactWriter` 将完整 stdout/stderr 落盘 `artifacts/`（`command.rs:117`），结果含 `@artifact/<session>/<id>` 引用；`read` 工具原生支持 `@artifact/` 引用读取完整输出（`files.rs:53`）；上下文 prune 保留 `artifact:` 关键行（P1-5），模型可随时 read 完整内容。链路完整，符合书中方案。
- **遗留（次要）**：上下文侧无自动「头 50 + 尾 50」摘要模板（prune 是 digest + 关键行 + tail 8 行），但 artifact 落盘已满足「完整输出可找回」的核心要求。

## 4. 反叙事设计备注

**MCP 缺席是刻意的**：书中引 Pi Coding Agent「核心刻意不内置 MCP，优先 CLI + Skills」——TPI 正是这一派（无 MCP、10 内置工具、无 Skills 加载）。对单用户本机 coding agent，内置工具自审计 > 第三方生态，安全面更小。合理，保持。

**工具粒度**：书中建议 >100 工具才需动态发现；TPI 10 工具平铺无压力，符合「先简单后复杂」。

## 5. 验证说明

- 依据：`book/chapter4.md` 全文 782 行；TPI `src/tool/mod.rs`（schema/description/parse_args）、`search.rs`（分页/预算）、`edit.rs`（normalize_lf/apply_edit）、`command.rs`（BashArgs）、`web.rs`（SSRF/引擎回退）、`scheduler.rs`（waves）。
- 未运行构建/测试（纯审阅）。
- 确认事实：description 无调用示例/代价（grep 证实）；bash 无危险命令检测（BashArgs 仅 command/cwd/timeout_ms）；无 linter 钩子；prune 后无 artifact 落盘（context/mod.rs 只裁剪文本）。

## 6. 下一章衔接

第五章（Coding Agent 与代码生成）：TPI 本身就是一个 Coding Agent——「七个核心工具」对照、代码作为元能力、文件系统作为 Agent 状态、自我验证/循环检测（TPI 的 ProgressTracker）、测试驱动。将正面审视 TPI 作为 Coding Agent 的完整度。
