# TPI Bug Audit

> 阶段：第一轮全量审计（2026-08-08）。基于真实用户长期使用视角，不依赖 `cargo test` 全绿。
> 状态图例：`[OPEN]` 待修 / `[FIXED]` 已修（含 regression test）/ `[WONT]` 本轮不修（记录原因）。
> 严重度：P0 = 数据/状态错误、死锁、panic、取消失效、输入丢失、terminal corruption；
> P1 = 高频 UX bug、明显卡顿、滚动错误、重要信息丢失、恢复异常；P2 = 低频/人体工学；P3 = polish。

---

## P0（数据/状态错误、panic、输入丢失）

### BUG-001 — read 工具读取大中文文件时 `String::truncate` panic
- **Severity:** P0（panic）
- **Area:** tool/read / UTF-8
- **Symptom:** 模型执行 `read` 读取一个超过 32 KiB 且在第 32 KiB 边界处是多字节字符（中文/emoji）的文件时，进程 panic；交互模式下整个 TUI 崩溃（panic hook 恢复终端后退出）。
- **Trigger:** `read` 一个 UTF-8 文件，正文前 32 KiB 末尾恰好落在一个 3 字节中文字符中间（例如一行很长、含大量中文的日志/源码）。
- **Root Cause:** `src/tool/files.rs:100` `text.truncate(DEFAULT_READ_MAX_BYTES)` 对 `String` 按字节截断；`String::truncate` 在非 char boundary 处直接 panic。`read_window` 返回的是 char-safe 的拼接文本，但随后被按裸字节截断。
- **Why tests miss it:** 现有 `read_window_returns_utf8_chinese` 用小文件（不触发截断）；`tui_utf8` 只测渲染层，不测工具层字节预算截断。
- **Fix:** 统一 UTF-8 安全截断 helper（`util::truncate_to_char_boundary(&mut String, usize)`），所有按字节预算截断 `String` 的代码必须走它。
- **Regression Test:** 构造 >32 KiB、截断点落在中文字符中间的文件，断言 `read` 成功返回且输出为合法 UTF-8、长度 ≤ 预算。
- **Status:** [FIXED]（util::truncate_to_char_boundary + read 大中文回归测试）

### BUG-002 — web_fetch 正文按字节 truncate panic
- **Severity:** P0（panic）
- **Area:** tool/web_fetch / UTF-8
- **Symptom:** `web_fetch` 抓取一个 48 KiB 截断点落在多字节字符中间的页面时 panic。
- **Trigger:** 任意页面转换后的纯文本 > 48 KiB 且边界落在 CJK/emoji 字符中间。
- **Root Cause:** `src/tool/web.rs:917` `body.truncate(FETCH_BODY_BUDGET)`，同 BUG-001。
- **Why tests miss it:** web 测试 fixture 都是短文本，不覆盖字节预算截断。
- **Fix:** 同一 helper。
- **Regression Test:** 构造多字节文本 > 48 KiB 的 HTML fixture，断言不 panic、输出合法。
- **Status:** [FIXED]（bounded_body + 大中文正文回归测试）

### BUG-003 — pending_message 必须再按一次键才执行（`tpi "prompt"` 不立即运行）
- **Severity:** P0（交互失效 / 输入不执行）
- **Area:** app 主循环 / pending message
- **Symptom:** `tpi "修复这个测试"` 启动后一直停在欢迎页，必须随便按一个键才开始运行；Agent 运行中用户输入下一条消息，当前 run 结束后也必须再按一个键才启动下一条。用户会以为消息丢了或程序卡死。
- **Trigger:** 1) `tpi "<prompt>"`；2) 运行中输入 Enter，等 run 结束。
- **Root Cause:** `interactive_loop` 每次循环第一步是 `tokio::select! { event = key_rx.recv() => ... }`，阻塞等键盘事件；`pending_message` / `pending_session` 的消费在 select **之后**。初始 prompt 与 run 期间排队的新消息都在等待一个永远不会到来的“终端事件”。
- **Why tests miss it:** `tests/tui_reducer.rs` 只验证 reducer 把 `pending_message` 置位，没有测 app 主循环何时消费；没有 app-loop 级集成测试。
- **Fix:** 循环开头若存在 `pending_message` / `pending_session`，先跳过阻塞 select 直接进入消费段。
- **Regression Test:** 需要 app-loop 级测试（见 BUG-005 的队列测试；此处验证初始 pending 无需键盘事件即可被消费）。
- **Status:** [FIXED]（主循环 has_pending_work 守卫 + 队列 + 测试）

### BUG-004 — 交互模式下 Ctrl-C 完全无效（Windows raw mode 语义）
- **Severity:** P0（取消失效 / 无法退出）
- **Area:** Ctrl-C / raw mode / 终端生命周期
- **Symptom:** Windows 交互模式下按 Ctrl-C 什么都不发生：不能取消正在运行的 Agent，也不能在空闲时退出。用户只能 Esc 取消、输入 `/quit` 退出。
- **Trigger:** `tpi` 进入交互 → 按 Ctrl-C（运行中或空闲）。
- **Root Cause:** crossterm `enable_raw_mode()` 在 Windows 上清除 `ENABLE_PROCESSED_INPUT`（见 `crossterm-0.28.1/src/terminal/sys/windows.rs` 的 `NOT_RAW_MODE_MASK`）。清除后 Ctrl-C 不再产生 `CTRL_C_EVENT`，`tokio::signal::ctrl_c()` 的 handler 不会触发；Ctrl-C 作为 `Key(Char('c'), CONTROL)` 事件进入键盘线程，而 `reducer.rs:230` 明确 `return effects; // Ctrl-C 由 ctrl_c handler 处理`——两条路径都断了。
- **Why tests miss it:** 所有取消契约测试直接调用 `CancellationToken`，不经过真实终端/信号；没有 reducer 层 Ctrl-C 语义测试。
- **Fix:** reducer 把 Ctrl+C 键事件映射为：运行中 → `UiEffect::CancelRun`；空闲 → `UiEffect::Quit`。保留 `ctrl_c()` handler 作为非 raw 模式（`-p`）与 Unix 兜底；`Quit` 效果在 app 层走正常退出（含终端 restore），并把 `restore_terminal_on_exit` 补上 `LeaveAlternateScreen`。
- **Regression Test:** reducer 测试：running + Ctrl+C → CancelRun；idle + Ctrl+C → Quit；且 overlay/modal 打开时 Ctrl+C 仍取消（或按优先级）。
- **Status:** [FIXED]（reducer Ctrl-C → CancelRun/Quit + app Quit 处理 + 测试）

### BUG-005 — 运行中连发两条消息，第一条被覆盖丢失
- **Severity:** P0（输入丢失）
- **Area:** pending message / 输入
- **Symptom:** Agent 运行中用户提交消息 A，继续输入消息 B 并回车，A 被 B 覆盖；run 结束后只执行 B，A 无声消失。
- **Trigger:** 运行中输入 A → Enter → 输入 B → Enter（两次都在同一 run 内）。
- **Root Cause:** `UiState.pending_message` 是单槽 `Option<String>`；`reducer.rs:112` 每次 Enter 直接 `= Some(text)` 覆盖。
- **Why tests miss it:** reducer 测试只验证单条提交。
- **Fix:** 改为有界队列（`VecDeque<String>`，容量如 16），Enter 入队，app 按序 `pop_front` 消费；超容量时提示并丢弃最旧（避免无限增长）。
- **Regression Test:** reducer 测试：两条 Enter 后队列含两条；消费顺序保持。
- **Status:** [FIXED]（pending_messages VecDeque + FIFO 测试）

### BUG-006 — /new 与 /sessions 恢复后屏幕仍显示旧 session（状态串台）
- **Severity:** P0（状态/数据不一致：屏幕 session A、模型 session B）
- **Area:** session / TUI projection
- **Symptom:** `/new` 后模型上下文已清空，但屏幕仍保留旧 transcript、plan、用量；`/sessions` 恢复 session B 后，模型上下文是 B，屏幕仍是 A 的内容（不会重建 B 的对话）。用户看到的内容与 Agent 实际上下文不一致，误判会话状态。
- **Trigger:** 对话几轮 → `/new`（或 `/sessions` 选择另一 session Enter）。
- **Root Cause:** `app.rs` 的 `/new` 只 `*session = None; history.clear()`；`pending_session` 恢复只更新 `session`/`history` 并 push 一条 System 行；两者都不清空/重建 `ui_state.view`（transcript/live/plan/usage/scroll/search）。
- **Why tests miss it:** 没有 app 层“session 切换后 view 与 history 同步”的测试；TUI 测试都从空 ViewModel 开始。
- **Fix:** `/new`：重置 view（清 transcript/live/plan/usage/scroll/search/menu/overlay/modal/status）；`/sessions` 恢复：重置 view 后从 `history` 重建 User/Assistant/System 消息（工具结果以摘要行呈现），保证屏幕与上下文一致。
- **Regression Test:** ViewModel 提供 `reset_for_new_session()` / `load_history(&[ChatMessage])` 单测；app 层 /sessions 恢复后 view.transcript 非空且与 history 对应（若可测）。
- **Status:** [FIXED]（ViewModel.reset_for_new_session/load_history + /new、/sessions、启动恢复接线 + 测试）

---

## P1（高频 UX bug、重要信息丢失）

### BUG-007 — bash 输出预算被 stdout 占满时 stderr 完全消失
- **Severity:** P1（重要信息丢失：失败原因不可见）
- **Area:** tool/bash 输出 / 人体工学
- **Symptom:** 一条失败命令同时产生大量 stdout（≥24 KiB）与少量 stderr 错误信息时，模型看到的输出里 `--- stderr ---` 段为空，真正的失败原因被吞掉；Agent 只能靠猜或反复重跑。
- **Trigger:** `bash` 执行 `make 2>&1` 之前那种 stdout 灌满、stderr 有错误（进程层已分别保留 stderr tail，但 `outcome_for` 合并预算时先扣 stdout）。
- **Root Cause:** `src/tool/command.rs` `outcome_for`：单一 `budget_left = 24 KiB`，先 `push_stream("stdout", ...)` 再 `push_stream("stderr", ...)`；stdout 超过预算后 `budget_left == 0`，`push_stream("stderr", ...)` 的 `head` 为空 → stderr 段完全丢弃。
- **Why tests miss it:** 现有测试只覆盖 `status_name_mapping`；没有输出预算分配测试。
- **Fix:** 为 stderr 预留最小 tail（如非空 stderr 至少保留 4 KiB，从总预算扣除；stdout 使用剩余预算），并让失败/非零退出时 stderr 优先于 stdout；同时用 UTF-8 安全方式从字节尾部取窗（避免 lossy 边界）。
- **Regression Test:** 构造 stdout ≈ 30 KiB、stderr 有 `error: x` 的输入，断言输出含 `error: x` 且 `--- stderr ---` 非空。
- **Status:** [FIXED]（stderr 预留 4 KiB + stdout 剩余返还 + UTF-8 tail + 4 个回归测试）

### BUG-008 — 多行输入 > 8 行时光标渲染到输入区外（不可见）
- **Severity:** P1（输入体验 / 视觉状态）
- **Area:** composer / draw_input
- **Symptom:** 粘贴/输入超过 8 行（或折行后超过输入区高度）的文本后，光标落在输入区之外（可能盖住 footer 或屏幕外），编辑时光标不可见、无法确认当前位置。
- **Trigger:** 输入 10 行长文本，光标在最后一行。
- **Root Cause:** `draw_input` 用 `rows = wrapped.len()`（未 clamp 到 `area.height`）计算 `scroll_rows = cursor_row.saturating_sub(rows - 1)`，于是 `y = area.y + cursor_row - scroll_rows = area.y + rows - 1`，而输入区实际高度 `input_area_rows` ≤ 8；`rows > area.height` 时光标 y 越界。
- **Why tests miss it:** 现有 `input_cursor_cell_tracks_wrapped_lines` 只测单行/短多行，未测超过输入区高度的场景。
- **Fix:** 以 `area.height` 计算可见滚动：`scroll_rows = cursor_row.saturating_sub(area.height - 1)`，`y = area.y + cursor_row - scroll_rows`（保证落在区内），Paragraph scroll 同步使用该值。
- **Regression Test:** tui/mod.rs 单测：10 行输入 + 4 行区域 → 光标 y 落在区域内。
- **Status:** [FIXED]（input_scroll_offset 按区域高度 + 回归测试）

---

## P2（低频问题、人体工学、性能）

### BUG-009 — 工具层同步 std::fs 在 async 运行时上执行
- **Area:** performance / async-blocking
- **Symptom:** 大目录 `list`/`search`、大文件 `read`、网络盘访问会占用 Tokio worker 线程；极端情况下与 UI 事件处理争抢 worker，出现短暂卡顿。键盘线程独立（不丢按键），但 UI 帧可能延迟。
- **Root Cause:** `tool::execute` 在 async future 内直接调用 `std::fs`（read/search/edit）。只有 `@` 文件索引用了 `spawn_blocking`。
- **Fix（建议）:** 本轮不重构（涉及面大）；后续把 search/list/read 放入 `spawn_blocking` + `blocking` 收敛。
- **Status:** [FIXED]（同步工具经 spawn_blocking 执行；ToolContext Clone + execute 重构 + 回归测试）

### BUG-010 — `-p` 模式 run 失败时 current_cancel 残留
- **Area:** cancel / lifecycle
- **Symptom:** `tpi -p` 在 provider 失败提前返回时，`current_cancel` 仍持有已取消 token；进程即将退出，影响很小，但状态不干净。
- **Root Cause:** `run_prompt_once` 中 `agent::run(...).map_err(..)?` 提前返回时未清空 `current_cancel`。
- **Fix:** 用 RAII guard 或在 `?` 前清空。
- **Status:** [FIXED]（run_prompt_once 在 ? 前清空 current_cancel）

### BUG-011 — `restore_terminal_on_exit` 缺 LeaveAlternateScreen
- **Area:** terminal lifecycle
- **Symptom:** 若 Ctrl-C 信号在 fullscreen 交互模式下真的触发（非 raw 场景/未来平台），退出后终端残留在 alternate screen。
- **Root Cause:** `spawn_ctrl_c_handler` 空闲退出路径只 disable_raw_mode + show cursor + reset color，未 LeaveAlternateScreen。
- **Fix:** 补上 LeaveAlternateScreen（与 BUG-004 一起）。
- **Status:** [FIXED]（restore_terminal_on_exit 补 LeaveAlternateScreen/DisableMouseCapture/DisableBracketedPaste）

### BUG-012 — 工具流式输出用 unbounded channel，UI 慢时无背压
- **Area:** channel / backpressure
- **Symptom:** 极端高输出命令时 `output_tx`（UnboundedSender）可在内存中堆积事件；工具侧自身输出有界（24 KiB tail + artifact），事件帧数可能很多但文本总量受限，风险中低。
- **Fix（建议）:** 工具层在发送前对每帧文本做合并/限速；本轮不重构。
- **Status:** [FIXED]（有界 TOOL_STREAM_CAPACITY=256 + try_send 丢帧 + 满通道不阻塞回归测试）

---

## P3（polish）

### BUG-013 — Modal 提示“↑/↓ 滚动”但 ↑/↓ 无效
- **Area:** modal / shortcuts
- **Symptom:** `/help` 等 Modal 标题写“Esc 关闭 · ↑/↓ 滚动”，实际 ↑/↓ 被 composer/menu 路由吃掉，只有 PgUp/PgDn/滚轮能滚动。
- **Fix:** Modal/Overlay 打开时 ↑/↓ 滚动 1 行，或改提示文案。
- **Status:** [FIXED]（Modal/Overlay 打开时 ↑/↓ 滚动 1 行 + 测试）

### BUG-014 — 搜索打开时 Paste 仍插入 composer
- **Area:** search / input
- **Symptom:** Ctrl+F 后粘贴查询词，文本进入输入框而不是搜索框；搜索框内只能逐字输入/退格。
- **Fix:** `UiEvent::Paste` 在 search 打开时追加到 search.query。
- **Status:** [FIXED]（搜索打开时 Paste 进入 search query + 测试）

### BUG-015 — `-p` 模式 provider 失败信息粗糙
- **Area:** CLI
- **Symptom:** `tpi -p` 在 provider 失败时 stdout 只输出“run 以 Error 结束……”，真正的错误在日志文件里；用户无法快速知道失败原因。
- **Fix（建议）:** `-p` 失败时把 `RunFailure` 的错误详情打到 stderr。
- **Status:** [FIXED]（-p Error 消息附带 session 文件路径，用户可直接查看原因）

### SCROLLBAR-001 — 全屏历史缺少可拖动垂直 scrollbar（§24 要求）
- **Severity:** P2（人体工学）
- **Area:** TUI 滚动
- **Symptom:** 长会话只能 PgUp/PgDn/滚轮/搜索跳转，无法直接看到位置或点击/拖拽到任意位置。
- **Fix:** 全屏转录区右侧 1 列轨道 + thumb（比例按 visual 行数）；点击轨道/拖拽 thumb 按比例锁定；
  与滚轮/PgUp/PgDn/Ctrl+End/Ctrl+Home 配合；inline 兼容模式不预留（避免布局回归）。
- **Regression Test:** fullscreen 右缘轨道+thumb 渲染、内容不足一屏轨道稳定；reducer ScrollbarClick 比例跳转、Ctrl+Home 顶部。
- **Status:** [FIXED]（render_frame/draw_scrollbar + UiEvent::ScrollbarClick + mouse_ui_event + Ctrl+Home + 4 个回归测试）

### CANCELLATION-001 — wall-time 超时被显示成“用户取消”（§16 要求区分取消来源）
- **Severity:** P1（状态错误：系统超时被 UI/session 误报为用户取消）
- **Area:** 取消语义 / watchdog / session
- **Symptom:** Agent 达到 wall-time 预算被 watchdog 自动取消时，UI 与 session 都记录为 Cancelled（用户取消），用户无法区分“我取消了”和“系统超时了”。
- **Root Cause:** watchdog 与用户取消共用同一个 CancellationToken，无取消来源。
- **Fix:** `limits::CANCEL_CAUSE_USER/WALL_TIME` + watchdog 到期前先写来源再 cancel；agent 用 `cancel_reason_for_cause` 映射为 `CompletionReason::WallTimeExceeded`（新增 serde 变体，session 可持久化）；UI/-p 明确提示“非用户取消”。
- **Regression Test:** cancel_reason 映射、on_deadline 先写来源再取消、WallTimeExceeded session 文件往返；用户取消路径（p1_1）保持 Cancelled。
- **Status:** [FIXED]

### TRANSCRIPT-001 — /doctor、/diff 大段内容污染聊天历史（§19）
- **Severity:** P2（人体工学：系统信息污染长期 transcript）
- **Area:** slash 命令 / transcript
- **Symptom:** `/doctor` 环境报告与 `/diff` 聚合 diff 直接 push 成 System 行，长会话里历史被大段诊断/差异文本污染。
- **Fix:** 两者改为 Modal（`open_modal`），Esc 关闭，不再写入 transcript。
- **Status:** [FIXED]

---

## 后续建议（不属本轮修复范围）
- 长会话每帧全量 rebuild transcript 逻辑行：当前 `plan_window` 每帧重建全部 entry 的 wrapped 行（缓存只缓存 markdown 文本行，wrap 仍全量）。2000 entries 时仍可接受；若 profiling 显示卡顿再做增量。
- 滚动搜索快照/命中索引：同上。

## 验收证据（2026-08-08 自动验收，§40 的非交互部分）
- `tpi -p "hi"`（真实 provider）：stdout 输出最终答案，exit 0；session JSONL 生成
  `user_submitted → run_started → assistant_message_committed → run_completed`（seq 1..4）。
- `tpi -p` 指向不可达端点：exit 1，stderr 输出
  `错误: provider failure: connection failed: attempt 2: ...`（§15 重试 + §19.2 可诊断错误）。
- `tpi doctor`：config/model/api_key/git_bash/sessions/artifacts/logs/workspace 全绿。
- `tpi --help`：CLI 正常。
- 交互 TUI（§40）已通过 node-pty（Windows ConPTY，真实终端语义）驱动验收：
  basic run+流式回答+/quit 干净退出（exit 0）、运行中 Ctrl-C 取消（出现“已发送取消”）、
  空闲 Ctrl-C 退出、PgUp/PgDn/Ctrl+End、read+bash(cargo test) 多工具任务（工具卡片可见）、
  运行中排队第二条消息自动执行（footer“已排队”）、运行中 resize、/new 会话切换、
  鼠标滚轮后输入存活。脚本固化在 scripts/interactive_acceptance.js。
  注：Alt+E 工具详情与 bracketed paste 无法经 ConPTY 输入模拟（由真实终端产生），
  由单元/集成测试覆盖（overlay 渲染/点击、paste 路由、editor unicode）。

