# TPI UX / 人体工学审计

> 第一轮（2026-08-08）。只讨论输入、历史、滚动、快捷键、Tool、Reasoning、Modal、Status、视觉层级、长时间使用人体工学。
> 判断标准：功能一天用几次？几次按键？能否预测？失败后是否知道发生了什么？是否打断阅读位置？是否破坏输入？是否污染长期 transcript？是否需要记忆奇怪快捷键？

---

## 1. 输入（Composer）

### 现状（好）
- Editor 是输入唯一事实源，`view.input` 只是投影（T6 已收敛）。
- 中文 IME：编辑器维护 Unicode 字符边界，`backspace/delete/←/→` 按 char 移动，不 panic。
- 多行：Shift+Enter / Alt+Enter / Ctrl+J；logical line 导航 + preferred column；Home/End 按 logical line。
- 历史：↑/↓ 到边界后进入 prompt history（去重、上限 100）。
- Ctrl+A/E/U/W/K 行级编辑；Alt+←/→ 词移动（中文按连续段）。
- 粘贴走 bracketed paste。

### 问题
1. **光标越界（P1，BUG-008）**：输入超过 8 行后光标被渲染到输入区外，长时间编辑大段文本时完全不可见。
2. **菜单/搜索期间 Paste 语义不一致（P3，BUG-014）**：搜索打开时 Paste 进 composer 而不是 search query。
3. **输入区高度策略**：`input_area_rows` 上限 8，但长输入时无视觉提示“内部滚动中”，用户可能不知道上方还有内容（建议滚动到光标时 footer 或输入区显示 `(N 行)`）。
4. **提交反馈**：Enter 提交后没有任何排队提示；运行中提交的消息要等 run 结束才执行——**已实现**（第二轮）：footer 显示“已排队 N 条消息”。
5. **Pending 单槽覆盖（P0，BUG-005）**：运行中第二条消息覆盖第一条。

### 建议（本轮只修 1/2/5，其余记录）
- 修光标越界；Paste 路由进 search；pending 改队列 + 排队提示。

---

## 2. 历史 / 会话

- `/sessions` 列表：按修改时间倒序、事件数、首条用户消息预览、Enter 恢复——信息够用。
- `/new` 与恢复后**屏幕不重置**（P0，BUG-006）：用户看到旧 session 内容但模型已切换，是最大的“不可预测”来源。
- 恢复后没有重建历史对话到屏幕：用户无法确认“我恢复的是哪段对话”。
- 建议：/new 清屏并显示分隔；恢复 session 时把 history 重建到 transcript（至少 User/Assistant 消息 + 工具摘要行）。

---

## 3. 滚动

### 现状（好）
- Follow / Locked(EntryId + row) 双模式；Locked 期间新内容不移动视口，只计数 `pending_below`，footer 有提示。
- PageUp/PageDown、滚轮（3 行）、Ctrl+End 恢复跟随、Alt+Up/Down 跳 User turn、Ctrl+F 搜索跳转。
- resize 后锚点保持语义位置（有测试）。

### 问题
1. **滚轮步长固定 3 行**，长对话中滚得很慢（PageUp 8 行也偏小）。可接受，但建议滚轮 5-6 行。
2. **没有可视 scrollbar**（任务 §24 建议）——**已实现**（第二轮）：全屏转录区右侧 1 列轨道 + thumb（比例按 visual 行数），支持点击轨道/拖拽 thumb 跳转；配合滚轮/PgUp/PgDn/Ctrl+End/Ctrl+Home。
3. **Locked 到底部后仍保持 Locked**，此时按 Ctrl+End 才回 Follow；用户可能不知道“已经在底部”。footer 的 pending 提示有帮助，可加“(已到底部, End 跟随)”状态。
4. **搜索命中不显示当前位置在文本中的上下文**，只显示 n/m；可接受。

---

## 4. 快捷键

### 现状
- 核心：Shift+Enter 换行、↑/↓ 历史、Tab 补全、Alt+T 思考折叠、Alt+E 最近工具、Alt+O 最近失败工具、Alt+[/] 切换工具、Ctrl+F 搜索、PgUp/PgDn、Ctrl+End 回底部、Esc 取消。
- `/help` Modal 列出命令与快捷键，不污染 transcript（好）。

### 问题
1. **Ctrl-C 在交互模式失效（P0，BUG-004）**：最常见取消/退出方式必须修。
2. **Esc 语义分层清晰**（overlay > modal > menu > run 取消），但 idle 时 Esc 无操作，用户可能期待 Esc 清空输入；建议记录（低优先级）。
3. **Alt+Up/Down 无 User 消息时静默**，无反馈（P3）。
4. **快捷键记忆成本**：Alt+E / Alt+O / Alt+[/] 区分“最近工具”和“最近失败工具”，稍显重叠；可保留（有 Alt+O 快捷看错误，价值高）。
5. Modal 内 ↑/↓ 无效但提示可用（P3，BUG-013）。

---

## 5. Tool 卡片

### 现状（好）
- 单行卡片：icon + name + target + duration/exit；运行中 spinner；失败保留 ≤4 行关键 tail；成功不展开。
- 完整输出走 Overlay（点击/Alt+E），不重写 scrollback、不污染 transcript。
- 命令压缩显示、有界 8 KiB；bash 实时输出运行中可见（尾部 3 行）。
- `@artifact` 引用保持模型可读完整输出。

### 问题
1. **失败命令 stderr 可能完全消失（P1，BUG-007）**：stdout 占满 24 KiB 预算时失败原因不可见——对 Coding Agent 是最大人体工学问题。
2. 成功工具也保留完整输出（可展开），但 UI 折叠态不占空间（好）。
3. 运行中实时输出只显示最后 3 行，没有“更多”提示；可接受（Overlay 可看）。
4. Tool 卡片失败 tail 用 error 色 DIM，可读性一般；建议失败 tail 不用 DIM（P3）。

---

## 6. Reasoning

- 默认折叠为 `◇ Thinking · Enter 查看`（一行），Alt+T 全局展开/折叠；点击行开 Overlay 看原文。
- 流式 reasoning 在 live 区，finalize 后进 transcript 作为独立条目（但默认折叠渲染成一行）。
- **好**：reasoning 不进 session/durable facts，不污染上下文。
- **问题**：折叠行仍占一条 Entry（高度 1），大量 reasoning 时 transcript 条目数增长；有 `reasoning_flood_collapses_to_one_line` 测试。可接受。
- 建议：长会话中 reasoning 超过 N 条时自动折叠更彻底（记录）。

---

## 7. Modal / Overlay

- Modal：/help /settings /session /sessions /thinking 等不进 transcript（好，符合 §19）。
- Overlay：工具详情/reasoning 原文覆盖显示，不重写 scrollback（好）。
- **问题**：
  1. Modal 内 ↑/↓ 无效（P3，BUG-013）。
  2. Overlay 打开时点击外部无操作（有意为之）；但用户可能想点击外部关闭——保持现状（Esc 关闭是标准）。
  3. Modal 尺寸 `min(88, width)-4`，窄终端下 Modal 比屏幕宽（`max(40)` 兜底）；极窄终端（<44 列）会溢出。P3。

---

## 8. Status / 信息层级

### 现状
- 最高优先级信息：Agent 正在干什么（Running tool/turn）、是否失败（tool card/tail/error 色）、下一步（plan 区域）。
- Footer：workspace/model/usage/context 用量条/状态；plan 独立紧凑区域（≤7 items）。
- 系统消息用 System 行（分隔线、run 失败提示等），数量少。

### 问题
1. **状态栏没有“已排队消息”提示**（见输入 §1.4）。
2. **ContextOverflow / MaxTurns 等结束原因只在 run 结束后的 System 行出现一次**，无持久状态；用户回到历史后看不到当前 run 的结果。可接受。
3. 大量成功工具调用时 transcript 全是卡片行；卡片已单行化，可接受。
4. footer 的用量条信息密度适中；但 token 数值对多数用户无决策价值（context 用量条更有用）。

---

## 9. 长时间使用人体工学

### 好
- 16ms 帧合并 + synchronized update；空闲不重绘（CPU 友好）。
- 2000 entry 上限防无限增长；消息/卡片输出有界。
- 键盘独立线程：运行中也能翻页/折叠/输入。
- Scroll Lock 锚点稳定，历史不漂移。

### 问题
1. **长会话 Ctrl+F 搜索后 Esc 关闭不跳回底部**（有意设计），但用户可能忘记自己处于 Locked；footer pending 提示帮助有限，建议状态栏在 Locked 时显示“历史浏览中”。
2. **无跨 session 记忆**（范围外，README 已声明）。
3. 每帧全量 rebuild wrapped 行：2000 entries 时 CPU 尚可，若实测卡顿再做增量（BUG-009 记录）。
4. 长会话下 `/sessions` 列表读取事件预览会解析整个文件（`first_user_preview` 调 `read_events`），session 文件很大时会慢；建议只解析前几行（P2，未列入 BUG_AUDIT 主表，记录此处）。

---

## 10. 结论（本轮 UX 修复优先级）
1. Ctrl-C 可用（BUG-004）
2. pending 立即执行 + 不丢消息 + 排队提示（BUG-003/005）
3. /new 与 session 恢复的屏幕/上下文一致性（BUG-006）
4. 输入光标不出界（BUG-008）
5. 失败命令 stderr 不消失（BUG-007）
6. 小 polish：Modal 滚动、Paste 进搜索、-p 错误到 stderr（BUG-013/014/015）
---

## 11. 修复状态（第一轮 2026-08-08）

- BUG-004 Ctrl-C：已修（reducer → CancelRun/Quit；Windows raw mode 下按键路径可用）。
- BUG-003/005 pending：已修（主循环跳过键盘阻塞 + 有界 FIFO 队列，连发消息不丢失）。
- BUG-006 /new 与 session 恢复：已修（view 重置/按 history 重建，屏幕与上下文一致）。
- BUG-008 输入光标越界：已修（滚动基准改为可见区域高度）。
- BUG-007 失败命令 stderr 消失：已修（stderr 最小预算 4 KiB 优先）。
- BUG-013 Modal ↑/↓、BUG-014 搜索 Paste、BUG-011 终端恢复、BUG-010 -p cancel 清理：已修。
- 未修（记录）：无（BUG-015 已修：-p Error 附带 session 路径）。

## 12. 第二轮修复状态（2026-08-08 续）
- BUG-009 工具层同步 fs：已修（同步工具经 spawn_blocking 执行 + 回归测试）。
- BUG-012 流通道背压：已修（有界 TOOL_STREAM_CAPACITY=256 + try_send 丢帧 + 满通道不阻塞测试）。
- 24 全屏 scrollbar：已实现（轨道+thumb、点击/拖拽、Ctrl+Home）。
- 排队提示：已实现（footer 已排队 N 条消息）。

## 13. 第三轮修复状态（2026-08-08 续）
- 16 取消来源：已修（wall-time 超时记录 WallTimeExceeded，UI/-p 明确提示非用户取消）。
- 19 transcript 污染：已修（/doctor、/diff 走 Modal）。
- BUG-015：已修（-p Error 消息附带 session 文件路径）。

## 14. 第四轮状态（2026-08-08 续）
- /sessions 预览性能：已修（first_user_preview 只流式读文件头部 ≤500 行，不再解析整个 session；长会话列表保持轻量）。
- 26/27/31 覆盖：新增随机尺寸渲染 proptest（width 40~220 × height 10~70，Follow/Locked）+ 2000 条 transcript 压力渲染测试。
- 验收：`tpi -p` 真实 provider 已通过；交互 TUI（raw mode / alternate screen / 鼠标拖拽 / Ctrl-C 信号路径）仍需真实终端人工验收（本环境无可用 PTY，winpty 无法管道驱动）。

## 15. 交互验收完成（2026-08-08）
- 通过 node-pty（Windows ConPTY）真实终端语义驱动 tpi：9 个场景全部通过
  （basic run、Ctrl-C 运行中/空闲、滚动键、多工具 read+bash(cargo test)、
  排队消息自动执行、resize、/new 切换、鼠标滚轮后输入存活、/quit 干净退出）。
- 验收脚本：`scripts/interactive_acceptance.js`（仓库外 npm i node-pty 后运行）。
- Alt+E overlay 与 bracketed paste 由真实终端产生，ConPTY 无法模拟，由单元测试覆盖。

## 16. PM 视角 TUI 显示审计（2026-08-08）
发现并修复的“显示会出问题”的点（按影响排序）：
1. footer 提示误写“End 返回最新”，实际快捷键是 Ctrl+End —— 已改文案并加断言。
2. 成功工具卡显示 `exit 0`（噪声且占宽度）—— 只在非成功时显示退出码；失败卡仍显示。
3. 窄终端（<44 列）Modal/Overlay 比屏幕宽、搜索框溢出 —— modal_width/height 与搜索框改为不超过终端尺寸（含单测）。
4. 系统分隔线固定 40 个 `─`：窄屏折行成两行、宽屏过短 —— 纯分隔线按终端宽度铺满（单测）。
5. emoji/组合字符宽度：`max(1)` 把 ZWJ/组合符算 1 列导致光标/折行漂移 —— 新增统一 `char_cell_width`（零宽=0），editor 与 wrap 全部使用（含单测）。
6. 工具卡主行在窄屏预算不足时会折行 —— 预算改为 name 优先截断、target 无余量即丢弃，保证单行（40 列单测）。
7. /help 快捷键说明补齐：Ctrl+End/Ctrl+Home/Alt+O/Alt+[/]、Modal 滚动、滚动条。

已确认无需修改：footer 长内容在窄屏被裁剪（Paragraph 不折行，属预期）；搜索命中无高亮（产品取舍，留待后续）。

## 17. 用户实测反馈修复（2026-08-08）
1. 思考/工具详情 Overlay 背景被 transcript 文字透出干扰 —— draw_overlay 缺 Clear，已补（先清空覆盖区再渲染）。
2. 回车发送后文本仍留在输入框 —— submit 清空 editor 后未同步 view.input，已补 sync_input。
3. 首次打开光标在 ❯ 左边 —— 空输入时 wrap 结果为空、行列未赋值（0,0），已改为停在 prompt 右侧（0,2）。
4. 模型输出期间光标一直闪烁 —— ratatui 每帧 set_cursor_position 都会 show_cursor；新增 should_show_input_cursor：空闲恒显示、运行中仅输入非空时显示（正在排队输入仍可见），其余时间隐藏。
全部有回归测试（submit_clears_input_projection / overlay_clears_background_before_rendering / input_cursor_empty_input_sits_right_of_prompt / cursor_hidden_during_run_when_input_empty）。

## 18. 下一轮迭代（2026-08-08）
1. 自由模式调度锁路径不一致（真 bug）：`tool_access` 用严格 resolver，外部绝对路径的等价写法（`..`/`.\`）映射到不同锁 → 同一外部文件可能并行写。新增 `resolve_lock_path`（自由模式词法规范化），`tool_access` 增加 allow_outside 参数；回归测试 freedom_mode_normalizes_outside_path_locks。
2. Ctrl-C 在搜索打开时直接退出/取消（人体工学陷阱）：改为优先关闭搜索，再按一次才退出；回归测试。
3. Modal/Overlay 打开时输入光标仍显示/闪烁：`should_show_input_cursor` 增加 modal/overlay 条件，打开弹层即隐藏输入光标。
4. footer 信息优先级：排队/新内容提示移到 ctx/tokens 之前，窄屏不再被挤掉。

## 19. 迭代第5轮（2026-08-08）
1. 弹层输入屏蔽（真 bug）：Modal/Overlay 打开时普通按键会写入后台 composer，关弹层后输入框出现乱码——现在只放行 Esc/导航键/（菜单打开时的 Enter），其余按键 no-op。回归：modal_blocks_composer_typing / overlay_blocks_composer_typing / sessions_menu_enter_still_works_with_modal_open。
2. 搜索命中高亮（人体工学）：命中条目整段下划线（保留原 fg），未命中不变；回归 search_highlight_underlines_matched_entries。
3. ConPTY 交互验收回归：9/9 通过（scripts/interactive_acceptance.js，NODE_PATH=临时 node_modules）。

## 20. 迭代第6轮（2026-08-08）
- PROVIDER-RETRY-001（P1 正确性）：SSE 中途断开且已收到事件时不可重试（否则重复文本/工具调用）；修复 + 单测/集成回归（RST mock，连接数=1）。

## 21. 迭代第7轮（2026-08-08）
- 多行输入提示（人体工学）：输入含换行时 footer 显示“输入 N 行”（>8 行内部滚动也可见）；单行不显示。回归 footer_shows_multi_line_input_hint。
- ConPTY 交互验收 9/9 通过（第 3 次全量回归）。

## 22. 迭代第8轮（2026-08-08）
- 长菜单可视窗口（/sessions 多会话）：选中项始终可见，窗口外项隐藏并显示 …（此前只渲染前 9 行，选中项超出可视区时用户看不到选择）。回归 long_menu_window_follows_selection。
- ConPTY 交互验收 9/9（第 4 次全量回归）。

## 23. 迭代第9轮（2026-08-08）
- 用户反馈 4 项修复后的同族补漏（同一批“悬浮窗/光标”问题复核）：
  1. 命令补全菜单同样悬浮在 transcript 上方，但 draw_menu 未 Clear —— 未选中行会透出背景文字（与“思考悬浮窗被背景文字干扰”完全同族）。补 Clear + 回归 menu_clears_background_before_rendering。
  2. Ctrl+F 搜索打开时，输入光标仍显示在 composer（打字进搜索框、光标却在输入框，视觉误导）。should_show_input_cursor 增加 search 条件，搜索打开即隐藏；字节级回归（\x1b[?25l）追加进 cursor_hidden_during_run_when_input_empty。
  3. 思考（reasoning）悬浮窗边框标题此前硬编码 “Tool details” —— 现按 overlay 类型显示 “思考（reasoning）”。回归 reasoning_overlay_uses_thinking_border_title。
- 全量测试通过（145 lib + 全部集成套件）。
## 24. 迭代第10轮（2026-08-08）
- 历史浏览草稿保护（人体工学）：↑ 进入 prompt history 前保存当前草稿，↓ 回到最新槽位时恢复草稿（此前直接清空/丢输入）；未浏览历史时按 ↓ 不再误清空输入。回归 history_browsing_preserves_draft / history_down_without_browsing_keeps_input；up_down_moves_within_multiline_then_falls_back_to_history 断言更新为恢复草稿。
- 弹层 Paste 屏蔽（真 bug，与弹层输入屏蔽同族）：Modal/Overlay 打开时 Paste（独立事件，按键屏蔽覆盖不到）会写入后台 composer，关弹层后输入框出现乱码——现改为弹层打开时 Paste no-op。回归 paste_blocked_while_modal_or_overlay_open。
- Modal 翻页/滚轮（人体工学，BUG-013 同族）：Modal 打开时 PgUp/PgDn 与鼠标滚轮此前滚动背后 transcript；现改为滚动 Modal 自身（↑/↓ 已支持）。回归 page_and_wheel_scroll_modal_when_open。
- Locked 状态提示（人体工学，第 3/9 节遗留）：历史浏览（Locked）且无新内容时 footer 显示“历史浏览中 · Ctrl+End 返回最新”。回归 footer_shows_history_browsing_indicator_when_locked。
## 25. 迭代第11轮（2026-08-08）
- 弹层鼠标点击屏蔽（真 bug，与弹层输入屏蔽同族）：此前只在 overlay 打开时屏蔽鼠标点击；Modal 打开时点击工具卡会在 Modal 后面再打开一个 overlay（两个浮层叠加、Esc 要关两次），点击 scrollbar 会滚动 Modal 背后的 transcript。已修：Overlay/Modal 任一打开时，鼠标 Down/Drag 一律不动作；reducer 侧 ClickTool/ClickReasoning/ScrollbarClick 也加防御性屏蔽。回归 clicks_blocked_while_modal_open / clicks_blocked_while_overlay_open。
## 26. 迭代第12轮（2026-08-08）
- Windows 控制台 UTF-8（真 bug，中文系统）：启动时未切换控制台代码页，GBK/936 控制台（cmd/PowerShell 旧 conhost）会把 UTF-8 输出（-p 答案、TUI、错误信息）显示成乱码。已修：main() 启动即 SetConsoleCP/SetConsoleOutputCP(65001)；doctor 新增 console 检查项。输入（config init/auth 中文）也随输入 CP 切换受益。
- 光标投影不同步（真 bug）：Home/End/Ctrl+A/Ctrl+E 只移动 editor 光标，未调用 sync_input()，view.input_cursor 停留在旧位置——按完键硬件光标不跟着走，直到下一次输入才纠正。已修：四处分支补 sync_input。回归 home_end_sync_input_cursor_projection。
## 27. 迭代第13轮（2026-08-08）
- 搜索打开时键盘翻页失效（真 bug，行为不一致）：Ctrl+F 搜索打开时，鼠标滚轮能滚动 transcript，但 PgUp/PgDn/Ctrl+Home/Ctrl+End 被搜索键路由吞掉，键盘无法浏览上下文。已修：搜索模式路由 PgUp/PgDn 滚动、Ctrl+Home 顶部、Ctrl+End 回最新（搜索保持打开）。回归 search_mode_keeps_transcript_navigation_keys。
- 滚轮步长 3 → 5 行（第 3 节建议落地）：长对话滚轮浏览更快；Modal/Overlay 内部滚轮同步 5 行。测试同步更新。
## 28. 迭代第14轮（2026-08-08）
- 搜索框 Ctrl+U 失效（真 bug）：Ctrl+F 搜索打开时按 Ctrl+U（/help 承诺“清空”）会把字母 u 插入搜索词，而不是清空——因为搜索键路由把 Ctrl+U 当普通字符。已修：搜索模式 Ctrl+U 清空 query。回归 search_ctrl_u_clears_query。
- prune 符号链接安全（真 bug，删除工具）：tpi prune 的 walk_files 用 path.is_dir()（跟随符号链接），~/.tpi/sessions 或 artifacts 里若有指向外部的符号链接目录，会把外部文件也按过期删除。已修：用 DirEntry.file_type()（不跟随链接）判断，symlink 目录不递归、symlink 文件只删链接本身。回归 walk_files_does_not_follow_symlink_dirs（unix）。
## 29. 迭代第15轮（2026-08-08）
- 斜杠菜单默认项陷阱（P2，真 bug）：命令补全菜单第一项是 /quit，输入 “/” + 回车（默认选中第一项并提交）会直接退出 TPI——探索菜单时误触即退出。已修：SLASH_COMMANDS 重排，help 第一、quit 最后；“/”+回车默认 /help。回归 slash_enter_defaults_to_help_not_quit。
- 空闲 Esc 清空输入（P3，第 4 节记录项落地）：idle 且输入非空时 Esc 清空当前输入（运行中 Esc 仍取消、弹层/菜单优先语义不变）。回归 esc_idle_clears_input。