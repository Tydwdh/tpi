# 13. 词汇审计：Run / Turn / Step / Attempt（P1-01）

> 状态：审计完成（2026-08-14）。目标词汇定义见
> [02-target-architecture.md](02-target-architecture.md) §3；本文档是**当前代码
> 与目标词汇的映射 + 歧义点清单**，用于指导后续逐点迁移。**本审计不改 session
> wire 名称、不做大规模 rename**。

## 1. 目标词汇（02 §3）

| 词 | 定义 |
|---|---|
| **Run** | 一次 public `send`/resume 触发的执行，直到 stop / await input / cancel / budget / fatal terminal |
| **Turn** | 一个用户/steering 输入批次引出的连续工作；可包含多个 Step |
| **Step** | 一次 model request + 该 response 要求的全部工具处理与结果回填 |
| **Attempt** | 同一 Step 内 provider transport retry/recovery 的一次网络尝试 |

不变量（02 §3）：一个 Step 至多提交一个 assistant message carrier；工具结果必须
属于产生它们的 Step；Attempt 中断可 durable 记录诊断但不能伪装成 committed
assistant message；Run terminal 前所有已启动工具必须有 terminal outcome。

## 2. 当前代码词汇盘点

### 2.1 `run`（与目标 Run 一致，无歧义）

| 位置 | 用法 | 真实含义 |
|---|---|---|
| `src/session/mod.rs` `SessionEvent::RunStarted/RunCompleted` | durable 事件 | 一次 send/resume 触发的执行边界 = Run ✓ |
| `src/agent/mod.rs` `session.begin_run() → RunId` | 类型化 ID | Run 身份（`RunId` 已存在）✓ |
| `src/agent/mod.rs` `info_span!("agent.run", %run_id)` | trace span | Run 级 span ✓ |
| `src/agent/mod.rs` `'run_loop: loop` | 控制流 | 整个 run 的主循环（内部每轮 = 一次 model request）✓ |
| `src/agent/mod.rs` 尾部 `info!(run_id, turns = turn, ...)` | 日志 | `turns` 字段名误导（见 §3.1） |
| `src/app.rs` `/retry` → 空 `user_message` 发起 `agent::run` | 命令 | **新 Run**（重试上次失败的 run）✓ 语义正确 |

### 2.2 `turn`（**与目标 Turn 冲突：实际 = Step**）

| 位置 | 用法 | 真实含义 |
|---|---|---|
| `src/agent/mod.rs` `let mut turn = 0u32;` + `turn += 1` | 计数器 | 每次 model request 递增 = **Step 序号** |
| `src/agent/mod.rs` `RuntimeEvent::TurnStarted { turn }` | live event | 每次 provider 请求前发送 = **StepStarted** |
| `src/agent/mod.rs` 注释 "一个 request_id 标识一个逻辑 model turn" | 注释 | request_id 实际是 **Step 级**（每 request 新 id）✓ 实现正确，命名误导 |
| `src/session/mod.rs` `RunLimits.max_turns` | durable（RunStarted payload） | **Step 级预算**（= config.max_model_turns，wire 字段，不可改名） |
| `src/config.rs` `max_model_turns` | 配置 | **Step 级预算** |
| `src/agent/mod.rs` 尾部 `turns = turn` | 日志 | Step 计数 |
| `src/tui/model.rs` `view.turn: u32` | UI 状态 | 展示当前 Step 序号（StatusLine::Running { turn }） |
| `src/tui/reducer.rs` `RuntimeEvent::TurnStarted { turn } => view.turn = turn` | 投影 | Step 序号 → UI |
| `src/app.rs` `ui_state.view.turn = 0`（3 处） | 重置 | run 开始前清 Step 计数 |

### 2.3 `step`（几乎不用；无命名冲突）

- `src/tool/scheduler.rs`：只有 **waves**（执行批次），无 `step` 概念。
- session / agent 无 `step` 标识符。
- **结论**：目标词汇的 Step 在代码中叫 `turn`；`step` 一词空闲，可作未来 rename 目标名。

### 2.4 `attempt`（与目标 Attempt 大致一致）

| 位置 | 用法 | 真实含义 |
|---|---|---|
| `src/agent/mod.rs` `'attempt: loop` | 控制流 | 同一 Step 内 provider 传输/恢复循环 ✓ |
| `RuntimeEvent::StreamRecovering { attempt }` | live event | text-only 断联后的第 N 次自动续写 ✓ |
| `RuntimeEvent::TurnRestarting { attempt }` | live event | partial tool-call 后第 N 次整体重生成 ✓ |
| `MAX_STREAM_RECOVERIES` / `MAX_TURN_RESTARTS` | 常量 | Step 内 attempt 上限 ✓ |
| `tests/p1_fixes.rs` `interrupted_attempt_records_...` / `recovery_capped_after_max_attempts` | 测试 | 传输中断/恢复次数 = Attempt 级 ✓ |

### 2.5 `retry`（Run 级操作）

| 位置 | 用法 | 真实含义 |
|---|---|---|
| `src/app.rs` `/retry` slash 命令 | 命令 | 对上次失败 run 发起**新 run**（空 user_message，不重复 UserSubmitted） |
| `src/agent/mod.rs` `manual_retry_continuation` | 标志 | 新 run 的首个 request 注入续写指令 |
| `src/agent/mod.rs:292` 注释 "RunStarted 作为新 attempt" | 注释 | **误导**：retry = 新 Run，不是 attempt |

## 3. 歧义点清单（真实混用）

### 3.1 `turn` 命名冲突（唯一实质性命名冲突）

- **症状**：agent 的 `turn`（= Step）与目标词汇的 Turn（用户输入批次）同名不同义。
  涉及 `TurnStarted`、`max_turns`、`view.turn`、日志 `turns =`。
- **影响**：读代码者会把"turn 数"理解为"用户消息轮数"，实际是"model request 次数"
  （同一用户消息可触发多次 request，如 tool 循环后再次请求）。
- **约束**：`RunLimits.max_turns` 是 **session wire 字段**（RunStarted payload），
  不可改名；`RuntimeEvent::TurnStarted` 是 live event（非 durable，可未来改）。
- **迁移建议**（一次一个歧义点，不 rename wire）：
  1. **本轮（P1-01）**：文档 + 修正误导注释（不 rename）。
  2. **后续（P1-03 live event 拆分时）**：`RuntimeEvent::TurnStarted` → 随 view event
     分离改名为 `StepStarted { step }`（live event 非 durable，改名安全）；
     `view.turn`/`StatusLine::Running { turn }` 同步改名。
  3. **`max_turns` 保留**：wire 兼容，文档标注"= Step 预算"。

### 3.2 "RunStarted 作为新 attempt" 注释（agent/mod.rs:292、p1_fixes.rs:1083）

- **症状**：`/retry` 发起的新 run 被注释称为 "attempt"。
- **影响**：与目标词汇冲突（attempt = Step 内传输恢复；retry = 新 Run）；
  测试读者误以为 retry 是 attempt 级操作。
- **修正**：注释改为 "RunStarted 记录新 run"（本轮做，纯注释零行为影响）。

### 3.3 `AssistantMessage` 注释 "已提交的 assistant turn"（session/mod.rs:41）

- **症状**：一次 provider response 的持久化被注释称为 "assistant turn"。
- **影响**：轻微；此处 turn 实指 Step 级 assistant 消息。
- **修正**：注释改为 "assistant message（Step 级）"（本轮做）。

## 4. 状态转换表与测试对齐

### 4.1 Run 级状态机（durable：SessionEvent）

```text
Idle
  | send / resume（/retry 也是新 run）
  v
RunStarted ──────────────► Running ──────────────────────────────► RunCompleted
  (durable)                │                                        (Stop / MaxTurns /
                          │ 每次 model request（Step）              Error / ProviderUnavailable
                          │   ├─ 'attempt: loop（传输恢复）          / ProviderInterrupted /
                          │   │    StreamRecovering / TurnRestarting  Cancelled / AwaitingUserInput /
                          │   └─ 工具执行 → ToolRequested/Started/Completed   WallTimeExceeded)
                          │        → 回填 → 下一 Step
                          │
                          ├─ request_input → UserInputRequested → RunCompleted(AwaitingUserInput)
                          │        → UserInputReceived → 继续（新 Run 边界）
                          └─ cancel → RunCompleted(Cancelled)
```

### 4.2 与现有测试对齐验证

| 测试 | 断言 | 语义 | 与本文档一致 |
|---|---|---|---|
| `tests/agent_flow.rs:59` | `TurnStarted { turn: 1 }` = 第一个 model request | Step 序号 | ✓（当前代码 turn=Step） |
| `tests/tui_reducer.rs:183` | `TurnStarted { turn: 2 }` | Step 序号 → UI | ✓ |
| `tests/p1_fixes.rs:358` | 中断 attempt 记录 partial、不伪装 committed | Attempt 语义 | ✓ |
| `tests/p1_fixes.rs:794` | 每次 attempt 断联、续写达上限停止 | Attempt 语义 | ✓ |
| `tests/p1_fixes.rs:1083` | retry 记录 RunStarted、不重复 UserSubmitted | 新 Run 行为 | 行为 ✓，注释命名 ✗（§3.2） |
| `tests/p1_fixes.rs:49`（P1-1） | cancel 保留 session 一致 | Run 级 | ✓ |
| `tests/trace_ancestry.rs`（P0-09） | 并发 run span 不交叉 | Run 级 | ✓ |

**结论**：现有测试的**行为断言**全部与目标词汇一致（当前代码的 turn=Step、
attempt=Attempt、run=Run）；只有**两处注释**的命名与目标词汇冲突（§3.2/3.3），
以及 `turn` 的标识符命名与目标 Turn 冲突（§3.1，受 wire 约束保留）。

## 5. Typed ID 结论：无需新增

| 目标身份 | 当前实现 | 结论 |
|---|---|---|
| RunId | `src/ids.rs::RunId`（UUIDv7） | 已有 ✓ |
| RequestId（= Step 身份） | `src/ids.rs::RequestId`，每个 model request 新分配 | 已有 ✓（实现已按 Step 级分配） |
| Attempt 身份 | `(RequestId, u32)`（attempt 计数） | **无需独立 ID**：attempt 是同一 Step 内的有序重试，`(request_id, attempt_n)` 三元组可唯一标识；独立 AttemptId 增加无收益的 UUID 开销 |
| ToolCallId | `src/ids.rs::ToolCallId` | 已有 ✓ |
| EventId / SessionId | `src/ids.rs` | 已有 ✓ |
| TurnId | — | **无需**：目标 Turn = 用户/steering 输入批次，当前实现中一个用户消息 = 一个 Run（`RunId` 即批次身份）；steering/follow-up inbox（P8）落地时才需要独立 TurnId，届时随 inbox 一起引入 |

> 补充：`RunLimits.max_turns` 虽名为 turn，实际是 Step 预算；它出现在
> `RunStarted` durable payload 中，**改名为 max_steps 会破坏 session wire**，
> 故保留原名并在文档标注（P1-01 约束：保持 session wire 名称）。

## 6. 本轮落地（P1-01 范围内）

1. 本文档（13-vocabulary-audit.md）：词汇盘点 + 映射 + 状态转换表 + ID 结论。
2. 修正误导注释（§3.2/3.3）：`agent/mod.rs:292`、`p1_fixes.rs:1083`、
   `session/mod.rs` AssistantMessage 注释——纯注释，零行为影响，不改 wire。
3. **不做**（留待后续任务）：`RuntimeEvent::TurnStarted` 改名（P1-03 live/view
   event 分离时）、`max_turns` 改名（wire 约束，永不改）。

## 7. 遗留（非本任务）

- P8 inbox 落地时引入 `TurnId`（steering/follow-up 批次身份）。
- P1-03 把 live runtime event 与 view event 分离时，`TurnStarted`→`StepStarted`、
  `view.turn`→`view.step` 一并改名（live event 非 durable，改名安全）。
