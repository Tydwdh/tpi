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
    /// 清除当前选区（§用户诉求：新按压开始时清除旧选区——点击其他地方
    /// 即可取消选中；由交互层在 Down 时发出）。
    SelectionClear,
    /// 鼠标点击命中工具卡片（Renderer hit-test 后）。
    ClickTool(String),
    /// 鼠标点击命中折叠的 reasoning 行（Renderer hit-test 后；EntryId §4.1）。
    ClickReasoning(crate::tui::scroll::EntryId),
    /// 鼠标点击命中链接文本（§成熟化；Renderer hit-test 后，参数是 URL）。
    ClickLink(String),
    /// 点击/拖拽垂直 scrollbar（§24）：参数是点击行在转录区内的偏移（0-based）。
    ScrollbarClick(u16),
    /// 侧边栏大纲点击：跳转到对应用户消息（锁定 transcript 到该 entry）。
    SidebarJump(crate::tui::scroll::EntryId),
    /// 侧边栏内部滚动（大纲/todo 过长时；§用户诉求：限定区域 + 滚动条）。
    SidebarScroll(/* up */ bool),
    /// 侧边栏滚动条点击/拖拽：按比例跳转（参数 = 点击行在侧边栏内偏移）。
    SidebarScrollbarClick(u16),
    /// 切换侧边栏开关（§用户诉求；默认 Ctrl+B）。
    ToggleSidebar,
    /// bracketed paste 文本。
    Paste(String),
    /// 旧终端将粘贴拆成逐键流：达到长文本阈值后，把已经插入的精确后缀
    /// `rendered_suffix` 折叠为占位符，全文转入旁路存储。
    CollapseKeyStreamPaste {
        rendered_suffix: String,
        text: String,
    },
    /// 长逐键粘贴折叠后，键盘线程继续旁路收集其余内容；流结束时用完整文本
    /// 更新占位符及存储。`initial_text` 用来精确定位本次折叠，避免串段。
    FinishKeyStreamPaste {
        initial_text: String,
        full_text: String,
    },
    /// Agent 运行时事件（模型增量/工具生命周期/上下文用量）。
    Agent(RuntimeEvent),
    /// 动画时钟（spinner）。
    Tick,
}
