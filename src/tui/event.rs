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
    /// 滚轮向上（3 行）。
    MouseScrollUp,
    /// 滚轮向下（3 行）。
    MouseScrollDown,
    /// 鼠标点击命中工具卡片（Renderer hit-test 后）。
    ClickTool(String),
    /// 鼠标点击命中折叠的 reasoning 行（Renderer hit-test 后；EntryId §4.1）。
    ClickReasoning(crate::tui::scroll::EntryId),
    /// bracketed paste 文本。
    Paste(String),
    /// Agent 运行时事件（模型增量/工具生命周期/上下文用量）。
    Agent(RuntimeEvent),
    /// 动画时钟（spinner）。
    Tick,
}
