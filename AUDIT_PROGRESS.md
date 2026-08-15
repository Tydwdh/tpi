# AUDIT_PROGRESS — TPI 功能级代码审计进度

> 审计日期：2026-08-16。范围：全 workspace（7 crates，~57k 行 Rust）。
> 方法：逐功能两轮审计（Design Review + Bug Review）+ 跨功能 Contract 审计，
> 关键发现均经调用链核实（Confirmed 表示代码路径可确认）。

## Project Map

| 功能 | 入口 | 核心模块 | 状态 | 外部依赖 |
| --- | --- | --- | --- | --- |
| CLI 入口 | src/main.rs | clap 定义 + run() | Cli | 终端 |
| 配置加载 | main::run → config::load | tpi-config/config.rs | Config | FS (toml) |
| 凭据管理 | tpi auth set/clear/status | tpi-config/auth.rs | Windows Credential Manager | Win32 |
| Session durable | Conversation/ensure_started | tpi-session/{store,protocol}.rs | SessionLog (JSONL) | FS |
| Session 恢复/修复 | --continue/--resume/repair | tpi-session/{recovery,repair,conversation}.rs | SessionLog | FS |
| Context 投影 | agent::run → build_context | tpi-agent/context/mod.rs | projection | — |
| Compaction | compact_turn | tpi-agent/agent/mod.rs + context | CompactionCommitted | provider |
| Provider 流 | provider::stream | tpi-agent/provider/openai_compat.rs | SSE | HTTP |
| Agent 循环 | agent::run | tpi-agent/agent/mod.rs | RunState | session+provider |
| 工具执行 | ToolBatchExecutor | tpi-agent/agent/tool_runtime.rs + tool/scheduler | waves/ProgressTracker | session |
| 内置工具 | 工具注册表 | tpi-capabilities/tool/*.rs | 无 | FS/shell/web |
| MCP | interactive_loop → McpManager | tpi-capabilities/mcp/*.rs | RunningServer | stdio 子进程 |
| Managed Process | bash background / process | tpi-capabilities/process/*.rs | ProcessRegistry | Windows Job |
| Remote SSH | bash/file 远端分发 | tpi-capabilities/remote/*.rs | SshClient | russh/sftp |
| Skills | refresh_global / activate_skill | tpi-capabilities/skills/*.rs | SkillManager | FS |
| TUI | interactive_loop | tpi-tui/{reducer,model,scroll,...}.rs | UiState | ratatui |
| Web 接口 | tpi serve | src/web.rs | busy/run_id | HTTP |
| Eval | tpi eval | src/eval/mod.rs | 任务目录 | provider |
| Subagent | register_subagent_tool | tpi-agent/subagent/*.rs | child session | provider |
| Doctor/Init/Prune | tpi doctor/init/prune | src/doctor.rs + main.rs | — | FS |

## 功能清单与进度

| ID | 功能 | Design Review | Bug Review | Cross Review | 状态 |
| --- | --- | --- | --- | --- | --- |
| F01 | Config 加载/合并/优先级 | ✅ | ✅ | ✅ | Done |
| F02 | CLI 入口与子命令 | ✅ | ✅ | ✅ | Done |
| F03 | Session durable 存储 | ✅ | ✅ | ✅ | Done |
| F04 | Session 恢复/修复 | ✅ | ✅ | ✅ | Done |
| F05 | Context 投影与 token 预算 | ✅ | ✅ | ✅ | Done |
| F06 | Compaction | ✅ | ✅ | ✅ | Done |
| F07 | Provider 适配与流 | ✅ | ✅ | ✅ | Done |
| F08 | Agent 循环 | ✅ | ✅ | ✅ | Done |
| F09 | 工具执行/调度 | ✅ | ✅ | ✅ | Done |
| F10 | 内置工具（bash/edit/write/read/search/web/process/request_input） | ✅ | ✅ | ✅ | Done |
| F11 | MCP 集成 | ✅ | ✅ | ✅ | Done |
| F12 | Managed Process | ✅ | ✅ | ✅ | Done |
| F13 | Remote/SSH | ✅ | ✅ | ✅ | Done |
| F14 | Skills | ✅ | ✅ | ✅ | Done |
| F15 | TUI 交互/渲染/滚动 | ✅ | ✅ | ✅ | Done |
| F16 | Web 接口 | ✅ | ✅ | ✅ | Done |
| F17 | Eval Harness | ✅ | ✅ | ✅ | Done |
| F18 | Auth/凭据 | ✅ | ✅ | ✅ | Done |
| F19 | Doctor/Init/Prune | ✅ | ✅ | ✅ | Done |
| F20 | Subagent | ✅ | ✅ | ✅ | Done |

## System-Level Review（跨功能）

| 维度 | 结论 |
| --- | --- |
| 状态一致性 | Session 单一事实源设计正确；发现 1 处 runtime history 与 session 不一致路径（ISSUE-001 预算超限悬空 tool_calls） |
| 生命周期 | startup→running→shutdown 整体完整；MCP shutdown 存在可挂死路径（ISSUE-006） |
| 错误传播 | 底层错误多数能到达用户；provider 4xx 确定性错误被误当瞬时错误重试（ISSUE-009） |
| Cancellation | 主链（用户 cancel → provider → tool）完整；远端 baseline 与 MCP reader 无取消点（ISSUE-006/007） |
| Configuration | 发现死配置字段 shell.kind 与文档/默认值矛盾（ISSUE-021/022） |
| Persistence | JSONL 严格校验 + 尾部修复 + 单写者锁正确；远端写覆盖存在网络错误误判（ISSUE-002） |
| Concurrency | 同文件大小写锁不一致（ISSUE-011）；ProcessRegistry 无界增长（ISSUE-012） |
| Contract | ToolBudget 超限路径违反"assistant.tool_calls 必须有对应 tool result"（ISSUE-001） |
| UX Semantics | 帮助文案宣称的 Ctrl+C 语义不存在（ISSUE-014）；搜索/模态内 Ctrl 组合键被当字面字符（ISSUE-016/018） |

**逐功能审计报告全文见 AUDIT_FINDINGS.md；执行摘要见 AUDIT_SUMMARY.md。**
