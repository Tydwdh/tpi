# 第七章审阅：《深入理解 AI Agent》× TPI

- **书本章节**：《深入理解 AI Agent》第 7 章「模型后训练」（`ai-agent-book/book/chapter7.md`，813 行全文）
- **审阅对象**：TPI 全仓库（确认无训练组件）
- **审阅日期**：2026-XX-XX（随当前 checkout 快照）
- **总体结论**：TPI 是纯推理应用（无任何训练/微调组件），本章是**合理缺席**——书中决策框架第一条就是「先问：需要后训练吗？如果通过 Harness 工程就能解决，不需要训练模型。大多数 Agent 应用落在这里」。TPI 正是纯 Harness/prompt 路线。值得记录的三处间接关联：eval 验证器可作 RLVR 奖励函数（桥梁未接）、Harness 护栏与 RLVP「奖励结果、约束过程」目标一致、六/七章「bad case→训练」链路中 TPI 只走到归因信号采集。

## 1. 对照总览

| # | 书中主题 | TPI 对应 | 符合度 |
|---|---|---|---|
| 1 | 预训练/SFT/RL 三阶段 | 无（推理应用） | — 合理缺席 |
| 2 | 决策框架「先问需要后训练吗」 | TPI 落「Harness 工程解决」分支 | ✅ 符合书中建议 |
| 3 | SFT/RL 训练组件 | Cargo.toml 无训练依赖（无 torch/verl 等） | — 合理缺席 |
| 4 | 评估环境→训练桥梁（RLVR） | eval `VerifyStep` 本质是可验证奖励函数；**无 RL 训练栈接入** | ⚠️ 桥梁未接 |
| 5 | RLVP「奖励结果、约束过程」 | Harness 侧 ProgressTracker（无进展惩罚）+ 路径边界（硬约束） | ✅ 目标一致（外部约束版） |
| 6 | 工具轨迹 loss mask（环境 token 不参与梯度） | 无训练，无直接对应 | — |
| 7 | bad case → 后训练链路（归因→数据→训练→双集验证） | 只有归因信号（edit_failures/repeated 等）；无训练 | ⚠️ 走一半 |
| 8 | 蒸馏（On-Policy Distillation/OPSD） | 无 | — 合理缺席 |
| 9 | 与 RAG/ICL 协同（事实→RAG/ICL，策略→参数） | TPI 全部用 ICL/harness 表达 | ✅ 符合「大多数应用」 |
| 10 | 后训练写入参数 vs Harness 护栏 | TPI 全走 Harness 护栏 | ✅ 书中明示等价目标 |

## 2. 亮点与间接关联

1. **evaluation 验证器天然可作 RLVR 奖励**：`VerifyStep`（Bash 断言/FileExists/FileContains）与书中「用测试用例、数据库断言、状态差异判断结果」的 RLVR 定义同构——若未来接入训练栈（verl 等），TPI 的 eval 环境稍加改造即可提供奖励函数（书中：「判分脚本直接就是奖励脚本」）。
2. **ProgressTracker = Harness 侧的「约束过程」**：书中 RLVP「奖励结果、惩罚路径」针对可训练模型；对已有模型，「Harness 护栏是外部约束，过程惩罚是内部内化——两者目标一致」。TPI 的 repeated_without_progress 拒绝 + 路径边界正是外部约束版的过程约束。
3. **「过早结束」问题的 Harness 侧已有雏形**：书中 Coding Agent 最常见失败之一是「测试没跑就宣称完成」；TPI 的 eval 用验收断言独立验证（模型声明不算数），system_prompt 也有「没有证据时不得声称已修复」——是「判定交给模型写不了的验证」的评估侧实现。
4. **格式稳定优先**：书中「结构化输出不稳定时先 SFT 稳定格式」——TPI 用 schemars schema + ArgsError 期望 shape 反馈在 Harness 层实现同等的「格式稳定」目标。

## 3. 差距与建议

### 3.1 训练栈缺失（合理缺席，无需补）

- TPI 定位是个人 coding agent 应用，训练投入产出比低；书中明确「大多数 Agent 应用不需要训练」。**不建议**为 TPI 引入训练组件。
- 若未来要自训练（如专属小模型），最小路径是：eval 验证器 → RLVR 奖励 + 轨迹前缀回归 → DPO 偏好对（书中表7-4 映射），配合外部训练框架（verl/unsloth），不改 TPI 核心。

### 3.2 「bad case→数据」链路可补评估侧一环

- **书中要求**：失败归因记录（首个错误步骤/类别/证据）是后训练与回归测试的共同基础。
- **TPI 现状**：eval 记录 edit_failures/repeated_actions 等聚合计数，但**无结构化失败归因输出**（哪一步是首个错误、哪个工具、什么证据）。第六章 3.3 已列，此处从「训练数据来源」角度再强调：没有归因，bad case 无法沉淀为回归任务或偏好对。
- **建议**：与第六章 3.3 合并——eval 结果增加失败摘要字段（首次 failed edit 的路径/错误、首次 stale、repeated 触发点）。

### 3.3 无「从轨迹学知识」机制（衔接第八章）

- 书中第八章把「从运行轨迹获得学习信号、更新知识/指令/程序/参数」作为进化主线；TPI 目前无轨迹→更新的自动机制（plan 由模型手工 update，AGENTS.md 由用户维护）。
- 属第八章主题，详见下一章审阅。

## 4. 反叙事设计备注

无（本章对 TPI 无叙事冲突——TPI 明确不参与训练，与书中「大多数应用不需要训练」一致）。

## 5. 验证说明

- 依据：`book/chapter7.md` 全文 813 行（含「何时选择 SFT 何时 RL」「RL 环境」「奖励设计」「蒸馏」「bad case 到后训练」「实践要点」各节）；TPI `Cargo.toml`（无训练依赖）、`src/eval/mod.rs`（VerifyStep 定义）。
- 未运行构建/测试（纯审阅）。
- 确认事实：TPI 无 torch/verl/unsloth 等训练依赖；无模型权重操作代码。

## 6. 下一章衔接

第八章（Agent 的持续进化）：TPI 将实质对照——学习信号（结果/过程/LLM Rubric）、更新载体（知识文档 / Prompt与Skills / 程序与Harness / 模型参数）、验证/灰度/回滚。TPI 的 session/plan/AGENTS.md 对应「程序与 Harness」载体，进化链路未闭合（无自动沉淀）。
