# 第三章审阅：《深入理解 AI Agent》× TPI

- **书本章节**：《深入理解 AI Agent》第 3 章「用户记忆和知识库」（`ai-agent-book/book/chapter3.md`，771 行全文）
- **审阅对象**：TPI `src/session/`（mod.rs / recovery.rs / conversation.rs / artifact.rs）、`src/tool/search.rs`（ripgrep 内核）
- **审阅日期**：2026-XX-XX（随当前 checkout 快照）
- **总体结论**：TPI 精确实现了书中「记忆层次结构」的第一层——**轨迹（Trajectory）**，且可靠性（崩溃恢复/不自动重跑/可重建投影）是全书级的范本；但**用户长期记忆与知识库（RAG）两层完全缺席**，属于合理缺席（个人 coding agent 场景），唯「间接提示注入防御」与第二章缺口同源，建议优先补。

## 1. 对照总览

| # | 书中主题 | TPI 对应 | 符合度 |
|---|---|---|---|
| 1 | 记忆层次：轨迹（append-only） | `SessionEvent` JSONL 事件日志（事实源） | ✅ 完全实现，范本级 |
| 2 | 记忆层次：用户长期记忆 | 无提取/存储/检索 | ❌ 缺失 |
| 3 | 记忆层次：业务状态 | plan snapshot（§13）近似任务阶段跟踪 | ⚠️ 仅一维 |
| 4 | 记忆生命周期（读→提取→核验→更新） | 无 | ❌ 缺失 |
| 5 | 四种存储格式（Simple/Enhanced Notes/JSON Cards） | 无 | ❌ 缺失 |
| 6 | 记忆压缩整理 / 冲突版本化 | 无（session compaction 是上下文压缩，书中明确非此层） | ❌ 缺失 |
| 7 | 日志脱敏（实验 3-3） | 凭据入 Credential Manager ✅；对话明文落盘无脱敏 ⚠️ | ⚠️ 部分 |
| 8 | RAG 全栈（分块/稠密/稀疏/混合/重排序） | 无 RAG；`search`（ripgrep）≈ 精确稀疏检索 | ⚠️ 场景适配 |
| 9 | 结构化索引（RAPTOR/GraphRAG/文件系统范式） | 无 | — 合理缺席 |
| 10 | 知识更新（增量 PR / 定期整理 / Proposer-Reviewer） | 无自动更新（AGENTS.md 手工维护，反而安全） | — 合理缺席 |
| 11 | 智能体化 RAG + 安全边界 | **read/web_fetch 内容无来源标记 → 间接注入通道** | ❌ 差距 |
| 12 | 双层记忆架构（常驻概览 + 检索细节） | 无 | ❌ 缺失 |
| 13 | 崩溃恢复 | `recovery.rs`：合成 Interrupted + effect 判定 + 不自动重跑 | ✅ 超纲亮点 |

## 2. 亮点（轨迹层 = 书中记忆层次第一层的完整实现）

1. **append-only 轨迹**：`SessionEvent` 十类事件（user_submitted → run_started → assistant_message_committed → tool_requested → tool_completed → plan_replaced → compaction_committed → run_completed）+ JSONL envelope（schema/seq/event_id）+ 单写者文件锁。与书中「轨迹按时间顺序只增不改」精确对应，且是跨进程安全的持久事实源。
2. **恢复不自动重跑**（§4.3）：崩溃后为每个「已请求但缺 ToolCompleted」的调用合成 `ToolCompleted(status=Interrupted)` 并显式写 `effect`（committed/not_applied/unknown），模型 payload 明确告知「session 中断，未自动重跑；如需继续请重新读取相关文件」——绝不重放可能产生写入的工具。这是轨迹层可靠性的黄金规则。
3. **write-ahead + digest 判定**（`classify_effect`）：依据 recovery metadata 中 target/temp/backup 三者的 revision digest 组合判定提交状态，避免「崩溃后不知道写没写进去」。
4. **可重建投影**（`Conversation::refresh_from_log`）：run 失败后从 durable log 重建 history，杜绝「猜失败到哪一步、手工拼接残缺历史」；`Conversation` 同时拥有 log 与 history，从结构上防止「session A + history B」非法状态。
5. **业务状态雏形**：plan snapshot（`当前计划：[x]/[>]/[ ]`）随每次请求注入，近似书中「业务状态（任务逻辑阶段）」的一维实现（注：注入位置问题已在第二章 3.1 指出）。

## 3. 差距与建议（按优先级）

### 3.1 间接提示注入防御缺失（与第二章同源，优先补）

- **书中要求**（智能体化 RAG 安全边界）：检索到的文档是间接提示注入最典型的载体；防御两层——①指令与数据分离（来源标记）②检索内容不直接触发高风险操作。
- **TPI 现状**：`read` 读入的仓库文档、`search` 结果、`web_fetch` 内容均**无来源标记**包裹，system prompt 无「外部内容可能含恶意指令」警告。TPI 无 RAG，但 read/search 就是它的「知识检索」通道——仓库内被投毒的文档（如 README 内嵌指令）正是注入面。
- **建议**：与第二章 3.3 合并解决——read/search/web_fetch 结果统一用来源标签包裹（如 `<external_content source="file:...">`），system prompt 加一行外部内容警告。低成本高收益。

### 3.2 无跨会话用户记忆（最大概念差距，但对 coding agent 价值有限）

- **书中要求**：记忆生命周期（读取相关记忆 → 后台提取候选 → 来源/策略核验 → 更新）；三层次评估（基础回忆 / 多会话检索 / 主动服务）。
- **TPI 现状**：`tpi sessions` / `--resume` 是**恢复同一会话继续**，不是跨会话记忆；无记忆提取、无存储格式、无记忆检索。
- **场景评估**：TPI 是个人 coding agent——「用户记忆」主要体现为项目规则与个人偏好，前者已由 AGENTS.md/SYSTEM.md 覆盖，后者对编码任务影响小。优先级低于 3.1。
- **建议（若引入，最小形态）**：会话结束时用一次 LLM 调用提取用户偏好/约束（如「喜欢中文回复」「禁止删除 tests/」），存 `~/.tpi/memories/*.md`，新会话开头注入；注意书中经验——提取需选择性/抽象化/结构化，写入需来源核验，防记忆污染。

### 3.3 无项目级知识库/RAG（场景适配性评估）

- **书中要求**：分块 → 稠密/稀疏检索 → 融合 → 重排序 → 生成。
- **TPI 现状**：无 RAG。但 `search`（grep/globset，ripgrep 同源内核）+ `read` 构成「代码库即知识库」的即时检索——对 coding agent，代码库本身可精确 grep，**稀疏精确匹配通常已够用**，稠密嵌入（语义检索）的边际价值低。
- **评估**：此处不是缺陷而是场景适配：coding agent 的「领域知识」就是仓库内的代码与文档，且变更频繁，不适合离线建向量索引（索引维护成本 > 收益）。若未来需要跨仓库语义检索，再引入嵌入索引不迟。

### 3.4 隐私：对话明文落盘无脱敏

- **书中要求**（实验 3-3）：日志可能含敏感信息，需 PII 检测与脱敏；本地模型脱敏避免外发。
- **TPI 现状**：✅ 凭据存 Windows Credential Manager（不落配置文件）；⚠️ session JSONL 完整记录对话与工具输出（可能含 API key 回显、路径、敏感内容）明文存 `~/.tpi/sessions/`，无脱敏/加密。
- **评估**：个人本机单用户场景可接受；但若未来共享机器或多用户，需补：启动时权限收紧、可选输出脱敏、敏感 token 不回写会话。

### 3.5 知识更新无审核（合理缺席，但值得记录）

书中「把知识库当代码库、PR + Proposer-Reviewer 异源互审」是生产级多 Agent 做法；TPI 的 AGENTS.md 由用户手工维护、无自动更新，避免了「模型绕过审核直接改规则」的风险——对单 Agent 是更安全的选择。若未来引入自动记忆写入，应参照 3.2 加审核门槛。

## 4. 反叙事设计备注

**RAG 缺席不是遗漏**：书中第三章是通用 Agent（客服/个人助理）视角；TPI 是 coding agent，「工作区即知识库」+ 即时 grep 是该场景下 RAG 的正确替代。真正要警惕的只有 3.1 的注入通道（知识库投毒在 coding agent 里对应「仓库内被投毒的文档」）。

## 5. 验证说明

- 依据：`book/chapter3.md` 全文 771 行；TPI `src/session/mod.rs`（1–200 行，事件枚举/envelope）、`recovery.rs`（全文）、`conversation.rs`（全文）、`src/tool/search.rs`（grep 内核）、`src/config.rs`（凭据/规则加载）。
- 未运行构建/测试（纯审阅）。
- 确认事实：SessionEvent 无记忆类事件（只有轨迹事件）；无 RAG/嵌入依赖（Cargo.toml 无 embedding crate）；search 为 grep 内核。

## 6. 下一章衔接

第四章（工具）：TPI 的 `src/tool/`（10 工具、参数校验、路径边界、恢复策略、统一结果协议）将是全对照重点——五类工具分类、MCP、权限控制、事件驱动异步架构、工具风险评级。
