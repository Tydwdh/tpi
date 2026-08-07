# Phase H：高级能力评估（TPI_STABILIZATION_TASK §38）

> 结论日期：2026-08（核心稳定化 Phase A–G 完成、§43 真实验收通过后）。
> 评估框架（任务书 §38）：每项能力必须回答——
> 1. 它解决了什么已经观察到的问题？
> 2. 不用它为什么不行？
> 3. 它会增加什么复杂度？
> 4. 怎么 benchmark？
> 5. 怎么回滚？
>
> 判定标准（任务书 §46）：Does this make the core loop more correct? /
> make failures more observable? / improve real coding task success? /
> reduce unnecessary model/harness complexity? 全否 → 不做。

---

## 结论总览

| 能力 | 结论 | 触发条件 |
|---|---|---|
| Skills framework | **不做** | 出现"同一提示反复手工粘贴"的可测量痛点 |
| MCP | **不做** | 出现真实的外部工具集成需求 |
| 插件 ABI | **不做** | 同 MCP |
| subagent | **暂缓** | 单一 agent 在真实任务上被证实有不可接受的 token/延迟瓶颈 |
| multi-agent / agent team | **不做** | 同 subagent |
| ACP | **不做**（任务书 §45） | §45 的 6 个进入条件全部成立 |
| client/server 大改 | **不做** | 出现真实的多进程/远程使用需求 |
| 跨会话长期 memory | **不做** | 出现"同一事实反复向模型解释"的可测量痛点 |

---

## 1. Skills framework

- **解决什么问题**：目前没有观察到的问题。agent 的 system prompt 已注入 AGENTS.md 项目规则（P1-12）；重复性的任务模板没有真实用户反馈支撑。
- **不用为什么不行**：没有证据表明当前路径（AGENTS.md + 直接指令）失败。
- **增加什么复杂度**：新文件格式、加载/校验、版本管理、与 session 事实模型交互——每一层都可能在核心稳定前引入新状态。
- **怎么 benchmark**：task success 率 + tokens/task 对比（有 baseline 后）。
- **怎么回滚**：删除 skills 目录与加载代码即可，无 schema 迁移。

## 2. MCP / 3. 插件 ABI

- **解决什么问题**：TPI 目前没有真实的外部工具集成需求；web_search/web_fetch 已覆盖网络访问。
- **不用为什么不行**：无。
- **增加什么复杂度**：动态 registry、进程生命周期管理、协议安全边界、工具 schema 动态化——直接对抗"静态分发 + 确定性"的现有设计（§8.1）。
- **怎么 benchmark**：真实任务需要的外部工具数量。
- **怎么回滚**：无历史包袱可回滚（未引入）。

## 4. subagent

- **解决什么问题**：长任务 token 成本/延迟。**当前未观察到瓶颈**：§43 真实验收中 deepseek-flash 完成完整 read→edit→verify 闭环仅 10+ 次请求，成本可忽略。
- **不用为什么不行**：单 agent 循环在 80 turn 预算内可完成典型任务。
- **增加什么复杂度**：子上下文管理、结果合并、事实源分裂（子 agent 的观察如何回到 session 事实模型）——与"Session 是唯一事实源"（§18.4）直接冲突。
- **怎么 benchmark**：同一任务单 agent vs subagent 的 task success + tokens + 延迟。
- **怎么回滚**：subagent 不写 session 事实（只返回文本）时可无损回滚；否则复杂。
- **暂缓理由**：需要先观测到单 agent 的真实失败模式。

## 5. multi-agent / agent team

- 同 subagent，且复杂度更高（协调协议）。**不做**。

## 6. ACP

- 任务书 §45 明确：不实现。进入条件（全部成立才重新评估）：
  - TPI 可以稳定完成真实 coding task ✅（§43 已验收）
  - 普通 compaction 有 benchmark ❌（未做）
  - session replay 完全可靠 ✅（§21 矩阵）
  - 已观察到明确的长 context 痛点 ❌
  - 能测量 task success 与 token cost ❌
  - 有 baseline 可做 A/B ❌
- **结论：条件 2/4/5/6 未满足，不做。**

## 7. client/server 大改

- 无真实的多进程/远程使用需求（个人终端工具）。**不做**。

## 8. 跨会话长期 memory

- 无"同一事实反复解释"的痛点证据；AGENTS.md 已是轻量项目记忆。**不做**。

---

## 后续条件触发清单（出现任一 → 重新评估对应项）

1. 用户在两个以上工作区重复粘贴同一段指令 → Skills
2. 需要集成工作区外的真实工具/服务 → MCP
3. 单一 agent 任务 token 成本或 wall time 成为实际阻塞 → subagent
4. §45 六条件全部成立 → ACP
5. 需要从另一台机器/进程使用 TPI → client/server
