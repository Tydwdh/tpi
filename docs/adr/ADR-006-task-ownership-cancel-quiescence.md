# ADR-006：Task Ownership、Cancel 与 Quiescence

- 状态：**Approved（P0-08，2026-08-14；用户原则批准：每个 task 必须有 owner、
  parent cancellation、terminal outcome 与 join/flush 边界）**
- 关联：`docs/refactor/06-agent-runtime-and-subagents.md`、`docs/refactor/08-migration-roadmap.md` P0-08 / P2-05

## Context

现状（审计 Medium-3）：局部代码理解 detach 风险——有 `AbortTaskOnDrop`
（`src/agent/mod.rs:32`）与 `CancellationToken`，但无进程/agent 级 supervisor。
已观察到的症状：

- tool stream forwarder 执行后直接 `abort()` 未 join；
- 非交互 run 为避免 UI channel 堵塞额外启动永久 drain task
  （`src/app.rs` `run_prompt_once`：`tokio::spawn(async move { while rx.recv().await.is_some() {} })`），
  用"常驻消费者"掩盖了"presentation channel 与 core 耦合"的生命周期错误；
- watchdog 等任务散落各处，无统一 shutdown 协议。

## Decision

**每个异步任务必须有 owner、parent cancellation、terminal outcome 和
join/flush 边界**。统一机制：

- 分层 `CancellationToken`（父 token 派生 child token；父 cancel 传播到全部 child）；
- `tokio_util::task::TaskTracker` 追踪任务，`supervisor.shutdown()` 先 cancel、
  tracker.close()、等待全部 tracked task 完成（quiescence）、汇总错误；
- 独立 `Supervisor` 包装（`src/process/` 或 agent 层新建），禁止各模块发明不同的
  shutdown 顺序。

shutdown 顺序（固定协议）：

```text
停止接收（close input / stop accepting）
→ cancel（CancellationToken，全部子任务）
→ 关闭 tracker（TaskTracker::close）
→ 等待（wait tracked tasks，join）
→ 汇总错误
→ flush durable data（session sync / writer flush）
```

禁止：

- detached `tokio::spawn`（无 owner、无跟踪）；
- 只 `abort()` 不 join（AbortTaskOnDrop 是过渡形态，P2-06 迁入 supervisor 后删除）；
- 锁内 await（`std::sync::Mutex` guard 跨 await）；
- 用永久 drain task 掩盖生命周期错误（必须消除 UI channel 与 core 的耦合，
  而非保留 drain）。

## Alternatives

- **维持现状（AbortTaskOnDrop + 散落 CancellationToken）**：拒绝。无法回答
  "所有任务是否已结束"，shutdown 顺序不一致，永久 drain task 掩盖真问题。
- **自建全局 event bus + 全局 task 注册表**：拒绝（`03-rust-technology-decisions.md`
  §2：会掩盖 owner、顺序和背压；使用窄 typed sink/channel）。
- **Async runtime 自动清理**：拒绝。drop 不会取消 JoinHandle（脱离），依赖 runtime
  回收任务会泄漏资源与状态。

## Consequences

正面：
- `supervisor.shutdown()` 幂等；任意路径（正常/错误/cancel/shutdown）都能收敛。
- 100 次 start/shutdown 后 tracked tasks 为 0 可测（P2-05 leak test）。
- 为 P8 子代理的 parent cancellation 传播提供基础。

负面/成本：
- 每个 spawn 点都要显式归属 supervisor（迁移成本；P2-06 一次一个 spawn）。
- TaskTracker 增加少量运行时开销（可忽略；P0-05 基线无感知）。

## Migration

1. P2-05 `Supervisor` walking skeleton：先拥有一个无害 background task，验证
   cancel/close/wait/aggregate error；CancellationToken + TaskTracker；leak test
   （100 次 start/shutdown 后 tracked=0）。
2. P2-06 迁 Agent watchdog / tool stream forwarder：一次一个 spawn；删除直接
   abort 或在 bounded hard-stop 后 await；验收：cancel at every await、
   terminal event ordering。
3. P2-07 MCP/process owner inventory：owner 不明的 task 纳入相应 supervisor，不改协议。
4. P3-06 terminal input adapter ownership：blocking thread 增加 start/shutdown/join
   contract（退出/terminal error/ctrl-c 不遗留线程）。
5. P3-05 headless：直接订阅 semantic runtime，**删除** `run_prompt_once` 的永久
   drain task（headless 不需要 UI channel）。

## Rollback

- 单任务迁移：revert PR（行为等价，仅归属变化）。
- Supervisor 本身：保留 wrapper API，回退到直接 spawn + token（不推荐，但可逆）。

## Evidence

- `docs/refactor/00-current-state-audit.md` Medium-3（症状与根因）。
- `src/agent/mod.rs:32` `AbortTaskOnDrop`（现有局部缓解，标注为过渡）。
- `src/app.rs` `run_prompt_once` 永久 drain task（待 P3-05 删除）。
- `03-rust-technology-decisions.md` §2（TaskTracker 官方建议：先 cancel 再 wait）。

## 非目标

- 不定义子代理预算/隔离（见 P8 / ADR-009）。
- 不把 shutdown 变成全局单例；每个 owner 有自己的 supervisor。
- 不引入 async-task、futures 之外的第三方取消库（CancellationToken 已足够）。
- 本 ADR 不改变 session 持久化语义（flush 顺序见 ADR-003 相关章节）。
