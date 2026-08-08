//! UI 事件（TPI_TUI_V2_TASK §26）：Terminal / Agent / Tick 统一进单向流。
//!
//! 鼠标事件在 app 层解析（hit-test 需要 Renderer），只把**语义化**结果
//! 送入 reducer（ScrollUp/Down、ClickTool/ClickReasoning），reducer 不依赖
//! 终端或渲染器。

use crate::agent::RuntimeEvent;
use ratatui::crossterm::event::KeyEvent;

/// 进入 reducer 的 UI 事件。
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// 键盘按键（仅 `KeyEventKind::Press`）。
    Key(KeyEvent),
    /// 滚轮向上（5 行）。
    MouseScrollUp,
    /// 滚轮向下（5 行）。
    MouseScrollDown,
    /// 鼠标移动（§24 hover 高亮；命中可点击行时高亮显示）。
    MouseMoved { column: u16, row: u16 },
    /// 应用内选择复制（§用户诉求）：鼠标按下开始拖动选择。
    SelectionStart { row: u16 },
    /// 拖动更新选择范围。
    SelectionUpdate { row: u16 },
    /// 释放鼠标结束选择（选区保留）。
    SelectionEnd,
    /// 鼠标点击命中工具卡片（Renderer hit-test 后）。
    ClickTool(String),
    /// 鼠标点击命中折叠的 reasoning 行（Renderer hit-test 后；EntryId §4.1）。
    ClickReasoning(crate::tui::scroll::EntryId),
    /// 点击/拖拽垂直 scrollbar（§24）：参数是点击行在转录区内的偏移（0-based）。
    ScrollbarClick(u16),
    /// bracketed paste 文本。
    Paste(String),
    /// Agent 运行时事件（模型增量/工具生命周期/上下文用量）。
    Agent(RuntimeEvent),
    /// 动画时钟（spinner）。
    Tick,
}
