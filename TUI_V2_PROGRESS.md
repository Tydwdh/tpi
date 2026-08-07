# TUI v2 进度与 Task 1 数据流 Inventory

> 依据：TPI_TUI_V2_TASK.md。每阶段完成在此记录验收断言。

## 当前数据流（Task 1，T0 产出）

### 终端事件路径（空闲态）

```
crossterm event::read（独立线程）
  → mpsc channel(key_tx, 128) → key_rx
  → interactive_loop（app.rs）select 分支
    → handle_key(key, &mut editor, &mut view, &mut pending, &mut pending_session)
      编辑操作全走 Editor（输入事实源，P0-5 投影：sync_input 复制到 view.input/input_cursor）
    → refresh_menus(&mut view)（/ 斜杠菜单、@ 文件菜单）
    → renderer.draw(&view)
```

### Agent 事件路径（run 中）

```
agent::run → ui_tx（RuntimeEvent channel，128）
  → run_interactive（app.rs）select 分支
    → view.push_stream_delta / begin_tool / finish_tool / append_tool_output
      / context_usage / turn / plan / budget_warning
    → renderer.should_draw()（16ms 帧合并）→ renderer.draw(&view)
  ticker 100ms → view.anim_tick（spinner）
  键盘分支：Esc 关 overlay → Esc 取消 run → 其余同 handle_key
```

### 渲染路径

```
renderer.draw(&view)
  → terminal.draw(|frame| render_frame(frame, view, theme, md_cache, ...))
    → plan_window(view, width, height)：
        窗口 = 距底部 transcript_scroll 的最后 area_h 个「逻辑行」
        （wrap 前按 entry 展开；跟随模式 scroll==0）
    → build_transcript_text / cached_markdown（version 缓存，宽度变化清空）
    → draw_plan / draw_footer / draw_input（硬件光标）/ draw_overlay / draw_menu
  → 闭合行经 insert_before 提交到 inline scrollback（不支持则降级内部滚动）
```

### 状态层现状（TUI v2 要迁移的边界）

| 现状 | 问题（任务书对照） |
|---|---|
| `ViewModel`（model.rs）单一结构：transcript/input/menu/overlay/scroll/usage 全混合 | §7 要求 Transcript/Live/Composer/Status/Modal 分离 |
| `transcript_scroll: u16` 距尾部「逻辑行」数 | §3-4 要求 ScrollMode::Follow/Locked + EntryId anchor；逻辑行 ≠ visual 行（§4.2） |
| `Vec<Entry>` + trim 2000 | §5-6 要求稳定 EntryId（trim 会改 index） |
| Editor（editor.rs）与 view.input 双状态 + sync_input 投影 | §25 要求 ComposerState 唯一事实源 |
| 每帧 build_transcript_text 全量 + wrap | §36-39 要求 layout cache + 每帧只布局可见区 |
| `interactive_loop`/`run_interactive`/`handle_key`（app.rs 大函数） | §26-27 要求 Event → Reducer → Effect |
| Renderer 内嵌 terminal 生命周期（mod.rs） | §29-31 要求独立 TerminalDriver + panic restore |
| 默认 `Viewport::Inline` | §1 要求默认 Fullscreen |
| Tool 卡片 overlay 已有（整改 B） | §17 保持：历史 tool 详情仅 overlay |
| 帧合并 16ms 固定 | §35 要求按 dirty 类型分档（streaming 16-33ms / spinner 80-120ms / idle 不重绘） |

### 现有可复用资产

- overlay（滚动、Esc、PgUp/PgDn、hit-target、Alt+[/] 切换、失败卡 Alt+O）
- pending_below 计数 + follow_tail（scroll==0 语义）
- Markdown 渲染缓存（entry version → Vec<Line>，宽度失效）
- scroll lock 期间新输出不拉回底部（整改 C）
- TestBackend 渲染测试（draw_to_test_backend）+ 大量 model 单测
- `@` 文件索引（后台扫描 2000 有界）、/ 斜杠菜单

---

## 阶段进度

- [x] T0 baseline：fmt/clippy/test 全绿；Task 1 inventory 完成
- [x] T1 UTF-8 P0：text.rs helper + 3 处 panic 修复（b7dd44f）
- [x] T2 TerminalDriver + 默认 Fullscreen（1a38d12）
- [x] T3 UiState/Reducer 单向流（66f55a3）
- [x] T4 Scroll Engine：Follow/Locked(EntryId+row) + §58 四场景（52663dd）
- [x] T5 Transcript/Live 分离（f9bfcf3）
- [x] T6 Composer v2：logical line/preferred column/Shift+Enter/Ctrl+J（62ef287）
- [x] T7 Modal：/help /settings /doctor /session /sessions /diff 出 transcript（69bdabe）
- [x] T8 Search（Ctrl+F）+ Alt+Up/Down user turn 跳转（61bbeff）
- [x] T9 Polish：§16 提示文案 + §64 回归补充（5c38674）

## §64 回归清单核对（自动化覆盖）

- [x] resize 不 panic / 80x24 布局 / 极小终端降级（tui_fullscreen）
- [x] CJK/emoji 不 panic、safe truncation（tui_utf8 + text.rs）
- [x] Locked 不被新输出拉回 / resize 保持 anchor / End 恢复（tui_scroll A-D）
- [x] PageUp/PageDown 按 viewport 移动 / wheel 更新 anchor（tui_scroll）
- [x] tool overlay 不改变 anchor（tui_scroll）
- [x] modal close 后保持原位置（tui_rework）
- [x] 500 deltas/s 不按 delta draw（tui_streaming frame_coalescing）
- [x] composer multiline cursor / paste 多行（editor + tui_streaming）
- [x] run 中输入响应 / cancel 回 idle（reducer + 架构性）

## 人工验收剩余（§55/§75）

- [ ] 真实终端启动占满/resize/退出恢复（TerminalDriver 已实现 + 自动化布局验证）
- [ ] §75 最终 18 场景（30-60 分钟连续使用）
