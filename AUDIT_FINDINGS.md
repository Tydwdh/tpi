# AUDIT_FINDINGS — TPI 功能级代码审计问题清单

> 只记录有证据的真实问题（Confirmed = 代码路径可确认；Highly Likely = 逻辑强烈
> 表明存在问题但需 runtime 验证；Suspicious = 值得调查，证据不足）。
> 行号为 2026-08-16 工作树行号。

---

# High

## ISSUE-001 — 工具预算超限路径产生悬空 tool_calls，resume 后消息序列非法

**功能：** F09 工具执行/调度
**严重程度：** High
**类型：** Contract Mismatch / Data Flow
**置信度：** Confirmed
**位置：** `crates/tpi-agent/src/agent/tool_runtime.rs:296-333`（预检循环内预算检查提前 return）
**调用链：**
```text
agent::run（turn N 提交 assistant(tool_calls=[c0..c7]) 到 session 与 messages）
→ ToolBatchExecutor::execute
→ 预检循环：c0..c4 已 prepared（tool_calls_total 达到 max）
→ 在 c5 处检查 tool_calls_total >= max → 只为 calls[5..] 合成 Rejected 并持久化
→ return BatchEnd::BudgetExceeded
→ agent 记录 RunCompleted(MaxToolCalls)，run 结束
→ 用户下一条消息：history 从 session 重建
→ assistant 消息携带 c0..c7 的 tool_calls，但只有 c5..c7 有 Tool 结果
→ provider 收到无结果 tool_call（c0..c4）→ 400
```
**当前行为：** 预算超限时，`for skipped in &calls[index..]` 只为**剩余**调用合成 Rejected 结果；
已计入预算的 `calls[0..index]`（可能刚被 prepared 或已 rejected）没有任何终态事件被持久化，
也**不会执行**（函数在构建 waves 前直接 return）。session 中 assistant 消息的 tool_calls
包含全部调用，但只有后半段有 ToolCompleted。
**期望行为：** 与 request_input 挂起路径（同文件 :695-743）一致——本批所有未执行的调用
（含已 prepared 但未执行的前段）都必须合成标准化 Rejected 并持久化 ToolRequested/ToolCompleted。
**触发条件：** 配置 `max_tool_calls = N`，模型在一个 turn 内发出 >N 个工具调用。
**根因：** 预算检查把"计数"与"执行/持久化"耦合在同一个预检循环里，提前 return 前未对
已计数未执行的调用补齐终态。
**影响：** 该 session 后续任何消息（resume 或继续对话）重建出的 provider 消息序列非法，
请求必失败（`tool_calls` 引用无对应结果）。用户被迫 /new。数据本身未损坏，但会话被"毒化"。
**建议修复：** 预算超限分支先遍历 `calls[..index]` 中不在 `rejected` 里的调用，为其合成
Rejected 终态并持久化（与 :695-743 相同逻辑，提取公共函数），再 return。
**建议测试：** 构造 `max_tool_calls=2` + 单 turn 3 个工具调用 → 断言 session 中每个
tool_call_id 都有 ToolCompleted；再 `project_messages` 断言 assistant 的每个 tool_call
都有对应 Tool 消息。

---

## ISSUE-002 — remote `is_no_such_file` 把所有 SFTP 错误当"文件不存在"，网络错误时静默覆盖远端文件

**功能：** F13 Remote/SSH
**严重程度：** High
**类型：** Error Handling / Data Loss
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/remote/files.rs:367-369`，调用点 `:54`、`:274-278`
**调用链：**
```text
remote_write → sftp.stat(target)
→ Err(SshError::Sftp(_))（权限错误/连接中断/协议错误都走这里）
→ is_no_such_file() == true → exists = false
→ 跳过 274-321 行 stale-revision 校验
→ 走 write_file（temp + rename 覆盖目标）
```
**当前行为：** `matches!(e, SshError::Sftp(_))` 匹配 SFTP 层的**所有**错误类型，
不只"文件不存在"（SSH 的 NoSuchFile）。
**期望行为：** 只有 SFTP 状态码 == NoSuchFile 时才视为不存在；其他 SFTP 错误（权限/网络/协议）
应让写操作整体失败。
**触发条件：** 远端网络瞬时抖动/ssh 连接半死时执行 remote_write 到已存在文件。
**根因：** 错误分类过粗——把"无法确认文件状态"误判为"文件不存在"。
**影响：** 用模型持有的旧内容（stale revision）原子覆盖远端新内容，不可逆数据丢失；
remote_read 同样把权限错误误报为 not_found（误导诊断）。
**建议修复：** `SshError::Sftp` 携带 SFTP 状态码时仅 `Status::NoSuchFile` 返回 true，
其余返回 false（让调用方按其他错误处理）。
**建议测试：** 用 fake SFTP 返回权限错误/连接错误，断言 remote_write 返回失败而非覆盖。

---

## ISSUE-003 — edit 失败诊断输出无上限 + 多遍 O(N·L) 扫描（模型上下文/CPU DoS）

**功能：** F10 内置工具（edit）
**严重程度：** High
**类型：** Boundary / Performance
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/tool/edit.rs:1186-1202`（`locate_context` 先整行
append 再检查 `MAX_CTX_CHARS`，超长单行直接突破 400 字符边界）、`:827-911`
（`diagnose_no_match` 对每个窗口×每行做字符串比较）、`:1025-1043`（每次 NoMatch 先跑
`locate_lenient` 两个 O(N·L) 定位 + diagnose + locate_context）、`files.rs:543-605`
（`failed_outcome` 拼装 error 无字节上限）
**当前行为：** 对 ≤64MiB 文件（`MAX_SNAPSHOT_BYTES`），一次 edit 未命中可产出数 MiB 的
诊断输出进入模型上下文；对海量行文件窗口扫描在 `spawn_blocking` 池里跑数秒到数十秒。
**触发条件：** 大文件（单超长行或海量行）上 edit old_text 未命中。
**根因：** 错误路径缺少行内截断与总字节上限，复杂度未受文件大小约束；`MAX_REPLACEMENTS=256`
逐条放大成本。
**影响：** 单次失败 edit 消耗巨量 token/阻塞工具线程池，影响后续工具执行。
**建议修复：** `locate_context` 先按字节截断再处理；`diagnose_no_match` 限制诊断行数与
每行长度（如 ≤20 行 × ≤200 字符）；`failed_outcome` 加总预算截断。
**建议测试：** 构造 100 万行文件 + 不匹配 old_text，断言诊断输出 ≤ 固定预算且耗时受控。

---

## ISSUE-004 — Windows 大小写不敏感文件系统下资源锁大小写敏感 → 同一文件并发写可静默丢内容

**功能：** F09 工具执行/调度 + F10 edit/write
**严重程度：** High
**类型：** Concurrency / Data Loss
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/tool/mod.rs:762-784`（`resolve_lock_path` 注释明确
排除大小写归一）、`crates/tpi-capabilities/src/tool/scheduler.rs:121-140`
（`scopes_conflict` 用 `==`/`starts_with` 大小写敏感比较）
**当前行为：** 对同一物理文件 `C:\ws\main.rs` 与 `C:\ws\Main.rs`（Windows 同文件），
调度器生成两把不同的资源锁、`SnapshotStore` 存两个 key。两笔写落在同一 wave，
经 `spawn_blocking` 真正并行执行。
**触发条件：** 同一批里模型用不同大小写路径对同一文件发两次写（如先 `read src/Main.rs`
后 `edit src/main.rs`）；或 `list SRC/` 与 `edit src/a.rs` 判定无冲突。
**根因：** 锁身份用词法字符串而非 canonicalize + 统一大小写。提交阶段二次 revision 校验
只挡"先后"竞态，挡不住两笔都通过预检后的并发 `ReplaceFileW`——后完成者覆盖先完成者，
两者都报 succeeded，先写内容静默丢失。
**影响：** 静默数据丢失（写入者双方都认为成功）。
**建议修复：** 在 Windows 上把锁键与快照键统一 `to_lowercase()`（或 canonicalize 后再取键）。
**建议测试：** 构造 `write src/main.rs` 与 `write src/MAIN.rs` 同一 wave，断言被调度为
串行且第二次带 revision 冲突反馈（或至少不同 wave）。

---

## ISSUE-005 — ManagedProcess registry 永不淘汰终态进程 → 会话内内存无界增长

**功能：** F12 Managed Process
**严重程度：** High
**类型：** Resource Leak / Lifecycle
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/process/managed.rs:248-295`（`insert` 只检查活跃数）、
`:348-354`（`mark_consumed` 是 no-op，注释承认 P1 未实现）、`:289-295`（每个进程 tail
保留 64KiB + command + env 全量快照）
**当前行为：** 进程结束后 `processes`/`order`/`cancels` 永不删除；`MAX_MANAGED_PROCESSES=16`
只限制**并发活跃**数，不限制历史总量。每个终态进程永久留存 tail ≤64KiB + 完整 env 快照。
**触发条件：** 长会话中模型反复 `bash background=true`（dev server、watcher、多次构建）。
**根因：** 注释明确"先实现为 no-op，P6 再精化"。
**影响：** 10 小时级会话内存持续增长；`processes_snapshot()` 注入 context 的文本也随之增长。
**建议修复：** 实现 `mark_consumed` 语义：终态进程按 LRU/数量上限（如保留最近 32 条）淘汰，
淘汰时释放 env/tail。
**建议测试：** 启动 50 个终态进程，断言 registry 内部条目数有上限。

---

## ISSUE-006 — MCP reader task 无视取消 token，server 衍生孙进程持管道时 shutdown 永久卡死

**功能：** F11 MCP 集成
**严重程度：** High
**类型：** Lifecycle / Concurrency
**置信度：** Confirmed（挂起概率取决于 server 是否衍生孙进程）
**位置：** `crates/tpi-capabilities/src/mcp/client.rs:100-124`（两个 reader task 忽略
取消 token）、`:301-306`（shutdown）
**调用链：**
```text
McpManager::shutdown_all → McpClient::shutdown
→ request("shutdown") → child.kill()（只杀直接子进程）
→ child.wait() → reader_supervisor.shutdown()
→ token.cancel() 后 tracker.wait()：等待阻塞在 next_line() 的 reader
→ 孙进程仍持 stdout/stderr 管道写端 → 管道永不 EOF → wait() 永不返回
```
**当前行为：** reader 任务体 `while let Ok(Some(line)) = reader.next_line().await` 无任何
取消点；`Supervisor::shutdown` 先 cancel 再 `tracker.wait()`。Windows 上 `child.kill()`
（TerminateProcess）不传播到进程树，孙进程持有管道句柄时 reader 永不退出。
**触发条件：** MCP server（node 系/language server 常见）spawn 了继承 stdout/stderr 的
孙进程，且该孙进程在 kill 后仍存活。
**根因：** P2-07"join reader 不留无主 task"的目标与实现矛盾：取消 token 只能终止"自己会
检查取消点"的任务，阻塞在不可取消的 `next_line()` 上的任务无法被终止。
**影响：** 应用退出路径（`src/app/mod.rs:1635`）永久挂起，用户无法退出。
**建议修复：** 在 tokio 中无法强制中断阻塞在同步 BufRead 上的任务；改为 reader 任务
select 取消 token 与 `read_line`（改用 tokio::io::AsyncBufReadExt::read_line + select），
或 shutdown 时先 close stdin/stdout 管道让 reader 读到 EOF，并为 `tracker.wait()` 加
超时兜底（超时后放弃 join 并告警）。
**建议测试：** 集成测试：server 衍生不退出孙进程，断言 `shutdown_all` 在有限时间内返回
（或 reader_tracked 归零）。

---

## ISSUE-007 — remote baseline 捕获无超时、无取消，工具 timeout_ms 契约失效

**功能：** F13 Remote/SSH
**严重程度：** High
**类型：** Error Handling / Boundary
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/remote/executor.rs:87-95`（调用点）、`:300-318`
（`client.exec(&capture, None, &empty, None)` 无 cancel 无 timeout）
**当前行为：** `capture_remote_baseline` 在带超时的主执行**之前**裸 `.await`，内部
`client.exec` 无 cancel、无 deadline。对比本地 `capture_baseline`
（`tool/command.rs:548-558`）显式给了超时。
**触发条件：** 首次远程 bash + SSH 连接半死（网络分区/远端 sshd 挂起），`channel.wait()`
无限等待。
**根因：** 工具级 `timeout_ms` 只包裹主 exec，不覆盖其前的 baseline 捕获。
**影响：** remote_bash 无限挂起，且执行期间持有 client 锁，后续所有远程命令排队。
**建议修复：** 给 `capture_remote_baseline` 包 `tokio::time::timeout`（沿用
`DEFAULT_TIMEOUT_MS`）与 cancel。
**建议测试：** fake ssh 挂起 baseline exec，断言 remote_bash 在超时内返回失败。

---

## ISSUE-008 — 远端命令输出/远端文件读取无大小上限，可进程级 OOM

**功能：** F13 Remote/SSH
**严重程度：** High
**类型：** Boundary / Resource
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/remote/ssh.rs:444-446`（`stdout.extend_from_slice` 无上限）、`:476-481`（`read_to_end` 全量读入）
**当前行为：** 远端 exec 的 stdout/stderr 逐帧累积到 Vec 无任何 byte 上限；远端文件
`read_to_end` 全量读入内存。本地路径有 `OUTPUT_BUDGET`(24KiB)/`MAX_OUTPUT_BUDGET`(16MiB)
兜底，远端完全没有。
**触发条件：** `bash` 在远端跑 `cat /dev/urandom`/`dd`；`read` 读巨型远端文件。
**根因：** transport 层无界分配。
**影响：** 进程 OOM 拖垮整个 agent。
**建议修复：** 远端 exec/read_file 增加与本地一致的 16MiB 硬上限（超限截断并标记 truncated）。
**建议测试：** fake ssh 持续输出大帧，断言读取在预算处截断。

---

## ISSUE-009 — provider 确定性 4xx/认证错误被当瞬时错误重试（最多 ~100 次请求）

**功能：** F07 Provider 适配与流
**严重程度：** High（UX/费用影响）
**类型：** Error Handling / Design
**置信度：** Confirmed
**位置：** `crates/tpi-agent/src/provider/openai_compat.rs:341-368`（重试分支只排除
"cancelled"）、`:564-575`（`classify_error` 把非 429 4xx 归为 Connection）、
`crates/tpi-agent/src/agent/mod.rs:1299-1305`（`is_transient_transport_error` 含 Connection）
**调用链：**
```text
请求 400（确定性坏请求）→ send_once 返回 Err("http 400: ...")
→ stream() 重试循环：非 cancelled 全部按传输失败退避重试（10 次）
→ 耗尽后 classify_error → ProviderError::Connection
→ agent 层 is_transient_transport_error(Connection)=true → turn 级重启（10 次）
→ 每次重启 = 全新 provider 请求（每请求内部再重试 10 次）→ 最多 ~100 次请求
```
**当前行为：** 401/403（认证）与 400（请求格式/内容策略）都被指数退避重试，
总量预算 360 秒。
**期望行为：** 认证错误（Auth）与协议类 4xx 应立即返回，不重试。
**触发条件：** API key 过期、请求内容被 provider 拒绝、参数错误——全部是确定性错误。
**根因：** 重试策略按"是否 cancelled"一刀切，未按错误类别区分确定性与瞬时错误。
**影响：** 用户看到长达数分钟的假"重试"（实际请求必失败），浪费 token/配额，诊断延迟。
**建议修复：** 重试分支对 `Auth` 与 4xx（除 429）直接返回；`classify_error` 增加
Http(4xx) 分类并让 `is_transient_transport_error` 排除它。
**建议测试：** fake server 返回 400，断言 provider.stream 仅发起 1 次请求并返回
非 Connection 错误。

---

# Medium

## ISSUE-010 — `process cancel` 3 秒未确认终态仍宣称 process_tree_terminated

**功能：** F12 Managed Process
**严重程度：** Medium
**类型：** Error Handling / Design
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/tool/process.rs:206-232`
**当前行为：** `registry.cancel(id)` 后 `wait_process(..., 3s)`；超时返回当前 `Running` 状态
时仍走 `Some(state)` 分支输出 `status: cancelled ... effect: process_tree_terminated`。
注释声称"无法确认时如实 Unknown"，但该分支从未产生 Unknown。
**触发条件：** 取消拒绝快速终止的进程树（子进程无响应）且 3 秒内状态未落库。
**根因：** 未区分"取消请求已发出"与"终止已确认"。
**影响：** 模型收到错误的"已终止"事实，可能基于错误状态继续操作。
**建议修复：** `wait_process` 超时且状态非终态时输出 `effect: unknown` / 明确"仍在终止中"。
**建议测试：** fake 进程树 3 秒内不进入 Cancelled，断言取消结果标记 unknown。

---

## ISSUE-011 — ToolRegistry overlay 覆盖同名 root 工具时 list/descriptors/get 视图不一致

**功能：** F10 工具注册表
**严重程度：** Medium
**类型：** Contract Mismatch
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/tool/registry.rs:241-246`（get 先查 overlay）、
`:249-259`（list 省略同名 overlay）、`:262-275`（descriptors 忽略 overlay）
**当前行为：** `runtime_inspect`（走 descriptors）展示 root 工具的 schema/description，
但执行（经 get）用 overlay 工具。模型按 root schema 构造参数可能与 overlay 行为不匹配；
`check_registry_invariants`（遍历 list）对被遮蔽的 overlay 工具永不校验。
**触发条件：** MCP server 注册与内置工具同名的工具（README2 支持路径）。
**根因：** list/descriptors 以 root 为唯一事实源，与 get 的"overlay 优先"语义矛盾。
**影响：** 模型被展示信息误导，参数错配；overlay 工具逃过不变量校验。
**建议修复：** 统一三者的可见性语义（都 overlay 优先），或禁止覆盖内置工具名。
**建议测试：** overlay 覆盖 `read` 后，断言 descriptors 与 get 解析到同一实现。

---

## ISSUE-012 — edit 在 revision 过期但 old_text 仍唯一匹配时宽松应用（设计风险）

**功能：** F10 edit
**严重程度：** Medium
**类型：** Design（Silent Data Risk）
**置信度：** Highly Likely
**位置：** `crates/tpi-capabilities/src/tool/edit.rs:1077-1082`（stale 但匹配 → lenient apply）、`:989-1055`（宽松定位 Tier2/3）
**当前行为：** 模型基于 revision R1 读取的内容构造 old_text；文件已被外部改成 R2。
只要 old_text 在 R2 中仍唯一匹配（含 trailing/缩进宽容匹配），编辑照常应用，
`previous_revision` 直接给 R2。
**触发条件：** 模型读完 → 外部工具（git/fmt/IDE）改文件 → 模型 edit（未重新 read）。
**根因：** 刻意设计（功能注释"§修复 #1"），但"基于过期快照构造的 new_text + 外部并发修改"
存在覆盖用户意图区域的风险——宽容匹配只保证 old_text 命中，不保证模型对周边状态理解有效。
**影响：** 静默覆盖外部修改的相邻区域（write 路径无此宽松，edit 独有）。
**建议修复：** 至少记录/提示 stale 宽松应用（session metadata 标记），或要求模型在
stale 时先 read。
**建议测试：** 构造 stale revision + 唯一匹配，断言结果带 stale 标记且可审计。

---

## ISSUE-013 — recover_after_failure 返回 Ok 时 temp 文件泄漏在 workspace

**功能：** F10 edit
**严重程度：** Medium
**类型：** Resource Leak
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/tool/edit.rs:1430-1443`（`commit_edit` 只在
`CommitFailed` 删 temp）、`:1544-1550`（`recover_after_failure` 的 Ok 分支）
**当前行为：** 正常成功路径删 temp；"ReplaceFileW 返回失败但 target 已是新内容"的恢复 Ok
分支既不删 temp（非 CommitFailed）也不删 backup → workspace 残留 `.tpi-edit-*.tmp`。
**触发条件：** ReplaceFileW 因备份路径权限/共享违规报告失败但主替换已完成（Windows 上
error 与结果不一致的情形）。
**根因：** temp 清理条件未覆盖 Ok 恢复分支（CommitRecoveryFailed 保留 temp 是刻意证据设计，
Ok 分支则是遗漏）。
**建议修复：** 在 recover_after_failure 的 Ok 分支清理 temp（backup 按策略处理）。
**建议测试：** fake ReplaceFileW 失败但文件已更新，断言 temp 被清理。

---

## ISSUE-014 — 帮助文案宣称的 Ctrl+C 语义在 TUI 中不存在

**功能：** F15 TUI 交互
**严重程度：** Medium
**类型：** UX / Contract Mismatch
**置信度：** Confirmed
**位置：** `src/app/mod.rs:650`（横幅"空闲 Ctrl+C 连按两次退出"）、`:1799`（帮助
"Ctrl+C 有选区复制/运行中取消/空闲连按两次退出"）、`crates/tpi-ui-types/src/keymap.rs:332`
（`ctrl+c` → `KeyAction::Copy`）
**当前行为：** keymap 中 Ctrl+C 只绑定 Copy；reducer 无选区时静默忽略；全代码不存在
"连按两次退出"或"运行中取消"逻辑。`spawn_ctrl_c_handler`（app/mod.rs:2698）只处理
`tokio::signal::ctrl_c()`，raw mode 下 Windows 将 Ctrl+C 作为按键上报，永不命中
（模块注释也承认"只覆盖非 raw mode 路径"）。
**触发条件：** 用户在 TUI 内按屏上提示按两次 Ctrl+C 期望退出 / 运行中按一次期望取消 →
无反应。
**根因：** 文案是旧版"复合语义"（copy_or_interrupt）遗留，键位迁移后文本未同步。
**建议修复：** 横幅与 /help 改为实际语义：复制 Ctrl+C、退出 Ctrl+D、取消 Esc。
**建议测试：** 人工验证（对照 /help 文本与 keymap 绑定）。

---

## ISSUE-015 — 运行中 Ctrl+D 被静默丢弃

**功能：** F15 TUI 交互
**严重程度：** Medium
**类型：** UX / Control Flow
**置信度：** Confirmed
**位置：** `src/app/mod.rs:2425-2432`（run 内效果循环把 `UiEffect::Quit` 归入 `{}`，
注释"run 中不会产生"为假）、`crates/tpi-tui/src/reducer.rs:676-679`（reducer 对 QuitApp
无条件产出 Quit，不检查 running）
**当前行为：** 运行中按 Ctrl+D 产生的 Quit 效果被丢弃，既不休止也不提示。
**触发条件：** 长 run 期间按 Ctrl+D 想退出 → 无反应（必须先 Esc 取消再 Ctrl+D）。
**根因：** reducer 与 run 主循环对 Quit 语义的约定不一致。
**建议修复：** 二选一：reducer 在 running 时忽略 QuitApp（提示"先 Esc 取消"），或
run 主循环处理 Quit（先取消 run，run 结束后退出）。
**建议测试：** reducer 单测：running=true 时 QuitApp 不产出 Quit 或产出带提示的效果。

---

## ISSUE-016 — 搜索打开时 Ctrl+字母组合被当普通字符插入搜索词

**功能：** F15 TUI 交互
**严重程度：** Medium
**类型：** UX / Control Flow
**置信度：** Confirmed
**位置：** `crates/tpi-tui/src/reducer.rs:329-348`（`handle_search_key` 的 Char 分支只特判
ctrl+u/ctrl+f）
**当前行为：** 搜索打开期间，Ctrl+C/Ctrl+Z/Ctrl+D/Ctrl+A/Ctrl+W 等落入 else 分支把字符
原样 push 进 search query（SearchState 路由在 keymap 之前拦截，绕过修饰位规范化）。
**触发条件：** Ctrl+F 打开搜索后按 Ctrl+C（想复制）→ 搜索词多出 "c"。
**根因：** 该分支未检查非字符修饰位。
**影响：** 搜索期间全部快捷键损坏成字符串。
**建议修复：** Char 分支先检查 `key.modifiers` 是否含 CONTROL/ALT，含则忽略或走对应动作。
**建议测试：** reducer 单测：搜索打开 + ctrl+c → 不插入字符。

---

## ISSUE-017 — 提交时整段 trim() 静默吃掉首行缩进/首尾空白

**功能：** F15 TUI 输入
**严重程度：** Medium
**类型：** UX / Data Flow
**置信度：** Confirmed
**位置：** `crates/tpi-tui/src/editor.rs:388`
**当前行为：** `Editor::submit` 执行 `self.text.trim().to_string()`，多行粘贴以首行缩进开头
时首行前导空白与末行尾部空白被整体剥离。
**触发条件：** 粘贴缩进代码块/续行命令后 Enter。
**根因：** 单行命令时代遗留的 trim 未按多行语义改为"去尾换行、保留行内空白"。
**影响：** 模型收到的指令语义被改变（缩进敏感内容）。
**建议修复：** 只 `trim_end_matches('\n')`（或 trim_end 换行符），保留行内空白。
**建议测试：** editor 单测：首行缩进内容 submit 后保留缩进。

---

## ISSUE-018 — request_input 自定义编辑（EditingCustom）中 Ctrl 组合键被当字面字符

**功能：** F15 TUI 交互
**严重程度：** Medium
**类型：** UX
**置信度：** Confirmed
**位置：** `crates/tpi-tui/src/reducer.rs:84-88`
**当前行为：** `QuestionMode::EditingCustom` 的 Char 分支不检查 `key.modifiers`，
Ctrl+C/Ctrl+Z/Ctrl+A 等把字母插入 custom_input。
**触发条件：** 自定义回答编辑中按 Ctrl+C 复制/取消 → 输入串多出字母。
**根因：** 模态拦截优先于 keymap，char 分支缺少修饰位过滤（与 ISSUE-016 同根因不同位置）。
**建议修复：** 同上，Char 分支检查 CONTROL/ALT 修饰位。
**建议测试：** reducer 单测。

---

## ISSUE-019 — 配置默认值与文档自相矛盾（默认主题/折叠行数）

**功能：** F01 Config
**严重程度：** Medium
**类型：** Design / Contract Mismatch
**置信度：** Confirmed
**位置：** `crates/tpi-config/src/config.rs:556`（`ui_theme` 默认 `"onedarkpro"`）、
`:184-185`（文档声称"默认 omp"）、`:557`（`ui_collapsed_lines` 默认 0）、`:190-192`
（文档声称"默认 10"）、`:357`（test_config 用 10）
**当前行为：** 无 `[ui]` 配置的用户得到 onedarkpro 主题、0 折叠行；文档与 /settings 展示
不一致；未知主题回退 omp 与空配置默认 onedarkpro 两条路径默认不同。
**影响：** 用户所见与文档不符，折叠行为与预期不符。
**建议修复：** 统一默认值（建议 onedarkpro 为实际默认并改文档；collapsed_lines 明确 0 或
10 择一并同步三处）。
**建议测试：** config 单测断言默认值与文档一致（把文档断言写进测试）。

---

## ISSUE-020 — 配置字段 `shell.kind` 存在但运行时从不读取

**功能：** F01 Config
**严重程度：** Medium
**类型：** Dead Config / Contract Mismatch
**置信度：** Confirmed
**位置：** `crates/tpi-config/src/config.rs:94-98`（`ShellFile.kind`）、`:598-599`（merge 保留）
**当前行为：** 全仓除解析/合并/测试外无消费点；运行时只用 `shell.path`
（`locate_git_bash` 优先配置路径）。用户设置 `[shell] kind = "git-bash"` 无任何效果。
**影响：** "功能存在但实际不工作"（配置欺骗用户）。
**建议修复：** 接线（kind 影响 shell 选择）或移除字段并文档说明。
**建议测试：** 设置 kind 后断言 shell 选择行为改变（或移除后断言不再解析）。

---

## ISSUE-021 — MCP stdout 用无界 channel

**功能：** F11 MCP
**严重程度：** Medium
**类型：** Resource Leak
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/mcp/client.rs:111-124`
**当前行为：** `unbounded_channel` 缓冲 stdout 行；reader 无条件 send，`request()` 只在
需要响应时 recv。server 主动发通知/日志刷 stdout 且长时间不被消费时消息无限堆积。
（对比：TUI 输出通道明确用有界 channel + try_send，tool/mod.rs:544。）
**建议修复：** 改有界 channel + try_send（丢弃最旧或丢弃新帧并计数）。
**建议测试：** 长时间不 request + server 高频刷 stdout，断言内存有界。

---

## ISSUE-022 — 相对 cwd 在远端解析到 SSH 登录主目录而非远端 session cwd

**功能：** F13 Remote
**严重程度：** Medium
**类型：** Contract Mismatch
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/remote/executor.rs:61-64`
**当前行为：** `args.cwd` 传相对路径时字面拼进 `cd 'src';` 前缀；远端每次 fresh exec
channel 初始 cwd 是 SSH 登录主目录，`cd 'src'` 相对 `~` 解析，与本地行为
（基于 session cwd，`tool/command.rs:177-181`）不一致。且 `cd` 失败时 bash 默认继续，
命令在错误目录执行。
**影响：** Local/Remote 共享同一 BashArgs 契约（§35/§36）但相对 cwd 语义两端不一致。
**建议修复：** 相对 cwd 先相对远端 session cwd 解析（或显式 `cd ~/... ` 前缀 + 失败即中止）。
**建议测试：** remote bash 集成：session cwd = /repo，cwd="src" 断言实际执行目录 = /repo/src。

---

## ISSUE-023 — 远端 temp 文件在 rename 失败/连接断开时泄漏

**功能：** F13 Remote
**严重程度：** Medium
**类型：** Resource Leak
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/remote/ssh.rs:485-507`（`{path}.tpi-tmp-{uuid}` → rename）
**当前行为：** write_file 先创建 temp，rename 失败/上传中断后无清理路径，远端累积
`.tpi-tmp-*` 垃圾。
**建议修复：** 失败路径尝试删除 temp（best-effort）；文档说明远端 FS 不支持 rename 的降级。
**建议测试：** fake sftp rename 失败，断言 temp 被清理。

---

## ISSUE-024 — remote_search 远端执行无 deadline

**功能：** F13 Remote
**严重程度：** Medium
**类型：** Boundary
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/remote/traverse.rs:182-190`
**当前行为：** `client.exec(&cmd, None, ...)` 只带 cancel 无超时（对比 remote_list/glob 有
30s deadline）。病态正则/巨型目录树可无限阻塞 agent 循环且烧远端 CPU。
**建议修复：** 加与本地一致的 SCAN_DEADLINE。
**建议测试：** fake ssh 挂起，断言超时返回 stop_reason=deadline。

---

## ISSUE-025 — host stderr reader 是 detached tokio::spawn，违反自身 ADR-006

**功能：** F12 Managed Process
**严重程度：** Medium
**类型：** Lifecycle（自相矛盾）
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/process/mod.rs:176-184`、`managed.rs:543-551`
**当前行为：** process-host 的 stderr 读取用裸 `tokio::spawn`，不受 Supervisor 跟踪、无人
join；任务阻塞在 `next_line()` 直到 host 死亡。McpClient 同款 reader 用了 supervisor，
两处实现不一致。
**建议修复：** 与 McpClient 一致，用 Supervisor 跟踪；退出路径 join。
**建议测试：** 现有 leak 测试补 host stderr reader 场景。

---

## ISSUE-026 — 大粘贴 >256KiB 提交时被静默截断，与"不受截断"承诺相悖

**功能：** F15 TUI 输入
**严重程度：** Medium
**类型：** UX / Data Loss（部分）
**置信度：** Confirmed
**位置：** `crates/tpi-tui/src/state.rs:33-37`（占位符承诺"不受 MAX_INPUT_BYTES 截断"）、
`:100-105`（`push_pending` 无条件 truncate 到 256KiB 并插入截断标记）
**当前行为：** 占位符机制只豁免编辑/上屏阶段；提交时仍把展开全文截断到 256KiB 且用户
无感知（无提示）。
**建议修复：** 提交超限时显式提示用户（或按行截断并展示行数差异）。
**建议测试：** state 单测：>256KiB 粘贴提交后断言截断标记存在且用户可见。

---

## ISSUE-027 — 每新增一条 transcript 全量重建 wrap 缓存（O(n²) 累计）

**功能：** F15 TUI 渲染
**严重程度：** Medium
**类型：** Performance
**置信度：** Confirmed
**位置：** `crates/tpi-tui/src/lib.rs:321-325`（transcript_revision 变化 → wrap_cache.clear()）、
`model.rs:812-814/936/918`（每次 push_line/finish_tool 都 bump revision）
**当前行为：** 工具生命周期事件与系统行都 bump revision，使历史 entry 全部重 wrap；
会话越长每帧成本越高。`render_cache.rs` 的 entry 级缓存**从未被引用**（死代码）。
**影响：** 长会话频繁工具调用时渲染变慢（O(n²) 累计）。
**建议修复：** 接入 render_cache 的 entry 级失效，或只在 wrap 宽度变化时清缓存。
**建议测试：** 性能基准：追加 2000 条消息后单次 draw 时间不随历史增长明显增加。

---

## ISSUE-028 — eval 超时路径可能把结果整个吞掉

**功能：** F17 Eval
**严重程度：** Medium
**类型：** Error Handling / Boundary
**置信度：** Highly Likely
**位置：** `src/eval/mod.rs:529-543`（超时→cancel→5s 兜底后丢弃 run_fut）、`:547-548`
（`read_events_with_ts` 严格用 read_envelopes）
**当前行为：** 若 run 取消后 5s 内未结束，run_fut 被 drop，可能留下一行半写入的 session
尾；随后严格读取直接 Err，`run_task` 传播 Err——超时评测不落任何结果文件。
**建议修复：** 超时路径降级：不依赖 session 严格读取，直接写超时结果（wall_time 已过、
verify 不跑）。
**建议测试：** 用挂起 fake provider + 极短 timeout，断言结果目录仍生成超时结果文件。

---

## ISSUE-029 — question 模态 Done 状态空框闪烁

**功能：** F15 TUI 交互
**严重程度：** Medium
**类型：** UX
**置信度：** Confirmed
**位置：** `crates/tpi-tui/src/reducer.rs:78-82`、`lib.rs:3060`（QuestionMode::Done 渲染空内容）
**当前行为：** 单选数字快选/Enter 提交后模态保持 Some 且 mode=Done，渲染为空框直到下一次
按键（或 app 异步清除）。
**建议修复：** Done 状态立即渲染"已提交"反馈或直接清除模态。
**建议测试：** reducer 单测 + 人工验证。

---

## ISSUE-030 — 搜索词清空后不恢复 follow

**功能：** F15 TUI 滚动
**严重程度：** Medium
**类型：** UX
**置信度：** Confirmed
**位置：** `crates/tpi-tui/src/model.rs:1291-1300`
**当前行为：** `update_search_query("")` 清空命中后不触碰 scroll_mode；用户 Ctrl+F → 键入
→ 删空 → 关闭搜索，视口仍停在旧命中位置。
**建议修复：** 空 query 时恢复 follow_tail()。
**建议测试：** model 单测：删空 query 后 scroll_mode 恢复 Follow。

---

## ISSUE-031 — cancel 与 MSG_EXIT 竞态：取消后的进程可能被记录为 Exited 而非 Cancelled

**功能：** F12 Managed Process
**严重程度：** Medium
**类型：** Concurrency / State
**置信度：** Confirmed（窗口窄）
**位置：** `crates/tpi-capabilities/src/process/managed.rs:627-648`
**当前行为：** select 中 cancel 分支触发 terminate 后，读分支可能收到 host 终止前已发的
MSG_EXIT 帧，状态被迁移为 Exited；终态补丁因"已是 terminal"跳过。
**影响：** 用户取消的进程显示为 exited（状态分类错误，影响模型判断）。
**建议修复：** 取消请求后忽略后续 Exited 迁移（或记录 cancelled_by_user 优先）。
**建议测试：** 竞态注入测试。

---

## ISSUE-032 — shutdown_all 串行且每个 server 最多 30s

**功能：** F11 MCP
**严重程度：** Medium（慢速路径，与 ISSUE-006 叠加则无限）
**类型：** Lifecycle / Performance
**置信度：** Confirmed
**位置：** `crates/tpi-capabilities/src/mcp/manager.rs:113-120`
**当前行为：** 逐 server `lock().await` → `shutdown()`（内部 request 默认 30s 超时）。
N 个挂死 server 时退出路径最长 N×30s。
**建议修复：** 并行 shutdown + 整体超时。
**建议测试：** 多 server 挂死场景断言退出有界。

---

## ISSUE-033 — 远端二次校验与 rename 间存在 TOCTOU 窗口

**功能：** F13 Remote
**严重程度：** Medium
**类型：** Concurrency
**置信度：** Suspicious
**位置：** `crates/tpi-capabilities/src/remote/files.rs:194-227`（二次校验）、`:229`（write_file）
**当前行为：** remote_edit 校验 revision 后 temp+rename，校验与 rename 之间另一 writer 修改
会被静默覆盖（上传耗时放大窗口）。本地 edit 有 journal/backup 语义，远端无 CAS。
**建议修复：** 接受现状则文档化降级语义；否则 rename 前再校验一次（缩小窗口）。
**建议测试：** 高并发写入远端场景验证。

---

# Low

## ISSUE-034 — web.rs token 走 query string + localStorage 持久化
**位置：** `src/web.rs:355-362`、`:674-677`。token 进入浏览器历史/日志并被 localStorage
持久化。认证本身无绕过（`/api/*` 全校验），属安全卫生问题（设计已声明"粗糙"）。Medium-Low。

## ISSUE-035 — web.rs panic 占位 run_id=0 与第一个真实 run 冲突
**位置：** `src/web.rs:502`（next_run 初值 0）、`:551-557`（RunGuard unwound 写入 run_id=0）。
服务启动后第一条消息 panic 时，轮询端可能把错误误认成自己的结果。建议 next_run 从 1 开始。

## ISSUE-036 — skills read_reference 无路径校验
**位置：** `crates/tpi-capabilities/src/skills/manager.rs:122`。`reference` 直接拼进
`references/` 后，`"../../config"` 可越界读取。当前无外部输入到达，但公开 API 未设防。
建议校验组件不允许 `..`（与 artifact 校验一致）。

## ISSUE-037 — skills frontmatter 在第一个冒号处截断
**位置：** `crates/tpi-capabilities/src/skills/parser.rs:113`（`split_once(':')`）。
`description: "foo: bar"` 解析为 "foo"。建议 split 一次（`splitn(2, ':')`）。

## ISSUE-038 — RemoteHost Debug 可能泄露 password
**位置：** `crates/tpi-capabilities/src/remote/ssh.rs:61` + `remote/mod.rs:52-62`。当前
password 恒为 None，但字段一旦接入，Debug 输出会把整个 RemoteHost（含 password）打进日志。
建议自定义 Debug 打码。

## ISSUE-039 — 远端 env 变量名未引用直接拼进 export 前缀
**位置：** `remote/executor.rs:70-72`、`remote/ssh.rs:409-410`。`export {k}=...` 的 k 未
shell_quote。模型已具完整 shell 权限，属防御纵深缺口（低价值）。

## ISSUE-040 — host pump 线程只等 3 秒就发 MSG_EXIT，派生进程输出丢失/帧撕裂
**位置：** `process/host.rs:94-103`。目标进程退出但派生进程仍持管道时 pump 不 EOF，3 秒后
放弃 join，窗口期内输出帧丢失或并发写。设计取舍，但应记录。

## ISSUE-041 — glob 的 MAX_SCAN_FILES/MAX_SCAN_BYTES 只统计命中文件
**位置：** `search.rs:286-301`（先过滤再计数，与 list 的 :198-203 语义不一致）。巨大仓库
匹配 0 个时 scan_limit 永不触发（有 deadline 兜底，非致命）。

## ISSUE-042 — action_key 数值字面量规范化不一致
**位置：** `scheduler.rs:282-289`。`1000`（int）与 `1000.0`/`1e3`（float）产生不同
action_key → 无进展检测误判（仅判定精度影响）。

## ISSUE-043 — msys_path_to_windows 对 `/c`（根目录）不转换
**位置：** `command.rs:579-591`（要求 len≥3）。cygpath 缺失 + `cd /c` 时 session cwd 被写成
`/c`，Windows 解析成 `C:\c`（不存在）→ 后续 bash 全部 spawn 失败。

## ISSUE-044 — read start_line=0 被静默当作 1
**位置：** `edit.rs:1784`（`saturating_sub(1)`）、`files.rs:123-130`（未拒绝 0）。模型用 0
翻页会与上次窗口重叠/死循环。

## ISSUE-045 — 多行编辑上下移动光标列漂移（半角片假名浊音点）
**位置：** `editor.rs:177`（UnicodeWidthStr::width）vs `:184`（char_cell_width）。U+FF9E/
FF9F 两口径不一致，↑/↓ 后列漂移。

## ISSUE-046 — FastPasteEnterGuard 把"打字后 30ms 内回车"改写为换行
**位置：** `paste.rs:39-41,64-77`。快速输入 + Enter（<30ms）在无 bracketed-paste 终端被
误判为粘贴流插入换行。Suspicious。

## ISSUE-047 — inline 模式上滚时 scrollback 内容可能重复绘制
**位置：** `lib.rs:889-911`。Locked 使 window_start < committed 时已提交到 scrollback 的
行在活动区重绘。Suspicious。

## ISSUE-048 — remote bash stdout/stderr 各自分配完整 24KiB 预算
**位置：** `remote/executor.rs:233-234`。输出可达 2×24KiB（本地共享预算 + stderr 保底）。

## ISSUE-049 — remote_glob 截断时仍报告 stop_reason="complete"
**位置：** `remote/traverse.rs:277`。cancel/deadline/scan_limit 截断后仍硬编码 complete
（remote_list 有正确逻辑）。

## ISSUE-050 — workspace.connection 字段是死状态
**位置：** `workspace/mod.rs:27`。全库无读取方（仅 Debug），SshClient 从不写它。属死代码。

## ISSUE-051 — FocusStack 组件是死代码
**位置：** `tpi-tui/src/focus.rs:1-160`。设计文档声称的焦点栈从未接入 reducer/app；焦点
"生命周期"不变量无法集中保证（如 blocking 判定漏 menu）。

## ISSUE-052 — 队列满丢最旧消息时 footer 计数不刷新
**位置：** `state.rs:109-120`。`push_pending` 丢弃分支未调 `sync_pending_len()`（长度不变
故无实际偏差，语义遗漏）。

---

# 已核查无问题（排除项，供后续回归参考）

- **滚动跟随语义**（scroll.rs + model.rs）：Locked 模式用 ScrollAnchor 锚定，新内容到达
  不强制跳底；回到底部恢复 follow。语义正确，与"绝不强制跳底"的 UX 目标一致。
- **CJK/emoji 宽度与选区映射**（text.rs/model.rs）：wrap/截断/选区偏移逐项对齐，未发现
  宽度错位。
- **web_fetch SSRF 防护链**（web.rs:97-261）：字面 IPv6/IPv4-mapped 处理、redirect 逐跳
  校验、DNS 解析后钉死地址防 rebinding、no_proxy——完整。
- **artifact 路径穿越**：`validate_artifact_component`（禁 `/` `\` `..`）+ session 匹配，
  双层防护。
- **edit CRLF 替换边界**：raw_offset 公式与测试一致，无错位。
- **session 单写者锁 + 尾部修复 + 严格校验**：middle 损坏拒绝、尾部残片丢弃、重复
  resume 不重复合成 Interrupted（conversation.rs 测试覆盖）。
- **compaction covered 范围**：recent 边界调整到 User/Assistant 原子单元起点，replay 与
  runtime 语义一致（projector 属性测试覆盖）。
- **终端恢复路径**：restore/Drop/restore_global/panic hook 四条路径对称逆序。
- **web.rs 并发**：busy 原子 + RunGuard 兜底 + 连接信号量 + 请求有界，未发现 token 绕过。
- **edit 提交二次校验**（commit_edit 紧邻提交前重读比对 revision + 双 digest）：
  先后竞态窗口处理正确（但不覆盖 ISSUE-004 的大小写并行写）。
- **recovery 分类**（classify_effect）：FileCommit 的 target/temp/backup 三态判定与
  candidate_revision 逻辑正确。
