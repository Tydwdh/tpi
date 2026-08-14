# Managed Background Process — 架构审计（Phase P0）

对应 AGENTS.md（任务书）§54。基于当前源码逐项审计，结论供 P1 起编码使用。

## 1. 当前 foreground bash 的完整生命周期

```
command::bash (src/tool/command.rs:60)
  ├─ 参数校验（空命令 / 超长 / timeout 范围）
  ├─ 按 ActiveWorkspace.kind() 分发 → local_bash / remote_bash
  ├─ local_bash (src/tool/command.rs:91)
  │   ├─ 定位 Git Bash（shell.path → 随包 → Program Files → PATH）
  │   ├─ 构造 wrapper：set -o pipefail + 用户命令 + __tpi_status + capture nonce 段
  │   ├─ session cwd / overlay(set/unset) 快照 → RunArgs{program=bash, args, cwd, env, env_remove}
  │   ├─ (首次) capture_baseline：跑一次无 overlay 的 bash 取 env 基线
  │   ├─ ArtifactWriter::create（完整输出落盘）
  │   ├─ run_in_host (src/process/mod.rs:114)
  │   │   ├─ spawn `tpi.exe __process-host`（CREATE_NO_WINDOW，stdin/stdout/stderr piped）
  │   │   ├─ Job::create()（KILL_ON_JOB_CLOSE，禁 breakaway）→ assign host pid
  │   │   ├─ 发送 Start spec（framed：len + kind=0 + JSON{program,args,cwd,env,env_remove}）
  │   │   ├─ 读循环：MSG_OUTPUT(1) → stream_sink(UI) + artifact + bounded tail；
  │   │   │             MSG_EXIT(2) → exit_code；cancel/timeout → job.terminate(1)
  │   │   └─ 返回 HostRunOutput；drop Job → KILL_ON_JOB_CLOSE 终止整棵进程树
  │   ├─ 事务 commit：Exited 且 capture 有效 → 更新 ShellSessionState(cwd/env/version)
  │   └─ 构造 ToolOutcome（24KiB tail + stderr 保底 + artifact 引用）
  └─ remote_bash (src/remote/executor.rs:28)
      ├─ overlay 注入为 export/unset/cd 前缀 + capture 段
      ├─ client.exec()：persistent transport 连接 + fresh exec channel（§38）
      └─ 结果剥离 capture → commit ShellSessionState（cwd/env）
```

关键事实：**每次调用 = fresh shell + 独立 Job Object；调用结束（含 timeout/cancel）必然
TerminateJobObject / 关闭 job handle，进程树不存活**。这正是前几轮实验里
`sleep 300 &` / `nohup` / `Start-Process` 全部被清理的原因。

## 2. process-host 是否能支持不等待子进程退出

`src/process/host.rs`：

- host 读 Start spec → `Command::spawn(target)` → 转发 stdout/stderr → `child.wait()` → 发 MSG_EXIT。
- host 是同步进程，**必须等待 target 退出才发 MSG_EXIT 并退出**（host.rs:81-106）。
- host 自身在 Job Object 内（tpi 侧 assign）；只要 tpi 侧**不 drop Job 且持续读帧**，
  host 就一直活着、一直 wait target、持续转发输出。

结论：**支持**。ManagedProcess 的实现不需要改 host 的 wait 语义——只需要：
1. host 在 target spawn 成功后补发一条 `MSG_STARTED`（payload 可含 pid），让后台启动
   能确认"进程已真正启动"（区分 spawn 失败 Exit(-2)）；
2. tpi 侧把 Job 持有权与读帧循环放进独立 tokio task（drain task），跨工具调用存活；
3. 取消 = 该 task 内 `job.terminate(1)`。

前台 `run_in_host` 读循环需对 `MSG_STARTED` 帧做 `continue`（向后兼容，见 §11 改动清单）。

## 3. Job Object ownership 在哪一层

`src/process/mod.rs:154-159`：Job 由 `run_in_host` 局部创建；函数返回即 drop
（`Job::drop` → `CloseHandle` → `KILL_ON_JOB_CLOSE`，mod.rs:534-542）。

- ownership 目前是 **函数作用域**（单次调用）。
- ManagedProcess 要求 ownership 提升到 **registry 持有的 ManagedProcess 记录**
  （连同 cancel token、drain task handle、artifact writer 一起），直到
  exit / cancel / TPI 退出（§29 session-owned；§31 crash → KILL_ON_JOB_CLOSE 兜底）。

## 4. stdout/stderr 谁负责持续读取

当前：`run_in_host` 的读循环是函数内 await 循环，调用结束即停止读取。
ManagedProcess 需要 **drain task**（tokio::spawn）持续读帧直到 MSG_EXIT / 取消：
- 每帧 → bounded ring tail（live output，进程记录内）+ ArtifactWriter（full，落盘）；
- MSG_STARTED → Starting→Running；MSG_EXIT → Exited(code) / spawn-fail → Failed；
- 读循环永不因"模型未查询"而停止（§15：输出必须一直被消费，否则 pipe 填满阻塞程序）。

## 5. TPI crash 后 Job Object 怎么处理

`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`（mod.rs:449）+ Job::drop 关闭 handle（mod.rs:537）：
**tpi.exe 崩溃 → 所有 job handle 关闭 → 整棵进程树终止**。这天然满足 §31
（"TPI crash → ManagedProcess tree 不成为孤儿"）与 §29（session-owned 退出即终止）。
无需新增清理逻辑；但要在 ManagedProcess 文档中把这一保证写清楚。

## 6. Remote exec channel 生命周期

`src/remote/ssh.rs:221`（persistent transport）+ `exec()`（fresh channel per command）：
- channel 关闭（命令结束 / 取消 / 网络断开）后，远端进程生死由 sshd 语义决定，
  **没有 Job Object 级 guarantee**（§39：best-effort cancel，代码明确"远端进程可能仍在运行"）。
- 任务书 §34/§36：Remote v1 允许较弱 guarantee；Remote ProcessHost 是第二阶段。
- 本阶段（P1-P7）**只做 Local**；Remote ManagedProcess 留到 P8 按实际能力定义 guarantee。

## 7. ShellSessionState 如何注入

- 前台 local：`RunArgs{program: bash, args: [--noprofile --norc -c wrapped], cwd: exec_cwd,
  env: overlay_set, env_remove: overlay_unset}`（command.rs:196-203），由 process-host 在
  `Command::new(target)` 时注入（host.rs:38-50）。
- ManagedProcess 复用同一注入路径：启动时快照 `ShellSessionState{cwd, env_overlay.set/unset}`
  构造 RunArgs（§9）。**background 不注入 capture wrapper、不 commit**（§10/§44 硬不变量）：
  后台进程永不反向修改 ShellSessionState——多个 background 之间不可能发生 shell state race。

## 8. Workspace scheduler 如何看待长时间外部 mutation

`src/agent/scheduler.rs`：bash 的 ToolAccess = `WorkspaceUnknown`（scheduler.rs:73）→ 串行执行。
- ManagedProcess 启动调用（background bash）同样按 `WorkspaceUnknown` 处理（复用 BuiltinTool::Bash
  的 execution_class，无需改 scheduler）。
- 进程运行期间的 read/edit 并发：revision-bound edit 已天然防 stale overwrite（§42）；
  不解析 bash 命令猜副作用（§39）；workspace 视为 externally mutable（§40-41），
  first version 不加 epoch 机制（§41 是"建议评估"，非硬要求）。

## 9. 哪些现有类型可以复用

| 现有类型 | 复用方式 |
|---|---|
| `RunArgs`（tool/command.rs:43） | ManagedProcess 的启动规格（program/args/cwd/env/env_remove） |
| `Job`（process/mod.rs:427） | 每个 ManagedProcess 独立 Job Object（§12/§13） |
| `ArtifactWriter`（session/artifact.rs:31） | full output 落盘（§16 第二层） |
| `append_bounded`（process/mod.rs:72） | bounded live tail（§16 第一层） |
| `HostRunRequest` 的构建模式 | 复用 host spawn + Start spec 协议 |
| `ToolOutcome` / `ModelPayload` / `ToolStatus` | background 启动与 process 工具的结果构造 |
| `BuiltinTool` / `ValidatedArgs` / `ToolContext` | 新增 `process` 工具（§5 统一入口） |
| `ShellSessionState` | 启动时 cwd/env snapshot（§9） |
| `CaptureScanner` | **不复用**——background 不捕获/提交 shell 状态 |

## 10. 最小 seam（改动清单）

1. `src/process/mod.rs`：host 协议加 `MSG_STARTED`；`run_in_host` 读循环 `continue` 跳过该帧。
2. `src/process/managed.rs`（新）：`ProcessId` / `ManagedProcessState` / `ManagedProcess` /
   `ProcessRegistry` + `start_background`（spawn drain task）+ `list/status/output/wait/cancel`。
3. `src/tool/command.rs`：`BashArgs` 加 `background: bool`（默认 false，§3）；`bash()` 增加
   background 分支（仅 Local；Remote 返回 rejected 或后续实现）。
4. `src/tool/mod.rs`：`BuiltinTool::Process`（name="process"）+ schema/description/parse_args/
   execution_class（Pure——读 registry，无文件副作用）/ValidatedArgs 变体/implemented_tools。
5. `src/tool/process.rs`（新）：process 工具实现（list/status/output/wait/cancel 五个 action）。
6. `src/tool/mod.rs` `ToolContext`：加 `processes: Arc<Mutex<ProcessRegistry>>`；
   `src/agent/tool_runtime.rs` ToolRuntime 构造时创建并注入；app/tests 构造点同步。
7. `src/agent/mod.rs` `build_context`：尾部注入 managed process snapshot（§25/§26，
   system 角色，只含 active + 近期变化未消费）。
8. `src/agent/system_prompt.md`：加最少纪律（§52，两句话）。
9. TUI `/processes` overlay（§27/§28）——P7 视时间做轻量版。

## 风险与边界（承接前几轮实验结论）

- 前台 `run_in_host` 的 `MSG_STARTED` 兼容改动必须保持现有行为（测试覆盖）。
- 后台启动的 ToolStatus 用 `Succeeded`（start succeeded，§50），模型可见文本
  `status: running`（§49/§51）——不改 ToolStatus 枚举，避免波及 provider/recovery/TUI 的穷尽 match。
- 退出语义：session-owned，TPI 正常退出/崩溃即终止（§29/§31），不做 detached（§32）。
