# P9 证据门控能力——延后 ADR（2026-08-14）

roadmap §12 明确："这些任务不能因为前面顺利就自动开始"，每项需独立 ADR，
包含真实用户场景、现有方案失败证据、威胁模型、性能/可用性验收和卸载方案。

## 结论：除 P9-05 外，其余 P9 项延后

| 项 | 状态 | 理由 |
|---|---|---|
| P9-01 session branching/fork | 延后 | 需要真实"多分支调查/对比实验"用户场景；当前 session 单线足够 |
| P9-02 SQLite derived catalog/FTS | 延后 | 当前 files/search 工具在 10k 文件内够用；FTS 需要真实搜索痛点证据 |
| P9-03 Wasm component extensions | 延后 | 无第三方扩展消费者（P7-06 同机制）；引入依赖需真实 consumer |
| P9-04 user-defined workflow DAG | 延后 | 无真实工作流用户场景；当前 agent loop 顺序执行 |
| P9-05 recursive agents depth > 1 | 已实现 | AgentGraph 统一托管 parent/child；由 max_depth/max_active/max_total runtime policy 限制 |
| P9-06 dynamic capability/profile reload | 延后 | 无热切换真实需求（P5-03 同）；静态 profile 足够 |
| P9-07 O7 OpenTelemetry exporter | 延后 | 无 collector/远程诊断部署；O2 本地 sink + O5 inspector 已覆盖本地观测 |

## 延后触发条件（任一出现即启动对应 ADR）

1. 真实用户场景描述（含失败证据：现有方案做不到什么）；
2. 威胁模型（新能力引入的攻击面与缓解）；
3. 性能/可用性验收基准；
4. 卸载方案（feature disable 回退）。

## 对 roadmap 的影响

- 其余 P9 不作为本轮"完成"目标；延后是**符合 roadmap 机制**的决策（不是跳过）；
- 相关前置已完成：O2 sink（P2-08）、O3 telemetry（P2-09）、O4 invariants
  （P4-11）、O5 inspector（P6-09）——本地观测完整，P9-07 无紧迫性；
- 若未来出现真实需求，按本 ADR 的触发条件启动。
