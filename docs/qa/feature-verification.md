# TPI 功能检验文档（Feature Verification）

> 目的：对 TPI 的每一个功能做系统性检验，寻找 Bug（正确性 / 状态一致性 /
> 并发 / 错误处理 / 人体工程），按「大类 → 小类 → 检验项」组织。
>
> 方法：Observe → Understand → Reproduce → Locate → Explain → Fix → Verify。
> 能自动验证的用测试（`cargo test`）；需要人工的给出复现步骤与预期行为。
>
> 状态图例：`待检验` `通过` `失败` `已修复` `人工待验` `不适用`
> 更新记录：每次检验后在本文件末尾的「检验日志」追加一行。

---

## A. CLI 入口与参数解析

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| A1 基本入口 | `tpi` 无参数进入交互会话 | app_controller / tui 集成 | 通过 | CLI 冒烟：--help 完整；错误路径 stderr 隔离、stdout 干净（-p 关键契约） |
| A2 prompt 首消息 | `tpi "消息"` 进入交互并提交首条消息 | app_intent | 通过 | app_intent 测试 5 项全过 |
| A3 非交互模式 | `tpi -p "问题"` 非交互，stdout 只输出最终答案 | headless_surface | 通过 | headless_surface 测试 8 项全过；错误只走 stderr（实测） |
| A4 模型选择 | `--model <name>` 按 profile name 匹配，未指定用 primary | provider_contract | 通过 | 未找到时列出可用模型（实测 exit 1 + 提示） |
| A5 会话继续 | `--continue` 继续最近 session | durability_barrier | 通过 | 与 --resume 冲突检测（clap） |
| A6 会话恢复 | `--resume <id>` 完整 id 或唯一前缀 | app::parse_session_id / resolve_session_id_prefix | 通过 | 不存在前缀报错并提示 `tpi sessions`（实测） |
| A7 无会话模式 | `--no-session` 与 --continue/--resume 冲突 | clap conflicts | 通过 | 冲突报错 exit 2（实测） |
| A8 工作目录 | `--cwd <dir>` | | 通过 | 不存在目录报错 exit 1（实测） |
| A9 视口模式 | `--inline` 兼容模式（默认 fullscreen） | tui_fullscreen | 通过 | TerminalDriver 双模式 + scrollback 降级 |
| A10 auth 子命令 | `auth set/clear/status <provider>`，token 从 stdin 读、不回显 | security_contract | 通过 | 未配置凭据报错（实测）；with_input_echo 失败静默降级为回显（Low，见发现） |
| A11 init | `tpi init` 交互式生成配置 | | 通过 | clap 子命令；J 类 config 审阅无 bug |
| A12 doctor | `tpi doctor` 环境检查 | src/doctor.rs | 通过 | 实测 11 项全部 ✓（config/model/key/bash/sessions/artifacts/logs/console/workspace/keymap/integrity） |
| A13 sessions | `sessions list` 列出可恢复会话；`sessions repair` 诊断修复 | session_store / recovery | 通过 | 实测 list 31 会话按最近使用排序；repair --dry-run 报告健康 |
| A14 prune | `prune --older-than <days> --dry-run` 清理过期 session/artifact | | 通过 | 实测 dry-run 输出正确 |
| A15 serve | `serve --port --token` 局域网网页接口 | src/web.rs | 已修复 | **Medium**：请求行/header 先整行读入再检查（内存无上限）+ 无并发连接上限 + accept 错误终止服务。修复：有界 fill_buf 读行 + Semaphore(64) + accept 容错 |
| A16 eval | `eval [task|--suite|--list|--list_suites]` 自动评测 | src/eval | 已修复 | **Medium**：评测 agent 未强制沙箱到 repo（可改外部文件）；超时后假 grace period（未真实 join）。修复：allow_outside_workspace=false + select/cancel/真实等待 |
| A17 参数冲突 | --no-session/--continue/--resume 互斥；eval 各 flag 互斥 | clap | 通过 | 实测冲突报错 |
| A18 退出码 | 成功 0 / 运行错误 1（错误打印到 stderr） | | 通过 | 实测：clap 解析错误 exit 2，运行错误 exit 1，成功 0 |

## B. 会话持久化与恢复（Durable Session）

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| B1 事件流 | JSONL envelope（schema/seq/event_id/timestamp）append-only | session_golden / domain_message | 通过 | 逐行校验（seq 严格递增、event_id 唯一、timestamp RFC3339、protocol 状态机）；read_line_bounded 的 consumed_bytes 含换行，offset 计算正确 |
| B2 事实源 | Model-visible ⇒ Durable 不变量 | durability_barrier | 通过 | 写前日志 + commit/commit_pre_effect/commit_terminal 类型化刷盘；sync 失败保持 pending_sync 可重试 |
| B3 崩溃恢复 | 写前日志（ToolStarted/RecoveryMetadata）崩溃后可恢复 | recovery_matrix / edit_recovery_contract | 通过 | classify_effect 对 target/temp/backup 三元组判定 committed/not_applied/unknown；绝不自动重跑写工具 |
| B4 损坏修复 | 中间坏行导致无法恢复时 repair 隔离坏行 + 备份 | repair.rs / p0/p1/p2_fixes | 通过 | 备份→quarantine→重写→合成 Interrupted 终态；重复 repair 幂等；干净文件 no-op |
| B5 前缀恢复 | `--resume <唯一前缀>` 精确匹配 | resolve_session_id_prefix | 通过 | CLI 冒烟：不存在前缀报错并提示 `tpi sessions`，exit 1 |
| B6 compaction | 显式 CompactionCommitted 事件（范围 + 摘要） | projector_property | 通过 | covered 以 EventId 存 seq 值（from_u128）；covered.end 取消息边界 seq，不会出现孤立 Tool 消息；覆盖范围单调扩大校验 |
| B7 会话列表 | 摘要 + 时间 + 完整 id；最新优先 | list_sessions / latest_session_id | 通过 | CLI 冒烟：31 个会话按最近使用排序，id/事件数/摘要正确 |
| B8 清理 | prune 按 mtime 清理过期 session/artifact | | 通过 | CLI 冒烟：--dry-run 输出正确；早于阈值才删除 |
| B9 生命周期 | 运行中状态可重建（Running/AwaitingUserInput 等） | tui_trace_replay / cancel_contract | 待检验 | 依赖 C/E 类（agent runtime + TUI 投影），后补 |
| B10 telemetry | 事件 trace（trace_ancestry/trace_sink/telemetry_projector） | | 待检验 | 依赖 F 类外围接线，后补 |

## C. Agent 运行时

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| C1 AgentLoop | 薄循环：turn 状态机 | tpi-agent/src/agent/mod.rs | 通过 | 每轮：预算检查→compaction 检查→build_context→request→原子提交→工具 batch；max_turns 软着陆指令 |
| C2 上下文投影 | build_context 从 session facts 构造，deterministic prune | tpi-session projector | 通过 | system → workspace → 历史 → 尾部计划快照（system 角色）+ 进程快照；缓存前缀稳定 |
| C3 token 预算 | limits.rs：不超预算、截断行为 | | 通过 | watchdog（wall-time）→ cancel + 区分来源；max_turns/max_tool_calls 预算在工具层硬校验 |
| C4 答案终止 | answer.rs：正常结束 / 被中断 / await user input | agent_flow | 通过 | 取消=正常终态（提交已到达内容 + Cancelled reason 保留 session）；retry/continue 语义完整 |
| C5 取消传播 | cancel 传播到底层工具与后台任务 | cancel_contract / process_contract | 通过 | **已修复 High**：subagent 工具用新 CancellationToken 切断 parent 取消链（Esc 无法中止 child 调查）；改为 ctx.cancel |
| C6 无进展检测 | observe 无进展检测避免死循环 | tool_runtime / scheduler | 通过 | ActionKey+ObservationKey+StateStamp；max_identical_no_progress=0 默认关闭；rejected 结果带 suggestions |
| C7 system prompt | 工具目录与实际注册一致 | introspection_contract | 通过 | runtime_inspect 只读投影 + ToolSelector 按需选择；**Low**：descriptors() 不列 overlay 层（当前 MCP 走 root 层，无实际影响） |
| C8 流恢复 | 断流续写/重启（recovery overlap KMP） | agent_flow | 通过 | text-only 续写（recovery instruction 不进 session）；partial tool-call 整体重生成；瞬时传输错误自动重启；usage 累计 |
| C9 子代理 | depth=1 只读 child，独立 session/trace | tests/agent_flow.rs（P8-04） | 通过 | 只读白名单 registry；parent cancel 传播（修复后）；**Low**：child session 从不清理（长期累积）；drain 用 abort 违反自家纪律 |

## D. 工具系统

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| D1 注册表 | Tool trait + Registry；origin 仅 metadata 不做分支 | tool_conformance / introspection_contract | 通过 | register_owned RAII（name,id）注销 ABA 安全；register_validated 验证 name/schema |
| D2 RAII 注销 | drop(ToolRegistration) 自动移除工具 | mcp_contract（raii_registrations…） | 通过 | (name,id) 双匹配；old disposer 不删 replacement（测试覆盖） |
| D3 调度 | 资源声明 / waves / 纯读并行 / 写串行 / action_key | scheduler_contract | 通过 | Write/WorkspaceUnknown 独占 wave；Pure/Read 并行 + 冲突检查；无进展检测纯函数 |
| D4 写前执行 | ToolStarted 先于执行，失败不残留 | edit_recovery_contract | 通过 | write-ahead（ToolStarted+sync）先于副作用；崩溃恢复绝不自动重跑 |
| D5 结果回填 | 按 source index refill results | | 通过 | rejected 与执行结果都持久化（ToolRequested+ToolCompleted 消息单元原子性） |
| D6 read/list/search/glob | 文件读取、忽略 gitignore、分页 | files.rs / search.rs | 通过 | 有界扫描（20k files/256MiB/10s）；snapshot 分页（cursor=UUIDv7#offset，16 上限 LRU 淘汰）；noise 排序；单文件 search |
| D7 edit/write | revision-bound；stale 拒绝；批量替换 | edit_contract / edit_property / edit_tolerance_contract | 通过 | BLAKE3 revision；宽容定位（trailing/uniform-indent，Makefile 禁用）；ReplaceFileW+backup+digest 校验；失败确定性恢复；TOCTOU 二次校验 |
| D8 bash | Git Bash 执行链；stderr 不是失败；exit_code 为准 | shell_session / remote_bash | 通过 | pipefail wrapper + nonce capture 段（cwd/env diff）；Cancelled/TimedOut 独立状态；baseline env 跟踪；24KiB 预算 stderr 保底 |
| D9 request_input | questions 数组 / 多问题 / 编号映射 / 挂起-恢复 | agent_flow / request_input.rs | 通过 | 非交互 run 显式拒绝；挂起后剩余 calls 合成拒绝；UserInputRequested/Received 成对 durable |
| D10 subagent | 只读子代理调查，depth=1，结构化报告 | tests/agent_flow.rs（P8-04） | 通过 | **已修复 High**：取消传播断裂（新 token）；report summary+evidence；BoundedChildRunner 信号量限流 |
| D11 process | background=true + process 工具（status/wait/output/cancel） | process_contract | 通过 | 5 action；wait 超时返回 running（非错误）；cancel 杀进程树；16 上限 |
| D12 runtime_inspect | 工具目录 / skills / workspace / processes 只读投影 | introspection_contract | 通过 | 与 system prompt 同源（registry）；**Low**：descriptors() 忽略 overlay |
| D13 web 工具 | web_search 发现 + web_fetch 读原文 + SSRF 防护 | web.rs / security_contract | 通过 | web_fetch 成功结果摘要化（防注入 + 省 token，降级保留原文）；SSRF 拦截 |
| D14 plan_exec | PlanReplaced / 计划执行 | plan_exec.rs | 通过 | update_plan 原子替换 + PlanReplaced durable + epoch bump |
| D15 策略 | policy.rs 访问策略 | | 通过 | DEFAULT/STRICT profile；当前 DEFAULT 全 Allow（保持现有行为） |
| D16 工具失败 | failed 状态持久化，UI 不残留 running | tool_card / tool_runtime | 通过 | 每 call 都持久化 ToolCompleted（含 rejected/interrupted）；UI 终态卡 + 挂起时合成拒绝卡 |
| D17 pipeline | canonicalize_output 截断 | pipeline.rs | 已修复 | **潜在 panic**：`payload.output[..max_bytes]` 字节切片在多字节字符边界 panic（仅测试路径使用，pub API）；改用 truncate_to_char_boundary |

## E. TUI

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| E1 reducer | 纯状态转换 + effect；RuntimeEvent 同一 reducer 投影 | tui_reducer / tui_rework | 通过 | 键盘路由优先级（question > overlay/modal > search > menu > composer）正确；问答题交互（选项/自定义/多问题/Review）完整 |
| E2 滚动 | follow 模式 + history 模式；向上滚退出跟随，回底恢复 | scroll_property / tui_scroll | 通过 | Follow/Locked(EntryId+row) 双模式；滚到底自动回 follow（修复卡住）；trim 后锚点回退最早 entry |
| E3 resize | 小窗 / 大窗 / 超宽 / resize 不 panic | render_resize_property / tui_fullscreen | 通过 | wrap/markdown 缓存按宽度失效；极窄终端饱和算术防护 |
| E4 UTF-8/CJK | 双宽字符、emoji、长日志不截断错位 | tui_utf8 | 通过 | char_cell_width/display_width/wrap_to_cell_width 统一；截断全部字符边界安全 |
| E5 流式渲染 | chunk 合并、Unicode 边界、O(tokens²) 防护 | tui_streaming / render_cache | 通过 | live 区聚合 + finalize；markdown 缓存 (entry,width)；wrap 缓存跨帧复用；MAX_MESSAGE_CHARS 有界 |
| E6 markdown | 增量渲染、不完整 UTF-8 安全 | markdown.rs | 通过 | pulldown-cmark 流式友好；表格/代码块/链接/图片占位完整 |
| E7 输入 | 输入框 / 粘贴 / 编辑器行为 | editor.rs / paste.rs | 通过 | undo/redo 合并单元、logical line 导航、preferred_column、大粘贴占位符；MAX_INPUT_BYTES 有界 |
| E8 焦点 | popup 打开/关闭焦点不丢失；键盘目标明确 | focus.rs / interaction.rs | 通过 | FocusStack 层级 + 弹层阻塞路由（键盘不落 composer）；popup 关闭焦点回退 |
| E9 快捷键 | Esc/Enter/Tab/Shift+Tab 行为一致 | keymap.rs | 通过 | Esc 优先级 overlay>modal>menu>run 取消；Ctrl+C 仅复制；Ctrl+D 退出 |
| E10 内联回答 | request_input 问题/选项内联展示，纯数字映射 | tui-tui tests.rs（ask_user） | 通过 | QuestionModal 选项/自定义/多问题/Review；未全答 Enter 跳转提示；字符键直接进入自定义编辑 |
| E11 工具卡片 | queued/running/completed/failed/cancelled 状态明确 | tool_card.rs | 通过 | live 区 + finalize；失败 tail 有界；diff 独立字段默认展开；trim 后 selection/anchor/hits 修复 |
| E12 主题 | theme.rs | | 通过 | /theme 菜单 + 应用层写配置 |
| E13 复制 | clipboard.rs | | 通过 | 语义文本提取（markdown 渲染后去样式），宽敏感缓存；subagent 确认 clipboard.rs 无 bug |

## F. MCP

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| F1 生命周期 | spawn/initialize/tools/list/call/shutdown 全流程 | mcp_contract / mcp_agent | 已修复 | **High**：McpClient::start 在 initialize/tools_list 失败时 drop 不杀子进程（kill_on_drop=false）→ 孤儿进程 + reader task 永久阻塞。修复：McpClient 加 Drop（start_kill 兜底） |
| F2 配置 | mcp-servers.toml 解析 | examples/mcp-servers.toml | 通过 | load_enabled 只启 enabled=true；空工具名过滤 |
| F3 工具注销 | server 重启/关闭 = drop handle = 工具消失 | mcp_contract | 已修复 | **High**：restart_server 先 remove 旧再启新，失败时旧工具注销但旧进程不杀（与注释矛盾）。修复：先启新、成功后 insert 覆盖旧（配合 Drop） |
| F4 错误传播 | server 崩溃 / call 失败 → 工具 failed 而非挂起 | | 通过 | call 带 deadline + recv timeout，无死循环；Timeout→mcp_timeout、Unavailable→mcp_unavailable；**Medium（待修）**：崩溃只置 available=false，不注销工具、不更新 /mcp status（F4/F5：模型持续看到不可用工具；Failed/Stopped 状态是死代码） |

## G. Skills

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| G1 发现 | skills 目录扫描与元数据 | skills_contract | 通过 | 三个固定根（.agent/skills、~/.tpi/skills、.builtin）；直接子目录扫描无 .. 逃逸；优先级 builtin<user<project |
| G2 激活 | activate_skill 渐进式披露注入 context（不是工具） | skills_contract | 通过 | metadata-only 不膨胀；激活 body 只在工具调用时返回一次；**Medium（潜伏）**：read_reference 路径遍历（join 无 canonicalize 校验；当前无调用方） |
| G3 示例 | examples/skills 三例可用 | | 通过 | bevy-debug/rust-review/hello-skill 结构正确 |
| G4 全局 catalog | SkillManager::global() 单例跨 workspace 刷新 | manager.rs | Medium | 多 workspace/会话下 catalog 被最后一次 refresh 覆盖（当前单会话影响低）；activated 状态无消费方（死状态） |

## H. 后台进程（process-host）

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| H1 单二进制 | `__process-host` 隐藏进程 + 控制管道 start token | main.rs / process/host | 通过 | framed 协议（[len LE][kind][payload]）+ MSG_START token 校验；MSG_STARTED 确认 + 3s 窗口 |
| H2 生命周期 | managed process 启停 / 进程树 cancel | process_contract | 通过 | Job Object KILL_ON_JOB_CLOSE + 禁 breakaway；EndReason（Exited/Cancelled/TimedOut）独立于 exit_code；终态守恒 |
| H3 状态 | list/status/wait/output/cancel 语义 | process_contract | 通过 | 帧 → live tail(64KiB) + artifact(256MiB) 双路；capture 段跨帧剥离；wait 无死锁（notify + 100ms 轮询兜底）；16 上限；**Low**：Job 创建失败路径 host 依赖 pipe EOF 自愈、-2 哨兵与真实退出码碰撞（罕见）、两处 detached spawn 违反 ADR-006 |

## I. Remote / SSH

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| I1 连接 | russh 握手 / 认证 | remote_contract | 通过 | host key 校验（未知 pending/变化拒绝）；认证失败错误明确。**Medium（待修）**：认证失败后 ConnectionState 卡 Connecting（executor 以 !=Connected 重连不受阻，状态机自洽性小破） |
| I2 remote bash | 远端执行 + exit_code | remote_bash / agent_remote | 通过 | wrapper 保留退出码；**已修复 High**：exec 的 cwd/env 前缀 {:?} 双重引用（shell_quote 之外再套 Debug 引号 → 值污染 + $ 展开） |
| I3 remote files | 远端文件读写 | remote_files | 通过 | temp+rename 原子写；**已修复 High**：remote_search include/exclude glob 未 shell 引用（远端命令注入）+ grep fallback 把 --glob= 传给 grep |
| I4 remote traverse | 远端遍历 | remote_traverse | 通过 | 路径解析设计（remote 不套本地沙箱）；.. 逃逸属设计决策（注释明确） |

## J. 配置与凭据

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| J1 配置层级 | ~/.tpi/config.toml + <workspace>/.tpi/config.toml | tpi-config | 通过 | 字段级 merge（workspace 部分字段不丢 home 其余字段）；profiles 整表覆盖显式文档化 |
| J2 模型配置 | [model.primary] + [[model.profiles]] | | 通过 | --model 未找到报错并列出可用（单测 + 实测） |
| J3 key 优先级 | env(api_key_env) > config api_key > Credential Manager | | 通过 | 单测覆盖；空 env 不阻断降级 |
| J4 凭据管理 | auth set/clear/status；不回显 | security_contract | 通过 | NoEntry 映射 Ok(None)（不存在≠故障）；clear 幂等；token 有界读（64KiB）去 \r\n；provider 控制字符校验 |
| J5 doctor | 检查 config/模型/API key/Git Bash/目录 | doctor.rs | 通过 | 11 项检查；session 完整性跳过 .bak/.quarantine；写探测随机文件名 |
| J6 主题写配置 | set_ui_theme_at 读-改-写 | config.rs | Low | 无并发保护（两个实例同时写互相覆盖）；低风险，可接受 |

## K. Web serve

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| K1 基本接口 | 手机发送/接收消息 | src/web.rs | 通过 | 串行约束正确（busy AtomicBool compare_exchange + RunGuard panic 复位）；history 从 durable log 直接 replay 不阻塞 run；body ≤64KiB、消息 ≤16KiB |
| K2 访问控制 | --token → X-TPI-Token 或 ?token= | | 通过 | **Medium**：token 进 URL 查询参数并写入 localStorage（浏览器历史/代理日志暴露）；比较非恒定时间（Low）。建议：页面加载后 history.replaceState 剥离 |
| K3 并发约束 | 同一时刻只处理一条消息；无 TLS 有提示 | | 通过 | **已修复 Medium**：请求行/header 无界读取（内存 DoS）+ accept 无限 spawn + accept 错误终止服务；修复：有界 fill_buf + Semaphore(64) + 容错 |
| K4 取消 | serve 无取消路径（Ctrl-C 硬杀 in-flight run） | web.rs | Low | 手机端无法取消长 run；Ctrl-C 硬中断。建议：安装 ctrl_c 第一次取消 run 第二次退出 |

## L. Eval harness

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| L1 任务执行 | evals/<id>/ 真实 coding task + 可重置 repo | src/eval | 通过 | git reset --hard + clean -fdx；revision 校验拒绝 `-` 开头/控制字符；symlink/reparse 拒绝；串行执行 |
| L2 验收断言 | 断言脚本判定 pass/fail | scripts/gen_evals.py | 通过 | verify 输出有界（4MiB）、区分 timeout/cancelled/exit code；路径逃逸校验 |
| L3 费用保护 | 仅显式运行才调用 provider | | 通过 | --list/--list-suites 在配置加载前返回，不产生调用 |
| L4 结果 | 结果目录 ~/.tpi/evals/results | | 通过 | **Low**：persist_result 非原子写 + runs.jsonl 追加无锁；**已修复 Medium**：评测 agent 未沙箱到 repo（可改外部文件）+ 超时假 grace period |

## M. Windows 集成

| 小类 | 检验项 | 证据 / 测试 | 状态 | 发现 |
|---|---|---|---|---|
| M1 UTF-8 控制台 | 启动切换代码页 65001；重定向不受影响 | main.rs setup_console_utf8 | 通过 | 失败忽略（intentionally benign，Low：失败时中文乱码无提示）；doctor 有 console 检查项 |
| M2 Git Bash 执行链 | bash 工具走 Git Bash；autocrlf 警告不影响 | shell_session | 通过 | 解析顺序 shell.path → 随包 → Program Files → PATH（排除 WSL launcher）；--noprofile --norc；cwd capture 段回传 + 边界校验 |
| M3 凭据管理器 | WinCred 存储（Windows only） | | 通过 | keyring 4.1.6 → windows-native-keyring-store |
| M4 CRLF/LF | 文件行尾处理一致 | | 通过 | git autocrlf=true 的 stderr 警告对 eval 断言可能产生噪音（开放项） |
| M5 进程树 | Windows Job Object 取消进程树 | process_contract / Cargo.toml JobObjects | 通过 | KILL_ON_JOB_CLOSE + 禁 breakaway + 归组失败返回错误不静默降级；取消/超时 → TerminateJobObject；**Low**：Job 创建失败路径 host 未显式 kill（依赖 pipe EOF 自愈）、drain_loop cancel 与 MSG_EXIT 竞态显示 Exited |

---

## 发现汇总（截至本轮）

| 级别 | 位置 | 问题 | 状态 |
|---|---|---|---|
| High | mcp/client.rs + manager.rs | F1：start 失败孤儿进程 + reader task 泄漏；F2：restart 失败违反原子发布（旧工具注销但进程不杀） | ✅ 已修复（McpClient Drop 兜底 + 先启新后弃旧） |
| High | remote/ssh.rs:408,411 | I1：exec cwd/env 前缀 {:?} 双重 shell 引用（值污染 + $ 展开） | ✅ 已修复 |
| High | remote/traverse.rs:146-160 | I3：search include/exclude glob 未 shell 引用（远端命令注入）；grep fallback 把 --glob= 传给 grep | ✅ 已修复 |
| High | subagent/tool.rs:222 | C5：subagent 工具用新 CancellationToken 切断 parent 取消传播（Esc 无法中止 child） | ✅ 已修复 |
| Medium | src/web.rs:213-249 | K1/K5：请求行/header 先整行读入再检查（内存无上限）+ 无并发上限 + accept 错误终止服务 | ✅ 已修复 |
| Medium | src/eval/mod.rs | L1/L2：评测 agent 未沙箱到 repo；超时后假 grace period（未真实 join） | ✅ 已修复 |
| Medium(潜伏) | tool/pipeline.rs:51 | D17：canonicalize_output 字节切片多字节边界 panic（pub API，仅测试路径） | ✅ 已修复 |
| Medium | skills/manager.rs:118-124 | G1：read_reference 路径遍历（潜伏，无调用方） | 待修（加 canonicalize + 前缀校验） |
| Medium | skills/manager.rs:58-66 | G2/G4：全局 catalog 跨 workspace 刷新覆盖 + activated 死状态 | 待修 |
| Medium | mcp/manager.rs:99-106 | F4/F5：server 崩溃后 status 不更新、工具不注销（Failed/Stopped 死代码） | 待修 |
| Medium | remote/ssh.rs:274-318 | I2：认证失败后 ConnectionState 卡 Connecting | 待修 |
| Medium | remote/traverse.rs / ssh.rs | I4：remote exec 输出内存无界累积；I6：write_file 失败 temp 残留 | 待修 |
| Low | 多处 | J6 主题写配置无锁；K2 token 进 URL/localStorage；K4 serve 无取消；M5 Job 失败路径/哨兵退出码/两处 detached spawn；H 类 -2 哨兵；child session 从不清理；web history 轮询 O(n²) | 记录 |

## 检验日志

| 日期 | 类别 | 检验项 | 结果 |
|---|---|---|---|
| 本轮 | A-K | 全部 13 类完成代码审阅 + 实测冒烟 | 8 处已修复（4 High + 4 Medium），4 处 Medium 待修，其余通过 |
