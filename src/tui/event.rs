//! UI 事件（TPI_TUI_V2_TASK §26）：Terminal / Agent / Tick 统一进单向流。
//!
//! 鼠标事件在 app 层解析（hit-test 需要 Renderer），只把**语义化**结果
//! 送入 reducer（ScrollUp/Down、ClickTool/ClickReasoning、Selection 的语义
//! 位置），reducer 不依赖终端或渲染器。

use crate::agent::RuntimeEvent;
use crate::tui::interaction::TextPosition;
use ratatui::crossterm::event::KeyEvent;

/// 进入 reducer 的 UI 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// 键盘按键（仅 `KeyEventKind::Press`）。
    Key(KeyEvent),
    /// 滚轮向上（5 行）。
    MouseScrollUp,
    /// 滚轮向下（5 行）。
    MouseScrollDown,
    /// 鼠标移动（§用户诉求：移除 hover 悬浮高亮——事件保留为穷尽占位）。
    MouseMoved { column: u16, row: u16 },
    /// 应用内选择复制（§用户诉求）：鼠标按下开始拖动选择。
    /// 携带**语义位置**（entry + 逻辑偏移），不依赖屏幕坐标。
    SelectionStart(TextPosition),
    /// 拖动更新选择范围。
    SelectionUpdate(TextPosition),
    /// 释放鼠标结束选择（选区保留）。
    SelectionEnd,
    /// 鼠标点击命中工具卡片（Renderer hit-test 后）。
    ClickTool(String),
    /// 鼠标点击命中折叠的 reasoning 行（Renderer hit-test 后；EntryId §4.1）。
    ClickReasoning(crate::tui::scroll::EntryId),
    /// 鼠标点击命中链接文本（§成熟化；Renderer hit-test 后，参数是 URL）。
    ClickLink(String),
    /// 点击/拖拽垂直 scrollbar（§24）：参数是点击行在转录区内的偏移（0-based）。
    ScrollbarClick(u16),
    /// bracketed paste 文本。
    Paste(String),
    /// Agent 运行时事件（模型增量/工具生命周期/上下文用量）。
    Agent(RuntimeEvent),
    /// 动画时钟（spinner）。
    Tick,
}
