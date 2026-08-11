# 第六章审阅：《深入理解 AI Agent》× TPI

- **书本章节**：《深入理解 AI Agent》第 6 章「Agent 的评估」（`ai-agent-book/book/chapter6.md`，874 行全文）
- **审阅对象**：TPI `src/eval/mod.rs`（1163 行）、`scripts/gen_evals.py`、`scripts/check_evals_baseline.py`
- **审阅日期**：2026-XX-XX（随当前 checkout 快照）
- **总体结论**：TPI 的 eval harness 实现了书中「工具调用型评估环境（SandboxEnv）+ 确定性验证器」的完整骨架，且**过程指标采集是亮点**（路径效率/行动合法率/重复检测全有）；主要缺口在**LLM-as-a-Judge/Rubric、统计显著性、多随机种子、失败归因自动化、内部评估基础设施（消融/AB/特性开关）**——对个人 coding agent 场景，多数属「按需补建」而非缺陷。

## 1. 对照总览

| # | 书中主题 | TPI 对应 | 符合度 |
|---|---|---|---|
| 1 | 评估五要素（数据集/状态/工具/Rubric/协议） | task.md + expected.toml + repo（数据集）✅ reset（状态可重置）✅ 确定性验证（Rubric 简化）⚠️ | ⚠️ 部分 |
| 2 | 工具调用型环境（SandboxEnv：有状态+隔离） | reset repo → 独立 session → agent::run（对应 Verifiers SandboxEnv） | ✅ |
| 3 | 确定性验证（GAIA/SWE-bench 风格） | VerifyStep：Bash 断言 / FileExists / FileContains | ✅ |
| 4 | 评估循环（reset→run→snapshot→score） | `run_task` 完全对应 | ✅ |
| 5 | 过程指标（行动合法率/工具调用正确率/路径效率） | **turns/tool_calls/repeated_actions/edit_failures/stale_failures/first_edit_time_ms** | ✅ 亮点 |
| 6 | 重复调用指纹（无进展检测） | `repeated_actions`（检测 repeated_without_progress） | ✅ |
| 7 | 成本与延迟 | input/output tokens + wall_time_ms ✅；无价格计算 ⚠️ | ⚠️ |
| 8 | Pass@1 / Pass@k / Pass^k | 单次单跑 = Pass@1；无 k 次采样 | ❌ |
| 9 | 多随机种子 | 无（每次单跑） | ❌ |
| 10 | LLM-as-a-Judge + Rubric 四准则 | 无（纯确定性验证） | ❌ |
| 11 | 一票否决项（幻觉/安全） | 无 LLM 评判，无 veto 维度 | ❌ |
| 12 | 失败归因（首个错误/结构化） | 无 LLM 归因；但 edit_failures/repeated_actions 等信号可辅助 | ⚠️ |
| 13 | 轨迹前缀回归任务 | 无（只有端到端） | ❌ |
| 14 | 统计显著性（配对/McNemar/bootstrap） | 无 | ❌ |
| 15 | 数据泄漏防范 | repo 固定 base_commit（快照隔离）✅；无 canary/动态参数 | ⚠️ |
| 16 | 可观测性（追踪/span） | session JSONL 全量事件 + EvalResult 指标 | ✅ 强 |
| 17 | 内部评估（消融/AB/特性开关/提示词敏感性） | 无 | ❌ |
| 18 | 模型选型（多模型对比） | 无（单模型配置） | ❌ |
| 19 | 评估→训练桥梁（仿真环境/RLVR） | 无（合理缺席，第七章主题） | — |
| 20 | 成本监控/预算 | max_turns/max_tool_calls/walltime ✅；eval timeout_sec ✅ | ✅ |

## 2. 亮点

1. **评估循环与书中骨架逐行对应**：`reset_repo`（git reset --hard + clean -fdx，base_commit 可固定）→ 独立 session（不污染用户会话，`~/.tpi/evals/sessions/`）→ `agent::run(task.md)` → 事件统计 → 验收断言 → JSON 结果。正是书中「reset、完整轨迹、最终状态缺一不可」的落地。
2. **过程指标采集丰富**：turns / tool_calls / 各工具调用数 / repeated_actions（无进展）/ edit_failures（含 stale 失败）/ first_edit_time_ms（首改时间，对应书中「路径效率-回退次数」）/ input-output tokens / compaction_count——评估不止看「成没成」，还能看「怎么走」。
3. **确定性验证器**：bash 断言（exit code + stdout/stderr contains）+ 文件存在/内容检查，全部机械化可复现，对应书中 GAIA 字符串匹配、SWE-bench 可执行验证路线——对 coding task 是可靠且零成本的对错标准。
4. **结果持久化分层**：`<task-id>.json`（最近一次）+ `runs.jsonl`（历史累计）——为后续配对分析/趋势追踪留了数据基础。
5. **安全边界**：eval 任务目录/组件拒绝符号链接与 reparse point；verify 路径防 `..` 逃逸；repo 必须位于任务目录内；symlink 逃逸检查（resolve_verify_path）——评测本身也遵守路径边界。
6. **超时与取消纪律**：timeout 后显式 cancel token + grace period，防止 provider task 残留；verify 步骤 60s 上限。

## 3. 差距与建议（按优先级）

### 3.1 LLM-as-a-Judge / Rubric 缺失（评分维度单薄）

- **书中要求**：开放式任务用 Rubric（四准则：专家指导/全面覆盖/权重/veto）由 LLM 评判；确定性检查与模型评判分层——veto 先由环境真值或规则否决，LLM 只评难形式化维度。
- **TPI 现状**：只有确定性验证（通过/失败二元）。对 coding task 够用，但对「答案质量」「是否按规范」等开放维度无评判。
- **建议**：在 expected.toml 扩展可选 `[[rubric]]`（dimensions + veto），eval 跑完后若配置了 rubric 则调用 judge LLM 评分；veto 维度（如幻觉/越权）先由确定性检查触发。这是从「能测」到「能评开放任务」的关键一步。

### 3.2 统计显著性与多随机种子缺失（结论可信度）

- **书中要求**：配对分析（McNemar/配对 bootstrap）、每配置 3–5 随机种子、标准误与置信区间；分差须超噪声才切换。
- **TPI 现状**：每次单跑单配置；runs.jsonl 有历史但无聚合分析。
- **建议**：`tpi eval` 增加 `--seeds N`（同任务多跑）与 `--compare A B`（配对 delta + 简易显著性报告，如 McNemar 或 bootstrap）。数据已在 runs.jsonl，缺的是聚合层。

### 3.3 失败归因与轨迹前缀回归缺失（从「失败」到「修复」断链）

- **书中要求**：失败归因（首个错误类别+证据，结构化 JSON/YAML）；端到端回归 + 轨迹前缀回归（冻结前缀，只测决策边界，允许动作集合）。
- **TPI 现状**：能记录 edit_failures/repeated_actions 等信号，但无归因输出、无前缀回归任务类型。
- **建议**：①eval 结果增加失败摘要字段（首次 failed edit 的路径/错误、首次 stale、repeated 触发点）——数据已在 session 里，只需提取；②expected.toml 支持 `prefix` 模式（给定消息前缀冻结起点）成本高，先做 ①。

### 3.4 Pass@k 与成本计算缺失

- **书中要求**：Pass@1/Pass@k/Pass^k 区分；成本=单价×token（区分输入/输出/缓存）。
- **TPI 现状**：单跑一次（Pass@1 近似）；tokens 有记录但无价格换算（config 有 price_input/price_output，未用于 eval）。
- **建议**：低成本——结果渲染时用 config 单价算成本；`--seeds N` 实现后自然得到 Pass@k。

### 3.5 内部评估基础设施缺失（消融/AB/特性开关/提示词敏感性）

- **书中要求**：每个特性可独立关闭（消融开关，启动路径极早期注入）；AB 测试（多臂/机制vs目标/护栏指标）；编译时+运行时双层特性开关；提示词确定性渲染+版本化+回归。
- **TPI 现状**：无。`config.rs` 有若干开关（allow_outside_workspace、interactive、force_compaction）但无系统性消融层。
- **评估**：对个人项目过重；但「提示词版本化快照 + 变更跑回归」一条值得做——AGENTS.md/system_prompt 变更是行为风险点。建议：`tpi eval` 记录 config 快照（model/prompt 版本）进结果，便于追溯「哪个 commit 改提示词、对评估集影响」。

### 3.6 数据泄漏防范可增强

- **书中要求**：canary GUID、动态参数生成、时间新鲜度。
- **TPI 现状**：repo 固定 commit 快照（防「任务被训练集收录后记忆化」的缓解有限——若模型见过该 repo 的 issue 修复，可能记忆答案）。
- **评估**：个人 coding agent 的 evals 基于真实仓库，泄漏风险本就存在；可参考 SWE-bench Verified 思路——任务从「真实 issue」改为「验证过的、带独立测试的」变体。优先级低。

## 4. 反叙事设计备注

**纯确定性验证 vs LLM-as-a-Judge**：TPI 选择确定性验证器，对 coding task 是正确的默认（书中也强调「有明确正确答案的任务用二元判定足够」）。LLM 评判应作为扩展而非替换——优先级 3.1 已列。

**单跑 vs 多跑**：书中「单次运行只能用来筛选方向」——TPI eval 定位为「快速筛选方向」合理；但若要支持「评估驱动选型」（模型切换决策），3.2 的多种子+显著性必须补。

## 5. 验证说明

- 依据：`book/chapter6.md` 全文 874 行；TPI `src/eval/mod.rs`（1–800 行实读，discover/reset_repo/run_task/stats/verify/persist）、`scripts/gen_evals.py`（README 提及）、`scripts/check_evals_baseline.py`（README 提及）。
- 未运行构建/测试（纯审阅）。
- 确认事实：无 judge/Rubric 字段（VerifyStep 仅 Bash/FileExists/FileContains）；无 seeds/compare 参数；EvalResult 有 tokens 无 cost；无 prefix 回归任务类型。
- 注意：eval 调用真实 provider 花钱（README 明示），本审阅未运行任何 eval。

## 6. 下一章衔接

第七章（模型后训练）：TPI 是纯推理应用，无训练/微调组件；将评估其合理缺席，并对照书中「评估环境→仿真环境→训练」的桥梁（TPI 的 eval 环境理论上可做 RLVR 奖励函数，但无训练栈）。
