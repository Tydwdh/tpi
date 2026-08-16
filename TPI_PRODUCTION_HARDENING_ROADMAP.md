# TPI Production Hardening Roadmap

> 目标：在不进行第二轮大重构的前提下，把当前 TPI 从“功能完整的 Coding Agent”推进到“具备生产级正确性、事务性、隔离性与恢复能力的 Agent Runtime”。

---

## 0. 总原则

当前阶段不再横向堆功能，优先补齐系统不变量。

核心目标：

- 不静默丢数据
- 不产生非法 Session
- 不覆盖用户后续修改
- 不让后台任务无限泄漏
- 不让外部错误被误判
- 不让 Workspace 修改绕过审计/撤销边界
- 不让生命周期无限等待
- 不让 Retry 发生多层乘法放大
- 不让模型承担不必要的机械编辑责任

当前 Kernel 边界尽量保持稳定：

```text
AgentLoop
Session
Context
ToolExecutor
ToolRegistry
```

后续工作围绕这些边界补 invariant，而不是重新设计一套 AgentLoop。

---

# Phase A — Edit V3：恢复稳定的模型侧编辑协议

## A1. 删除模型侧行号编辑原语

从 agent-facing schema 删除：

```text
edit_range
replace_lines
insert_before
insert_after
```

行号只用于：

```text
read
search
navigation
```

不再作为 mutation address。

---

## A2. 模型侧只保留一个 Edit primitive

统一成：

```text
edit(
    path,
    revision,
    replacements = [
        {
            old_text,
            new_text
        }
    ]
)
```

语义：

> 把“这段现有文本”变成“这段新文本”。

插入、删除、替换全部由 `old_text -> new_text` 表达。

---

## A3. 保留 V2 的底层事务内核

继续保留：

- revision 校验
- batch edit
- 同一 snapshot 上 resolve
- overlap 检测
- `ResolvedSplice`
- reverse splice
- atomic commit
- Mutation Journal
- logical LF
- Windows 原子替换

最终结构：

```text
Model
  ↓
old_text -> new_text
  ↓
revision check
  ↓
canonical matching
  ↓
unique match
  ↓
ResolvedSplice[]
  ↓
overlap check
  ↓
reverse apply
  ↓
atomic commit
  ↓
Mutation Journal
```

---

## A4. Fuzzy Matching 改成 Conservative Canonical Matching

不要做“找最像的位置”。

只容忍表示差异：

```text
Exact
↓
TrailingWhitespaceEquivalent
↓
UniformOuterIndent
↓
Fail
```

规则：

```text
0 match  -> 进入下一层
1 match  -> 成功
>1 match -> MultipleMatches，立即失败
```

禁止：

- 相似度打分后自动挑一个
- 编辑距离推测
- AST 猜测模型意图
- 多候选自动选最接近位置

原则：

> Normalize representation, preserve structure, never guess semantics.

---

## A5. CRLF/LF 不属于 fuzzy

文件层统一：

```text
physical file
    ↓
detect original EOL
    ↓
convert to logical LF
    ↓
match / splice
    ↓
restore original EOL on commit
```

模型永远不需要处理 CRLF/LF。

---

## A6. Tab/Space 保守处理

默认保持当前“共同外层缩进平移”策略。

允许：

```text
    foo()
        bar()
```

和：

```text
        foo()
            bar()
```

因为只是统一 outer indent 变化。

默认不允许把：

```text
\tfoo()
\t\tbar()
```

直接视为：

```text
    foo()
        bar()
```

除非未来增加语言感知的 `IndentStyleEquivalent`，并且满足：

- 当前语言明确为 indentation-insensitive
- relative indentation structure 完全一致
- mapping 唯一
- match 唯一

Python / YAML / Makefile 等缩进敏感场景保持严格。

---

## A7. Edit V3 验收指标

做固定 A/B/C benchmark：

```text
V1
V2
V3
```

固定：

- 模型
- temperature
- system prompt
- repository snapshot
- 任务集

至少统计：

- edit first-pass success rate
- NoMatch count
- MultipleMatches count
- stale revision count
- repair edit count
- syntax break count
- extra read count
- token usage
- task success rate

重点指标：

```text
repair edit rate
```

V3 的目标是显著降低“第一次 edit 把代码机械性改坏，再二次修复”的比例。

---

# Phase B — Session / Mutation 数据完整性

## B1. Tool Call 生命周期必须闭合

定义 invariant：

```text
Every model-emitted tool_call eventually has exactly one terminal result.
```

无论出现：

- tool budget exceeded
- ask_user suspend
- user cancellation
- preflight failure
- scheduler fatal error

都必须补齐每个 tool call 的终态。

建议统一：

```text
finalize_unexecuted_calls(reason)
```

不要给不同退出分支分别写特殊逻辑。

---

## B2. Undo / Redo 使用 CAS

Undo：

```text
current == mutation.after
    -> restore mutation.before

current == mutation.before
    -> AlreadyUndone

otherwise
    -> UndoConflict
    -> 不写文件
```

Redo：

```text
current == mutation.before
    -> restore mutation.after

current == mutation.after
    -> AlreadyRedone

otherwise
    -> RedoConflict
```

必须覆盖测试：

```text
Agent: A -> B
User:  B -> C
Undo

期望：
Conflict

禁止：
C -> A
```

---

## B3. Journal 损坏进入 Tainted 状态

不要把 destructive undo 建立在 best-effort 数据源上。

建议：

```text
JournalState {
    mutations,
    integrity: Clean | Tainted
}
```

行为：

```text
Clean:
    undo / redo allowed

Tainted:
    history view allowed
    inspect allowed
    undo / redo rejected
```

除非用户明确 `--force`。

---

## B4. Windows 文件 Identity 统一

建立统一抽象：

```text
WorkspacePathIdentity
```

所有组件使用同一 identity：

- scheduler lock
- snapshot store
- mutation journal
- edit target
- write target
- future watcher/LSP

展示路径和比较 identity 分离：

```text
WorkspacePath {
    display,
    identity
}
```

Windows 下考虑：

- separator normalization
- lexical normalization
- case-folded identity

验收：

```text
src/Foo.rs
src/foo.rs
```

必须判定为同一资源。

---

# Phase C — Retry / Provider Robustness

## C1. 保留自动 Retry

自动 Retry 是产品能力，不删除。

目标：

> 当模型/API请求偶发失败时，由 Harness 自动替用户重试，而不是要求用户手工点 Retry。

---

## C2. Retry 需要统一 ownership

真正需要避免的是多层 Retry 相乘：

```text
HTTP client retry x3
Provider retry x3
Agent retry x3

=> 最坏 27 次
```

普通 provider/network failure 只允许一个组件拥有 retry 责任。

---

## C3. 使用共享 RetryBudget

建议：

```text
RetryBudget {
    max_attempts,
    max_elapsed
}
```

例如：

```text
max_attempts = 4
max_elapsed = 30s
```

所有 retry 共用同一个 budget。

---

## C4. 错误分类决定“继续 retry 的程度”

建议策略：

```text
Cancelled
    -> 0 retry

400 / 401 / 403
    -> 至少允许 1 次 defensive retry
    -> 仍失败后停止

408 / Timeout / Transport
    -> 多次 bounded retry

429
    -> bounded retry
    -> 尊重 Retry-After

5xx
    -> bounded retry
```

原因：

- OpenAI-compatible 网关/中转未必严格遵守 HTTP 语义
- 4xx 偶发也可能来自代理层异常
- 但不能无限浪费请求

---

## C5. Retry 必须保持 Request Identity

Retry 应代表：

> 同一次请求的再次尝试。

因此保持：

- same model
- same messages
- same tools
- same params
- same session snapshot

不要在 retry 间：

- 自动 compact
- 修改 tool schema
- 重新裁剪历史
- 改模型参数

否则就已经不是“Retry”，而是新一次 Agent 决策。

---

## C6. Retry 记录

Debug/trace 中记录：

```text
attempt 1 -> 502
attempt 2 -> timeout
attempt 3 -> success
```

用户正常使用时可以不展示。

---

# Phase D — Runtime Lifecycle / Resource Boundaries

## D1. MCP shutdown 必须 bounded

统一原则：

```text
Application shutdown must be bounded.
```

任何 subsystem 都不能无限等待。

推荐 lifecycle：

```text
create
  ↓
Supervisor owns
  ↓
CancellationToken
  ↓
close IO / cancel
  ↓
wait bounded deadline
  ↓
force cleanup
  ↓
return
```

避免没有 owner 的裸 `tokio::spawn()`。

---

## D2. Remote IO 统一 Budget

任何远程 IO 必须同时具备：

```text
deadline
cancellation
size bound
```

建立：

```text
IoBudget {
    deadline,
    max_output_bytes,
    cancellation
}
```

应用到：

- remote exec
- read
- write
- search
- glob
- list
- baseline

原则：

> No network IO without deadline, cancellation and size bound.

---

## D3. Remote 错误 fail closed

只把明确的 `NoSuchFile` 映射成 NotFound。

以下全部直接失败：

- PermissionDenied
- ConnectionLost
- ProtocolError
- unknown transport error

原则：

> Unknown is not False.

---

## D4. ManagedProcess 增加 GC

Runtime registry 不应永久保留所有历史对象。

建议：

```text
running
    -> always retain

recent completed
    -> retain full metadata

older completed
    -> compact

very old
    -> remove from runtime registry
```

Durable Event Log 可以保留历史，但 Runtime object lifetime 必须有界。

---

## D5. Process Output 增加 Cursor

改成：

```text
process.output(
    id = "p1",
    after = 48321
)
```

返回：

```text
data
next_cursor = 51782
truncated = false
```

避免模型反复吃同一个 tail。

---

# Phase E — Execution Model 补完整

最终 Execution 明确分三类：

```text
Execution
├── Command
├── Job
└── Terminal
```

## E1. Command

用于：

- 一次性命令
- 有 timeout
- 短生命周期

当前 `bash foreground` 属于这一层。

---

## E2. Job

用于：

- 长任务
- watcher
- dev server
- build
- test
- 不需要交互 stdin

继续使用当前 ManagedProcess。

不要删除。

---

## E3. Terminal

单独新增 Persistent PTY：

```text
terminal_open
terminal_write
terminal_read
terminal_resize
terminal_signal
terminal_close
```

Windows：

```text
ConPTY
```

Unix：

```text
PTY
```

不要做：

```text
bash(interactive=true)
```

也不要把 Terminal 塞进 ManagedProcess。

---

# Phase F — Workspace Transaction

## F1. 不继续扩 Bash 黑名单

不要无限增加：

```text
sed -i
perl -i
tee
cp
mv
python open()
...
```

黑名单没有终点。

---

## F2. 建立 WorkspaceMutationBoundary

长期目标：

```text
before workspace state
    ↓
execute any tool
    ↓
detect workspace delta
    ↓
WorkspaceMutation
    ↓
Journal
```

这样：

```text
cargo fmt
git checkout
codegen
shell script
```

造成的修改都可以进入统一 mutation。

---

## F3. 第一阶段可以不做 OverlayFS

先做 TrackedWorkspace：

```text
pre-scan
    ↓
execute
    ↓
mtime/size fast scan
    ↓
hash changed candidates
    ↓
生成 before/after mutation
```

后续再演进到：

```text
Overlay Workspace
```

---

## F4. Journal 最终升级 CAS

未来：

```text
objects/
  b3:aaa
  b3:bbb

journal:
  before = b3:aaa
  after  = b3:bbb
```

使：

```text
revision
CAS object id
mutation snapshot id
```

尽量统一。

收益：

- 去重
- journal 变小
- undo/redo 更快
- checkpoint 简单
- future branch/session fork 更自然

---

# Phase G — TUI Correctness

当前阶段只修 correctness，不做第三次 UI 大重构。

优先：

- Ctrl+C / Ctrl+D / Esc 行为与帮助一致
- modal input 统一 modifier handling
- 多行输入不要 `trim()` 掉首行缩进
- 大粘贴截断必须明确提示
- transcript wrap cache 改增量失效
- request_input / search 输入不要吞 modifier 语义

原则：

> 用户输入中的 whitespace 和 modifier 都是数据，不是装饰。

---

# 暂时不要做

当前阶段不建议：

- 再重构 AgentLoop
- Everything-is-Plugin
- 新增 git/cargo/npm/rg 专用原生工具
- Process / Terminal 合并
- AST 自动修代码
- compiler/parser 自动兜底 Edit
- 继续增加 fuzzy heuristic
- 先做 Remote Sandbox
- 为了架构图漂亮重写 pipeline
- 扩张 Subagent 高级能力

---

# 推荐实施顺序

```text
Phase A
Edit V3
    ↓
Phase B
Tool-call lifecycle
Undo/Redo CAS
Windows PathIdentity
Journal integrity
    ↓
Phase C
RetryBudget / Retry ownership
    ↓
Phase D
Remote IO budget
MCP bounded shutdown
Process GC
Process output cursor
    ↓
Phase E
Persistent PTY Terminal
    ↓
Phase F
WorkspaceMutationBoundary
Journal CAS
    ↓
Phase G
TUI correctness hardening
```

---

# 每个功能统一采用的实践流程

## 1. 先写一句 invariant

示例：

```text
Undo never overwrites content that was not produced by the target mutation.
```

```text
Every model-emitted tool_call eventually has exactly one terminal result.
```

```text
Application shutdown always finishes within a bounded deadline.
```

```text
A retry replays the same request identity within a shared retry budget.
```

---

## 2. 写 adversarial scenario

优先测坏路径：

- 同一文件不同大小写
- Agent edit 后用户手工改文件
- MCP 子进程继承 stdout 后不退出
- Remote stat 时断线
- Provider 连续返回 401/502/timeout
- Edit old_text 有两个候选
- 连续启动 100 个后台短进程
- Cancel 发生在 tool prepared 与 execute 之间

---

## 3. 先写 contract test

测试名直接表达 contract：

```text
undo_conflicts_with_external_change
budget_exceeded_still_completes_every_tool_call
windows_paths_with_different_case_conflict
remote_permission_error_is_not_not_found
mcp_shutdown_is_bounded
completed_process_registry_is_bounded
retry_budget_prevents_nested_retry_amplification
retry_preserves_request_identity
edit_rejects_ambiguous_old_text
```

---

## 4. 再实现

每次问：

> 这个 invariant 应该属于哪个深模块？

不要把同一规则复制到多个工具。

例如 path identity 应成为共享 primitive，而不是分别散落在 edit/write/scheduler。

---

## 5. Failure 使用结构化结果

优先：

```text
UndoConflict
StaleRevision
AmbiguousMatch
BudgetExceeded
Timeout
OutputLimitExceeded
JournalTainted
RetryBudgetExhausted
```

不要让模型/TUI解析字符串判断错误类型。

---

## 6. 做真实 Agent 回归

每个大能力改动都应该有固定任务集。

尤其 Edit V3：

- 插入 if
- 删除分支
- 修改函数
- 添加 match arm
- 修改嵌套闭包
- CRLF 文件
- Tab 文件
- 重复代码
- 多 replacement
- 大函数
- JSON/TOML

保持环境固定，比较行为和指标。

---

# Production Gate

下面这些全部成立后，再把 TPI 称为 production-ready core：

```text
□ Edit 不依赖 line mutation address
□ Edit 使用 before -> after 局部变换
□ Tool-call 生命周期永远闭合
□ Undo 不覆盖用户/外部后续修改
□ Journal 损坏不会静默执行 destructive undo
□ 同一物理文件永远共享同一 identity
□ Remote 错误 fail closed
□ Remote IO 有 deadline/cancel/size bound
□ Runtime registry 有 GC / 上限
□ Shutdown 有 deadline
□ Retry 自动存在，但只有一个 owner
□ Retry 使用共享 RetryBudget
□ Retry 保持 request identity
□ Command / Job / Terminal 生命周期分离
□ Workspace side effects 可观测
□ Session resume 不产生非法 provider history
□ TUI 不改变用户原始输入语义
```

---

# 当前最建议立即连续完成的四项

```text
1. Edit V3
2. Tool-call lifecycle closure
3. Undo / Redo CAS
4. Windows PathIdentity
```

然后再进入：

```text
RetryBudget
Remote/MCP bounded runtime
Persistent PTY
WorkspaceMutationBoundary
```

这条路线的重点不是继续增加工具数量，而是把现有 TPI 的正确性、事务性、隔离性和恢复能力做实。
