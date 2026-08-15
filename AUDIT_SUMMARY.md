# Executive Summary — TPI 功能级代码审计

审计范围：全 workspace（7 crates，~57k 行 Rust），20 项功能逐一两轮审计（Design +
Bug）+ 系统级跨功能审计。审计期间未修改生产代码（只写审计文档）。

## 功能总数 / 已审计功能数

**20 / 20**（F01 Config → F20 Subagent，见 AUDIT_PROGRESS.md）

## 问题统计

| 严重程度 | 数量 |
| --- | --- |
| Critical | 0 |
| High | 9 |
| Medium | 24 |
| Low | 19 |
| 合计 | 52 |

置信度分布：Confirmed 43 · Highly Likely 3 · Suspicious 6。

> 说明：未发现 Critical 级（确认的数据损坏/安全逃逸/核心功能完全错误）——本地主路径
> （Local workspace + bash/edit/session）整体工程质量高，失败路径大多有兜底。最接近
> Critical 的两个 High 都集中在 **Remote（SSH）** 路径。

## 最危险的 10 个问题

1. **ISSUE-002** — `remote is_no_such_file` 把网络/权限错误当"文件不存在"→ 远端文件被
   stale revision 静默覆盖（不可逆数据丢失）。
2. **ISSUE-001** — 工具预算超限产生悬空 tool_calls → 该 session 后续所有消息 provider
   必失败（会话被"毒化"）。
3. **ISSUE-006** — MCP reader 无取消点，server 衍生孙进程持管道时退出永久挂死。
4. **ISSUE-003** — edit 失败诊断无上限输出 + O(N·L) 扫描（大文件模型上下文/CPU DoS）。
5. **ISSUE-004** — Windows 大小写不敏感 FS 上资源锁大小写敏感 → 同一文件并发写静默丢内容。
6. **ISSUE-007** — remote baseline 捕获无超时无取消 → remote_bash 无限挂起并锁住全部远端命令。
7. **ISSUE-008** — 远端输出/文件读取无大小上限 → 进程级 OOM。
8. **ISSUE-009** — 确定性 4xx/认证错误被指数退避重试（最多 ~100 次请求、~6 分钟假重试）。
9. **ISSUE-005 / ISSUE-021** — ProcessRegistry 与 MCP stdout channel 无界增长（长会话内存泄漏）。
10. **ISSUE-014/015/016/018** — TUI 键位语义群：帮助文案宣称的 Ctrl+C 语义不存在、
    运行中 Ctrl+D 被吞、搜索/模态内 Ctrl 组合键被当字面字符。

## 最容易产生 Silent Bug 的模块

1. **`crates/tpi-capabilities/src/remote/`**（SSH/SFTP）——错误分类过粗、无界、无超时，
   与本地路径行为不对称；Remote 是"第二实现"，契约与 Local 共享但实现漂移。
2. **`crates/tpi-agent/src/agent/tool_runtime.rs`**——批执行状态机（预算/挂起/拒绝）
   的持久化完整性，一处提前 return 就破坏消息单元原子性。
3. **`crates/tpi-tui/src/reducer.rs`**——模态/搜索/运行的键位路由优先级，修饰位过滤
   缺失导致"按键变字符"。
4. **`crates/tpi-config/src/config.rs`**——默认值、文档、测试 fixture 三处不一致 +
   死配置字段，用户与文档都被欺骗。

## 最脆弱的生命周期

**MCP server 生命周期**（ISSUE-006/021/032）：spawn 正确、RAII 注销正确，但 shutdown
的 quiescence 依赖"任务自己检查取消点"，而 reader 阻塞在不可取消的同步读上——退出路径
可能无限挂起。**Managed Process**（ISSUE-005/031/025）次之：registry 无淘汰、cancel 状态
竞态、一处 detached spawn 违反自身 ADR。

## 最危险的并发路径

1. **预算超限/挂起的批执行与 session 持久化交错**（ISSUE-001）——提前 return 漏持久化。
2. **Windows 同文件大小写双写**（ISSUE-004）——锁语义与 FS 语义不一致，静默丢内容。
3. **MCP shutdown quiescence**（ISSUE-006）——supervisor wait 依赖不可取消的 IO。
4. **远端二次校验与 rename 的 TOCTOU**（ISSUE-033）。

## 最危险的跨模块 Contract

1. **"assistant.tool_calls 的每个 call 必须有对应 Tool 结果"**——tool_runtime 预算路径
   违反（ISSUE-001）；request_input 挂起路径已正确处理，预算路径未对齐。
2. **"工具 timeout_ms 契约"**——remote baseline 未兑现（ISSUE-007）。
3. **"Local/Remote 共享 BashArgs 语义"**——相对 cwd 两端解析不同（ISSUE-022）。
4. **"帮助文案 = 实际键位"**——Ctrl+C/Ctrl+D 文案与实现脱节（ISSUE-014）。
5. **"overlay 工具可见性"**——registry 三视图不一致（ISSUE-011）。

## 测试覆盖明显不足的功能

- **Remote/SSH**（F13）：多数问题无测试（is_no_such_file 分类、无界、无超时、cwd 语义）。
- **预算超限路径**（F09）：无"单 turn 超 max_tool_calls"的持久化完整性测试。
- **MCP shutdown 挂死**（F11）：无"孙进程持管道"场景。
- **TUI 键位语义**（F15）：搜索/模态内修饰位、Ctrl+D running 门控无 reducer 测试。
- **配置默认值一致性**（F01）：无"默认值与文档一致"的契约测试。

## 推荐修复顺序

```text
第 1 批（数据安全 / 会话毒化，必改）：
  ISSUE-001（预算超限悬空 tool_calls）→ ISSUE-002（远端错误分类）
第 2 批（挂死 / OOM）：
  ISSUE-006（MCP shutdown）→ ISSUE-007/008（remote 超时与有界）
第 3 批（正确性）：
  ISSUE-004（大小写锁）→ ISSUE-003（edit 诊断有界）→ ISSUE-009（4xx 重试）
第 4 批（资源）：
  ISSUE-005（ProcessRegistry 淘汰）→ ISSUE-021（MCP channel 有界）
第 5 批（UX 一致）：
  ISSUE-014~018（键位语义）→ ISSUE-019/020（配置一致性）
第 6 批（Medium/Low 其余）：
  按 AUDIT_FINDINGS.md 逐条
```

每条修复的验证方法见 AUDIT_FINDINGS.md 对应条目（大多数都有可落地的单测/集成测试方案）。

## 审计限制（剩余风险）

- 未运行真实 provider / 未做人工交互验证：所有 UI 类问题（ISSUE-014~018、029、030、
  046、047）的"用户感知"层面是代码路径推断，需人工 TUI 验证。
- 未做长时运行（10 小时级）实测：ISSUE-005/021 的无界增长影响程度基于代码结构推断。
- Remote 层多数发现未用真实 SSH 环境复现（需要远端环境）。
- `scripts/`、`.github/`、`examples/`、`docs/` 不在本审计范围。
