# 07. TUI 与人体工程重构计划

## 1. 目标体验

用户始终能回答四个问题：

1. 当前焦点在哪里？
2. 此时输入会做什么？
3. Agent/工具/子代理正在做什么，是否可取消？
4. 新内容到达时视图会不会移动？

TUI 重构不能只以截图漂亮为目标。首要指标是输入动作少、反馈快、焦点可预测、历史阅读稳定、Unicode 正确、长时运行不退化。

## 2. 边界

```text
TerminalAdapter
   -> RawTerminalEvent
   -> InputMapper(keymap/mode/focus)
   -> UiIntent
   -> UiReducer(UiState) -> AppCommand + UiEffect
   -> AppController
   -> semantic RuntimeEvent
   -> ViewProjector
   -> UiState
   -> Components/Layout/Renderer
```

### 2.1 UiReducer 规则

- 纯函数或接近纯函数：输入 state + event，输出 state + effects；
- 不读文件、不启动 process、不调用 agent、不打开 URL；
- event 处理后 invariants 可 assert；
- async result 带 request/generation ID，过期结果不得覆盖新状态；
- view cache 是 state 的派生部分，可丢弃；
- `tui::reducer` 不引用 `app`。

## 3. Component contract

建议最小 contract：

```rust
trait Component {
    fn id(&self) -> ComponentId;
    fn constraints(&self, ctx: LayoutContext) -> ComponentConstraints;
    fn update(&mut self, event: &UiEvent, ctx: &mut UiUpdateContext) -> EventResult;
    fn render(&self, area: Rect, frame: &mut Frame, ctx: &RenderContext);
    fn cursor(&self, area: Rect, ctx: &RenderContext) -> Option<CursorPosition>;
}
```

不要把每个小文本做 trait object。component 是有独立 state/input/layout 生命周期的区域。

### 3.1 建议组件

```text
RootShell
  +-- Header/WorkspaceStatus
  +-- MainSplit
  |     +-- TranscriptPane
  |     |     +-- EntryList
  |     |     +-- ToolCard
  |     |     +-- ChildAgentCard
  |     +-- Sidebar (optional/narrow-hidden)
  +-- Composer
  |     +-- Editor
  |     +-- Autocomplete/Menu
  +-- Footer/RunStatus
  +-- OverlayStack
        +-- SessionPicker
        +-- ToolDetails
        +-- Approval/Input
        +-- Help/Error
```

`ToolCard` 的 domain data 来自 view projection；它不调用 tool runtime。

## 4. Focus 管理

### 4.1 FocusStack

普通焦点是一个 explicit `FocusTarget`；overlay push 时保存 previous focus，pop 时恢复。目标：

```text
Composer <-> Transcript <-> Sidebar
                   |
              Overlay pushed
                   v
              OverlayControl(s)
                   |
              close/cancel
                   v
              exact previous focus
```

规则：

- 没有 popup 时默认 composer；
- mouse click 只在 hit-test 成功时改变 focus；
- resize/scroll 不改变 focus；
- hidden/disabled component 不能持有 focus，layout 变化时选择 deterministic fallback；
- Esc 先关闭最上层 overlay，再取消 transient selection/menu，再按 keymap 决定 cancel run；
- Enter 的行为由 focus/mode 决定，状态栏给出提示；
- Tab/Shift+Tab 使用可测试 focus order，不根据 render 调用顺序偶然决定。

### 4.2 Focus 测试

对每个 overlay 执行：open -> Tab cycle -> mouse background -> resize -> Esc -> assert previous focus。Property test 随机 event sequence 后保证 focus 指向 visible/enabled component。

## 5. Layout

### 5.1 先使用 Ratatui 0.30 Layout/Flex

为每个 component 定义 min/preferred/max/grow/shrink。Root layout 根据宽度进入：

| 模式 | 建议触发 | 行为 |
|---|---|---|
| Narrow | 小于约 70 cells，最终由测试调参 | sidebar 隐藏/overlay，metadata 压缩，composer 保底 |
| Normal | 常用宽度 | transcript + 可选 sidebar |
| Wide | 超宽 | 限制正文最大阅读宽度，额外空间给 metadata/sidebar，不拉长代码行 |

阈值不是 magic constant 散落各文件；集中在 `LayoutPolicy` 并可配置/测试。

### 5.2 Layout invariants

- 所有 rect 在 terminal bounds 内；
- width/height 为 0/1 的极端窗口不 panic/underflow；
- composer 获得硬件光标所需最小行；
- overlay 自动 clamp；
- scrollbar/content viewport 不重叠；
- CJK 标题按 cell truncate；
- resize 保持语义 scroll anchor 和 focus；
- 没有 `u16` 减法 underflow。

只有 Ratatui Layout 无法表达真实 plugin-defined nested UI 时才试验 Taffy。

## 6. Transcript 与滚动

### 6.1 状态模型

```rust
enum ScrollMode {
    FollowEnd,
    Anchored(ScrollAnchor),
}

struct ScrollAnchor {
    entry_id: EntryId,
    logical_row: u32,
    viewport_row: u16,
}
```

不要只保存 `offset_from_bottom`。append、streaming entry 增高、折叠和 resize 都会让纯 offset 无法表达“用户正在读哪一行”。

### 6.2 行为

- 初始/End/滚到底部 -> `FollowEnd`；
- 用户 wheel up/PageUp/Home -> `Anchored`；
- append/stream while Follow -> 继续底部；
- append/stream while Anchored -> 屏幕中的 anchor entry/row 保持；
- user scroll 到新底部阈值 -> 恢复 Follow；
- resize -> 重排后找到相同 semantic anchor；若 entry 删除，选择最近可见 predecessor；
- 展开/折叠工具卡 -> 若操作点在 anchor 上方，补偿高度；
- search jump -> Anchored 到 match，Esc 返回 prior anchor。

### 6.3 Streaming 合并

不要每 token 全量 parse/layout/render。建议：

- runtime delta 按 request/entry 累加；
- render scheduler 最多每 16–33 ms 刷新一次，用户输入/terminal resize/terminal state 立即刷新；
- Markdown incomplete block 使用 incremental-safe fallback；完成消息后完整 parse；
- 每 entry cache key = `(entry_id, content_revision, width, theme, expansion)`；
- delta channel 满时合并相邻 text，不丢 terminal event；
- UI 显示 dropped/coalesced diagnostics 仅在 debug。

## 7. Editor、IME 与 paste

### 7.1 Editor contract

状态：text、grapheme cursor、selection、preferred visual column、undo/redo、history draft、completion state、paste transaction。

动作：insert grapheme/text、delete grapheme/word/line、move grapheme/word/logical/visual line、select、undo/redo、history、submit、newline、cancel completion。

### 7.2 Hardware cursor

render 后 editor 返回 terminal cell cursor。必须：

- 光标在 visible viewport；
- CJK/emoji 使用 cell width；
- combining cluster 不拆；
- overlay/completion 改变位置后同步；
- 非 editor focus 时按需要隐藏或放到 focused input；
- Windows IME composition 不因每帧错误 cursor 抖动。

### 7.3 Bracketed paste

把 paste 当一次 transaction，而不是大量 key presses：

- 开始/结束边界明确；
- payload 有 byte 上限和 UTF-8 validation；
- newline/tab 保留；
- escape-looking 内容不执行快捷键；
- 大 paste 只产生一个 undo unit；
- submit 快捷键不能在 paste 中误触；
- placeholder/预览策略保持现有用户可见语义；
- TUI exit/cancel 时无半 paste state。

Crossterm EventStream spike 若不能保持这些行为就拒绝，继续 blocking adapter 但给线程显式 owner/shutdown。

## 8. 键盘与动作人体工程

建议先建立 action inventory，再决定 keymap：

| 高频动作 | 目标输入数 | 默认行为 |
|---|---:|---|
| 提交普通 prompt | 1 | Enter（multiline 策略需明确） |
| 插入换行 | 1 chord | Shift+Enter 或配置，不与 terminal 支持冲突 |
| 运行中 steering | 1 | Enter 后 receipt，状态标“next step” |
| 运行中 follow-up | 1 chord | Alt+Enter 等，状态标“after current run” |
| 取消最上层 UI | 1 | Esc |
| 取消运行 | 1–2 | Esc 在无 transient UI 时；危险时明确提示 |
| 返回底部 | 1 | End/快捷键 |
| 打开工具详情 | 1 | Enter/click focused card |
| 搜索历史 | 1 chord | 明确 focus/退出恢复 anchor |

终端对 Shift+Enter/Alt+Enter 编码存在差异，必须在 Windows Terminal/ConPTY/SSH 测试；不支持时提供 discoverable fallback，不假装收到不同 key。

## 9. 信息层级

默认 transcript：

- Primary：用户/assistant 正文、需要用户操作的 input/approval、失败终态；
- Secondary：工具名称/target/status、child summary；
- Metadata：duration、exit code、token/cache、provider/model；
- Debug：raw stream、schema、完整环境、内部 retry；默认隐藏且 secret-scrubbed。

工具 running 时立刻出现卡片；完成后状态和 duration 更新。失败不能只用红色，必须有文字/code/下一步。cancelled 与 failed 样式/文本不同。

## 10. Main screen 与 alternate screen

参考 Pi，把 renderer 做成策略：

- **Alt-screen**：app-owned scroll、完整 overlay、稳定布局；
- **Main-screen**：保留终端 scrollback，overlay/局部重绘能力可能受限。

先保留当前默认，Phase 6 做真实终端矩阵后决定是否同时支持。两种 renderer 共享 component/view state，不能复制业务 reducer。

## 11. 性能预算与测量

在固定 hardware/terminal size/fixture 上记录：

| 场景 | 初始目标（基线后调整） |
|---|---|
| idle CPU | 接近 0，不靠高频无条件 tick |
| keypress -> frame | P95 < 50 ms |
| runtime event -> visible state | P95 < 100 ms（render cadence 内） |
| 10k entries resize | P95 < 100 ms 或可见渐进反馈 |
| streaming 100 chunks/s | bounded CPU/memory，无 O(tokens²) |
| 10h transcript/cache | 有上限/逐出，RSS 不线性无限增长 |

目标数必须在 Phase 0 测当前基线后批准；此处不是未经测量的硬承诺。

测量点：layout cache hit、Markdown parse time、render time、changed cells/lines、event queue depth/dropped deltas、entry count/cache bytes、input latency。

## 12. 组件化迁移顺序

1. 记录 input/runtime event traces 和 buffer snapshots。
2. 移除 reducer -> app 反向依赖，纯 projection 移到正确 owner。
3. 建 `UiIntent/AppCommand/UiEffect`，保留现有 render。
4. 建 `FocusStack/OverlayStack` 并迁一个 popup。
5. 抽 `LayoutPolicy`，先保持像素/行 parity。
6. 抽 TranscriptPane + semantic ScrollAnchor；跑全部 scroll/property tests。
7. 抽 Composer/Editor；决定自有 editor vs tui-textarea spike。
8. 引入 render cadence/cache，建立 benchmark 后优化。
9. 试验 Crossterm EventStream；通过 Windows parity 才替换线程。
10. child panel/新 capability UI 只在基础完成后加入。

一次只迁一个 component/owner；每步都能切回旧 renderer/reducer facade。

## 13. 人工验收矩阵

至少在 Windows Terminal + PowerShell、Windows Terminal + SSH、一个 Linux terminal 执行：

- 窗口 20x5、40x10、80x24、160x50；连续 resize；
- 短/长/CJK/emoji/combining/长路径/长命令；
- 快速模型流、快速 bash stdout/stderr；
- 在底部 append、向上阅读时 append、回到底部；
- wheel/PageUp/PageDown/Home/End；
- selection、link/tool click、scrollbar drag；
- popup 嵌套、Esc、Tab/Shift+Tab、mouse focus restore；
- IME 输入、composition、paste 多行、撤销；
- submit 后 composer 立即清空并显示 receipt；
- request input/approval、cancel、过期 answer；
- MCP/child fail 后 UI 不留 running；
- terminal suspend/resume、异常退出后 terminal mode 恢复。

人工结果记录 terminal/version/步骤/预期/实际/screenshot 或 trace；“看起来正常”不是结果。
