# 第一章审阅：《深入理解 AI Agent》× TPI

- **书本章节**：《深入理解 AI Agent》第 1 章「Agent 基础知识」（`ai-agent-book/book/chapter1.md`）
- **审阅对象**：TPI（`src/agent/`、`src/context/`、`src/tool/`、`src/provider/`、`src/config.rs`）
- **审阅日期**：2026-XX-XX（随当前 checkout 快照）
- **总体结论**：TPI 是书中「自主 Agent（ReAct 循环）」范式的忠实实现，Harness 五要素中上下文/工具/约束/纠正工程化程度高；主要差距在**人工干预（高风险操作确认）**与**自动验证**两处，另有一处与书的叙事相反的设计（`ask_user` 被主动移除）。

## 1. 对照总览

| # | 书中主题 | TPI 对应 | 符合度 |
|---|---|---|---|
| 1 | Agent = LLM + 上下文 + 工具 | provider / build_context / 10 内置工具 | ✅ 完全符合 |
| 2 | 上下文 = 静态前缀 + 轨迹 | `build_context()`：system + 消息投影；tools 独立字段 | ✅ 符合，且更强 |
| 3 | 工具调用四步流程 | provider 流式 tool_calls → 校验 → waves 调度 → 按原 index 回填 | ✅ 符合，且落地并行 |
| 4 | 消融实验 1-1：缺工具结果 → 无限循环 | `ProgressTracker`（ActionKey+ObservationKey+StateStamp 判重） | ✅ 有专门防护 |
| 5 | Harness 五要素 | context / tools / 约束 / 验证 / 纠正 | ⚠️ 验证偏弱 |
| 6 | 自主 Agent 停止条件 | max_turns / max_tool_calls / wall-clock watchdog | ✅ 符合，且更细 |
| 7 | 护栏三层（输入/执行/输出） | 执行侧强；输入侧仅长度+校验；输出侧缺失 | ⚠️ 部分符合 |
| 8 | 人工干预（失败阈值/高风险操作） | 有「优雅移交」；无高风险操作确认 | ❌ 差距 |
| 9 | 编排模式：工作流 vs 自主 | 纯自主 Agent，无工作流层 | ✅ 符合（个人场景合理） |
| 10 | 模型选型 | provider/reasoning/context_window/单价可配 | ✅ 部分（无多模态） |
| 11 | Agent 学习三层 | 上下文适应 + artifact 更新；无参数更新（合理缺席） | ✅ 部分 |

## 2. 亮点（值得保留的设计）

1. **事实源与投影分离**：session 事件日志是持久事实源，发给模型的是投影（`context/mod.rs` §15.1）——比书中「轨迹不断追加」更进一步，是可恢复性的基础。
2. **请求级 token 预算**：`estimate_request` 把 system prompt + 计划快照 + 工具 schema 计入估算，避免「只算 messages → provider length error」（P0-9 修正记录）。
3. **无进展检测带状态戳**：StateStamp 用快照 revision，文件变化后允许重试（`bump_workspace_epoch`），不会误伤「修改后再试」的合法模式——正是书中思考题 4 的正面答案。
4. **waves 调度**：Pure/Read 并行（受 max_parallel_tools 限制）、Write/WorkspaceUnknown 按源顺序串行，结果按原 call index 回填。
5. **停止语义干净**：finish=stop 且无 tool call 立即完成，绝不追加第二次模型请求；取消来源区分用户 vs 超时。
6. **流恢复分级**：text-only 断联 → 续写 1 次；partial tool-call 断联 → 整轮重生成 1 次；上限防无限重试。
7. **write-ahead 恢复**：写工具预检生成一次 CommitPlan，持久化与执行复用同一 plan，崩溃恢复判定一致。

## 3. 差距与建议（按优先级）

### 3.1 高风险操作确认缺失（主要差距）

- **书中要求**：人工干预是生产护栏核心——超过失败阈值、高风险（敏感/不可逆）操作应触发人工监督；Claude Code 对照是「每个工具默认需授权」。
- **TPI 现状**：`tool/mod.rs` 顶部注释明确「`ask_user` 不是工具：需要用户决定时模型直接输出问题并结束当前 run」；`interactive` 字段标注「§11 移除 ask_user 后仅保留供未来交互原语使用」。`bash` 执行 `rm -rf`、`git push`、批量删除等不可逆操作无确认检查点。
- **评估**：对用户本机 workspace 的个人 coding agent 风险可控，但这是与第一章 Harness 主线最明显的偏离。
- **建议最小切入**：给 `bash`/`write` 增加风险分级（如检测危险命令模式/删除路径），interactive 模式下触发确认；不破坏 `-p` 非交互路径。

### 3.2 harness 级自动验证缺失

- **书中要求**：验证 = 自动判断操作结果对错（Linter/类型系统/工具调用结果校验）。
- **TPI 现状**：有参数预检、无进展检测；但编辑后无自动语法检查、测试失败无自动反馈钩子，验证依赖模型自觉调 bash 跑测试。
- **建议**：最小切入是为 edit/write 增加可选的后置校验（如 `.rs` 语法、JSON 合法性），或将验证规则写入 system prompt 与 eval harness 对齐。

### 3.3 输入/输出侧护栏空白

- 输入侧仅有长度限制（16KB）与 schema 校验；输出侧无 PII 过滤/输出验证。
- 个人场景风险较低，可接受；留待第二章（提示注入）与第四章（工具权限）展开时再评。

## 4. 反叙事设计备注

`ask_user` 被主动移除（§11 移除），改为「模型输出问题并结束 run，用户下轮回复」。这与书中「用户沟通工具 / 人工干预」的叙事相反，但有合理性：在单终端交互场景，结束 run 等用户输入与「调用 ask_user 工具阻塞等待」在体验上等价且实现更简单。保留此设计，但应在文档中记录其与书中模型（人工干预为第一公民）的差异。

## 5. 验证说明

- 依据：`book/chapter1.md`（539 行全文）对照 `src/agent/mod.rs`、`scheduler.rs`、`tool_runtime.rs`、`limits.rs`、`context/mod.rs`、`tool/mod.rs`、`system_prompt.md`、`config.rs`。
- 未运行构建/测试（纯审阅）；审阅时工作区有 5 个未提交改动（集中在 tui 层），不影响本章结论。
- 配置默认值：max_model_turns=80、max_tool_calls=160、max_wall_time_minutes=45、max_parallel_tools=4（`config.rs`）。

## 6. 下一章衔接

第二章（上下文工程）是全书最关键章节，TPI 的 `context/mod.rs`（预算/裁剪/压缩）+ `provider/openai_compat.rs`（消息结构/SSE）会有大量可对照点，重点：KV Cache、提示注入攻防、Agent Skills、Agent 状态栏、上下文压缩。
