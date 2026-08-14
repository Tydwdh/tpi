# Recorded UI Trace Corpus（P0-04）

## 目标

用**录制的事件序列**（而非代码内联构造）验证 TUI reducer 行为：输入 trace
（key/mouse/paste/focus）与 runtime trace（assistant/tool/process）按固定
terminal sizes 回放，断言终态语义。回放**不依赖 wall clock / network**。

这是 P6（TUI 组件化）的回归网：组件化必须保持这些 trace 的语义不变
（P6-01 逐 overlay 迁移时用 recorded trace parity 验收）。

## 录制格式

每个 fixture 是一个 JSONL 文件，**每行一个事件**，时间戳一律省略（回放逐行
立即应用，不依赖时序）：

```json
{"kind":"key","code":"char","char":"h"}          // 键盘
{"kind":"key","code":"enter"}
{"kind":"key","code":"pageup"}
{"kind":"key","code":"ctrl_end"}                  // Ctrl+End
{"kind":"scroll","dir":"up","lines":8}            // 鼠标滚轮（语义化）
{"kind":"scroll","dir":"down","lines":8}
{"kind":"paste","text":"..."}                     // bracketed paste
{"kind":"agent","delta":"line1\nline2\n"}         // runtime: assistant delta
```

支持的事件 kind（当前子集；扩展时更新本文件与回放映射）：

| kind | 字段 | 映射 |
|---|---|---|
| `key` | `code` + 可选 `char`/`ctrl` | `UiEvent::Key(KeyEvent)` |
| `scroll` | `dir` = up/down, `lines` | `UiEvent::MouseScrollUp/Down` |
| `paste` | `text` | `UiEvent::Paste` |
| `agent` | `delta` | `UiEvent::Agent(AssistantDelta)` |

## 回放协议

`tests/tui_trace_replay.rs`：

1. 读 `manifest.json` 拿到 fixture 列表与每个 fixture 的 `size`（固定终端尺寸）
   与 `assert`（终态断言类型）；
2. 对每个 fixture：构造初始 `UiState`（`UiState::new(ViewModel)`），逐行读 trace、
   反序列化为 `UiEvent`、依次 `reducer::update(&mut state, event)`；
3. 断言终态（见下）。

断言类型（manifest `assert` 字段）：

| 断言 | 含义 |
|---|---|
| `pending_messages` | 断言 `state.pending_messages` 精确等于 expected（manifest `expected` 数组） |
| `scroll_mode_follow` | 断言回放结束后 `state.view.scroll_mode == Follow`（确定性终态；滚动细节由 tui_scroll 资产覆盖） |
| `no_panic` | 仅断言回放可执行完成（结构骨架用） |

## manifest schema

```json
{
  "schema": 1,
  "size": {"width": 120, "height": 40},
  "traces": [
    {
      "id": "001_input_queue",
      "file": "001_input_queue.jsonl",
      "assert": "pending_messages",
      "expected": ["hi", "a"]
    },
    {
      "id": "002_scroll",
      "file": "002_scroll.jsonl",
      "assert": "scroll_mode_follow"
    }
  ]
}
```

## 录制方法

- 键盘/鼠标/粘贴：真实 TUI 会话中记录 `UiEvent` 序列（未来 P6 提供录制器；
  当前 fixture 由手工构造 + 回放测试验证）；
- runtime trace：从真实 session 事件流投影出 `Agent` 事件（P1-03 后
  runtime/view event 分离，可直接复用）。

## 扩展方向（P6 前不实现）

- focus/modal/overlay 打开-关闭-取消序列（P6-01 FocusStack/OverlayStack 的
  recorded parity）；
- resize 事件（固定多组 size，reducer 对每组合法）；
- keymap 变化（不同 keymap 下同一 trace 语义不变）。
