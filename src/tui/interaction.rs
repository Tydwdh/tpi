//! 交互层（TPI Interaction Refactor Phase 2）：Pointer State Machine + 语义选区。
//!
//! 设计目标：
//! - 统一 idle / run 两条鼠标路径——同一个状态机，绝不再复制一份鼠标逻辑。
//! - Selection 指向**内容**（entry + 逻辑文本偏移），不指向屏幕格子：resize、
//!   rewrap、滚动、折叠都不会改变已选中的内容（对齐 scroll.rs 的 ScrollAnchor 思想）。
//! - 一个终端 cell **可以同时是**可选择文本 + 可点击控件（§PointerHit 组合式）：
//!   轻点 Tool header → Click；在 Tool header 的文字上拖动 → Selection。
//! - Click / Selection / Scrollbar 三类手势在一个状态机内仲裁：Down 只记录，
//!   位移超阈值才进 Selection，Up 时仍在 Pressed 且按下/抬起 action 一致才 Click。
//!
//! 本模块不依赖 Renderer：屏幕坐标 → 语义目标的翻译由 app 层通过
//! [`PointerHit`] 完成（renderer 提供布局快照）。状态机本身是纯函数，
//! 输入解析后的 [`PointerInput`]，输出语义化 [`UiEvent`]。

use crate::tui::event::UiEvent;
use crate::tui::scroll::EntryId;

/// 语义文本位置：指向某个 transcript entry 的逻辑文本偏移（char 边界）。
///
/// 与屏幕坐标无关：同一个位置在任意宽度/滚动状态下都指向同一段内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextPosition {
    pub entry_id: EntryId,
    /// entry 逻辑文本中的 char 偏移。
    pub offset: usize,
}

/// 语义选区：anchor（按下点）+ focus（当前点），顺序无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: TextPosition,
    pub focus: TextPosition,
}

impl TextSelection {
    /// 规范化起止：anchor/focus 按 (entry_id, offset) 排序，供复制/高亮使用。
    pub fn normalized(self) -> (TextPosition, TextPosition) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

/// 可点击动作（§PointerHit：文本与动作可共存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerAction {
    /// 工具卡片（点击展开/收缩）。
    Tool(String),
    /// 折叠的 reasoning 行。
    Reasoning(EntryId),
    /// 链接文本（点击打开 Link Overlay；§成熟化）。
    Link(String),
    /// 侧边栏用户消息大纲行（点击跳转到该 User 消息）。
    SidebarJump(EntryId),
}

/// 命中区域类别（决定手势路由：scrollbar 拖拽 vs 文本选择 vs 无动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerRegion {
    /// 转录区（可发起文本选择）。
    Transcript,
    /// 垂直 scrollbar。
    Scrollbar,
    /// 右侧边栏（§用户诉求）：点击大纲行跳转；拖拽不发起文本选择。
    Sidebar,
    /// 其它（header/footer/空白/弹层）。
    Other,
}

/// 一次 hit 的组合结果（§PointerHit：一个 cell 既是文本又可能是可点击控件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerHit {
    /// 该位置的语义文本位置（`None` = 非转录文本，不可选择）。
    pub text: Option<TextPosition>,
    /// 该位置的可点击动作（`None` = 无动作）。
    pub action: Option<PointerAction>,
    /// 命中区域。
    pub region: PointerRegion,
}

impl PointerHit {
    /// 纯文本位置（转录区，无动作）。
    pub fn text_at(position: TextPosition) -> Self {
        Self {
            text: Some(position),
            action: None,
            region: PointerRegion::Transcript,
        }
    }

    /// 可点击 + 可选择的文本（工具主行 / reasoning 折叠行）。
    pub fn actionable(position: TextPosition, action: PointerAction) -> Self {
        Self {
            text: Some(position),
            action: Some(action),
            region: PointerRegion::Transcript,
        }
    }

    /// scrollbar。
    pub fn scrollbar() -> Self {
        Self {
            text: None,
            action: None,
            region: PointerRegion::Scrollbar,
        }
    }

    /// 侧边栏大纲行（§用户诉求）：有动作、无文本选择、区域 = Sidebar。
    pub fn sidebar_jump(entry: EntryId) -> Self {
        Self {
            text: None,
            action: Some(PointerAction::SidebarJump(entry)),
            region: PointerRegion::Sidebar,
        }
    }

    /// 侧边栏空白（无动作，区域 = Sidebar）。
    pub fn sidebar_blank() -> Self {
        Self {
            text: None,
            action: None,
            region: PointerRegion::Sidebar,
        }
    }

    /// 空白/不可交互区域。
    pub fn none() -> Self {
        Self {
            text: None,
            action: None,
            region: PointerRegion::Other,
        }
    }
}

/// 指针状态机的输入（app 层把 crossterm 鼠标事件 + 布局命中合成此值）。
#[derive(Debug, Clone)]
pub enum PointerInput {
    /// 按键按下（仅左键）。
    Down {
        column: u16,
        row: u16,
        hit: PointerHit,
    },
    /// 按住左键移动。
    Drag {
        column: u16,
        row: u16,
        hit: PointerHit,
    },
    /// 释放左键。
    Up {
        column: u16,
        row: u16,
        hit: PointerHit,
    },
    /// 未按键移动（hover；无动作，仅为穷尽路由）。
    Move {
        column: u16,
        row: u16,
        hit: PointerHit,
    },
    /// 滚轮。
    ScrollUp,
    ScrollDown,
}

/// 指针手势状态机。
///
/// 状态转移（§PointerHit）：
/// - `Idle + Down` → `Pressed{origin_column, origin_row, origin_hit}`（不立即动作）。
/// - `Pressed + Drag`：位移超过阈值 →
///   - 按下点在 scrollbar → `DraggingScrollbar`；
///   - 否则按下点有文本位置 → `Selecting`，从**按下点**的 TextPosition 开始
///     （此时不管当前位置是否 Tool action——文本与动作可共存）。
/// - `Selecting + Drag`：更新 focus。
/// - `Pressed + Up`：按下/抬起 action 一致 → `Click`（未进入 Selecting 时）。
/// - `Selecting + Up`：结束选择（选区保留），**不再触发 click**。
/// - `DraggingScrollbar + Drag`：滚动；`Up` → 结束。
#[derive(Debug, Clone, Default)]
pub enum PointerGesture {
    #[default]
    Idle,
    Pressed {
        origin_column: u16,
        origin_row: u16,
        origin_hit: PointerHit,
    },
    Selecting {
        anchor: TextPosition,
        focus: TextPosition,
    },
    DraggingScrollbar,
}

/// 拖动进入 selection 的最小位移（cells；Manhattan 距离）。
/// §PointerHit：2（原 3）——用户反馈拖选不灵敏；「点击」是零位移，不触发
/// 选择，误触仅限 3 cells 的拖动（那本就是拖动而非点击），可接受。
const DRAG_THRESHOLD: i32 = 2;

impl PointerGesture {
    /// 喂入一个指针事件，返回要发给 reducer 的语义事件。
    /// （无视口边界：拖选不会触发自动滚动——测试与无视口场景。）
    pub fn feed(&mut self, input: PointerInput) -> Vec<UiEvent> {
        self.feed_in_viewport(input, None)
    }

    /// 带视口矩形喂入（§成熟化：拖选自动滚动需要转录区边界）。
    /// `Selecting` 状态下拖出视口上下边缘 → 附带 `MouseScrollUp/Down`
    /// （视口滚动后选区继续扩展，拖选可跨越当前屏幕）。
    pub fn feed_in_viewport(
        &mut self,
        input: PointerInput,
        viewport: Option<ratatui::layout::Rect>,
    ) -> Vec<UiEvent> {
        match input {
            PointerInput::Down { column, row, hit } => {
                *self = PointerGesture::Pressed {
                    origin_column: column,
                    origin_row: row,
                    origin_hit: hit,
                };
                // §用户诉求：任何新按压先清除旧选区——点击其他地方即可取消选中
                //（若随后拖选，SelectionStart 会建立新选区）。
                vec![UiEvent::SelectionClear]
            }
            PointerInput::Drag { column, row, hit } => match self {
                PointerGesture::Idle => Vec::new(),
                PointerGesture::Pressed {
                    origin_column,
                    origin_row,
                    origin_hit,
                } => {
                    let (ox, oy) = (*origin_column as i32, *origin_row as i32);
                    let (cx, cy) = (column as i32, row as i32);
                    let moved = (cx - ox).abs() + (cy - oy).abs();
                    if moved <= DRAG_THRESHOLD {
                        return Vec::new();
                    }
                    match origin_hit.region {
                        PointerRegion::Scrollbar => {
                            *self = PointerGesture::DraggingScrollbar;
                            Vec::new()
                        }
                        PointerRegion::Sidebar => {
                            // 侧边栏：拖拽不发起文本选择（纯控件区）；保持 Pressed，
                            // 抬起时若位移未超阈值 → Click（跳转）。
                            Vec::new()
                        }
                        PointerRegion::Transcript => {
                            // 按下点在转录区：从**按下点**的文本位置开始选。
                            // （tool 主行同时是文本+动作——拖动即选择，不点动作。）
                            if let Some(anchor) = origin_hit.text {
                                let current = hit;
                                let mut events = self.begin_selecting(anchor, current);
                                // 首次超阈值拖动已越出视口边缘：一并自动滚动。
                                if let Some(rect) = viewport {
                                    if row < rect.y {
                                        events.push(UiEvent::MouseScrollUp);
                                    } else if row >= rect.y + rect.height {
                                        events.push(UiEvent::MouseScrollDown);
                                    }
                                }
                                events
                            } else {
                                Vec::new()
                            }
                        }
                        PointerRegion::Other => Vec::new(),
                    }
                }
                _ => self.feed_drag(column, row, hit, viewport),
            },
            PointerInput::Up { hit, .. } => {
                let origin = match self {
                    PointerGesture::Pressed { origin_hit, .. } => Some(origin_hit.clone()),
                    _ => None,
                };
                match self {
                    PointerGesture::Selecting { .. } => {
                        *self = PointerGesture::Idle;
                        vec![UiEvent::SelectionEnd]
                    }
                    PointerGesture::DraggingScrollbar => {
                        *self = PointerGesture::Idle;
                        Vec::new()
                    }
                    PointerGesture::Idle => Vec::new(),
                    PointerGesture::Pressed { .. } => {
                        *self = PointerGesture::Idle;
                        // 未进入 Selecting：按下/抬起 action 一致 → 才是一次点击。
                        match (origin.and_then(|o| o.action), hit.action) {
                            (Some(PointerAction::Tool(id)), Some(PointerAction::Tool(u)))
                                if id == u =>
                            {
                                vec![UiEvent::ClickTool(u)]
                            }
                            (
                                Some(PointerAction::Reasoning(id)),
                                Some(PointerAction::Reasoning(u)),
                            ) if id == u => {
                                vec![UiEvent::ClickReasoning(u)]
                            }
                            (Some(PointerAction::Link(url)), Some(PointerAction::Link(u)))
                                if url == u =>
                            {
                                // §成熟化：链接文本轻点 → 打开 Link Overlay。
                                vec![UiEvent::ClickLink(u)]
                            }
                            (
                                Some(PointerAction::SidebarJump(id)),
                                Some(PointerAction::SidebarJump(u)),
                            ) if id == u => {
                                vec![UiEvent::SidebarJump(u)]
                            }
                            _ => Vec::new(),
                        }
                    }
                }
            }
            // §用户诉求：移除 hover 悬浮高亮——未按键移动不再上报悬停态，
            // 卡片不做瞬态提亮（点击展开仍是唯一交互）。
            PointerInput::Move { .. } => Vec::new(),
            PointerInput::ScrollUp => vec![UiEvent::MouseScrollUp],
            PointerInput::ScrollDown => vec![UiEvent::MouseScrollDown],
        }
    }

    /// Selecting / DraggingScrollbar 状态下的拖动更新。
    fn feed_drag(
        &mut self,
        _column: u16,
        row: u16,
        hit: PointerHit,
        viewport: Option<ratatui::layout::Rect>,
    ) -> Vec<UiEvent> {
        match self {
            PointerGesture::Selecting { anchor, focus } => {
                if let Some(pos) = hit.text {
                    *focus = pos;
                    let mut events = vec![UiEvent::SelectionUpdate(pos)];
                    // §成熟化：拖出视口上下边缘 → 自动滚动（选区持续扩展跨越屏幕）。
                    // 每次越界滚一档；持续拖动持续滚。
                    if let Some(rect) = viewport {
                        if row < rect.y {
                            events.push(UiEvent::MouseScrollUp);
                        } else if row >= rect.y + rect.height {
                            events.push(UiEvent::MouseScrollDown);
                        }
                    }
                    events
                } else {
                    let _ = anchor;
                    Vec::new()
                }
            }
            PointerGesture::DraggingScrollbar => {
                // app 层根据当前 row 计算滚动偏移，走 ScrollbarClick 语义。
                vec![UiEvent::ScrollbarClick(row)]
            }
            _ => Vec::new(),
        }
    }

    /// 进入 Selecting 状态：设置 anchor/focus，并发出 `SelectionStart(anchor)` +
    /// （若当前位置是文本）`SelectionUpdate(focus)`。
    ///
    /// P0：SelectionStart 必须有 producer——否则 reducer 端 `view.selection`
    /// 永远为 None，Ctrl+C 复制与高亮全部失效。
    fn begin_selecting(&mut self, anchor: TextPosition, current: PointerHit) -> Vec<UiEvent> {
        let mut events = vec![UiEvent::SelectionStart(anchor)];
        let focus = match current.text {
            Some(pos) => {
                events.push(UiEvent::SelectionUpdate(pos));
                pos
            }
            None => anchor,
        };
        *self = PointerGesture::Selecting { anchor, focus };
        events
    }
}

/// 终端 cell 列 → 文本 char 偏移（§InteractionRefactor：CJK/emoji 按 cell 宽度
/// 精确定位，不做 `chars().take(c)` 近似）。`text` 是该行的语义文本，
/// `column` 是 0-based 终端列。返回该列起始处的 char 索引（越界返回文本长度）。
///
/// ASCII：1 char = 1 cell；中文/emoji：1 char = 2 cells；零宽字符 = 0 cell。
/// 例：`abc你好xyz` 中 cell 3 是「你」的第 1 个 cell → char 索引 3。
pub fn cell_to_char(text: &str, column: usize) -> usize {
    let mut char_off = 0usize;
    let mut cell_off = 0usize;
    for ch in text.chars() {
        let w = crate::tui::text::char_cell_width(ch);
        if cell_off + w > column {
            break;
        }
        cell_off += w;
        char_off += 1;
    }
    char_off
}

/// 文本行前 `char_count` 个字符占用的 cell 宽度（§InteractionRefactor：
/// 复制/高亮时把 char 范围投影回 cell 范围）。
pub fn chars_to_cells(text: &str, char_count: usize) -> usize {
    text.chars()
        .take(char_count)
        .map(crate::tui::text::char_cell_width)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tp(entry: u64, offset: usize) -> TextPosition {
        TextPosition {
            entry_id: EntryId(entry),
            offset,
        }
    }

    fn text(c: u16, r: u16, pos: TextPosition) -> PointerInput {
        PointerInput::Down {
            column: c,
            row: r,
            hit: PointerHit::text_at(pos),
        }
    }

    fn down(c: u16, r: u16, h: PointerHit) -> PointerInput {
        PointerInput::Down {
            column: c,
            row: r,
            hit: h,
        }
    }

    fn drag(c: u16, r: u16, h: PointerHit) -> PointerInput {
        PointerInput::Drag {
            column: c,
            row: r,
            hit: h,
        }
    }

    fn up(c: u16, r: u16, h: PointerHit) -> PointerInput {
        PointerInput::Up {
            column: c,
            row: r,
            hit: h,
        }
    }

    /// 轻点工具卡片 → 只有 ClickTool（不产生 selection）。
    #[test]
    fn tap_tool_card_produces_click_not_selection() {
        let mut g = PointerGesture::Idle;
        let action = PointerAction::Tool("c1".into());
        let hit = PointerHit::actionable(tp(1, 0), action);
        // Down：清除旧选区（§用户诉求），不产生 selection 本身。
        assert_eq!(
            g.feed(down(5, 5, hit.clone())),
            vec![UiEvent::SelectionClear]
        );
        assert!(
            g.feed(up(5, 5, hit))
                .contains(&UiEvent::ClickTool("c1".into()))
        );
    }

    /// 在卡片文字上拖动超阈值 → 进入 selection（文本+动作共存），不触发 click。
    #[test]
    fn drag_on_tool_text_selects_and_does_not_click() {
        let mut g = PointerGesture::Idle;
        let action = PointerAction::Tool("c1".into());
        let origin = PointerHit::actionable(tp(1, 5), action);
        let _ = g.feed(down(5, 5, origin));
        // 位移超过阈值（4 cells > 3），拖入文本区域（动作与文本可共存）。
        let events = g.feed(drag(9, 5, PointerHit::text_at(tp(1, 5))));
        assert!(
            events.contains(&UiEvent::SelectionStart(tp(1, 5))),
            "拖动必须发 SelectionStart: {events:?}"
        );
        assert!(events.contains(&UiEvent::SelectionUpdate(tp(1, 5))));
        // 抬起：结束选择，不是 click。
        let events = g.feed(up(9, 5, PointerHit::text_at(tp(1, 5))));
        assert!(events.contains(&UiEvent::SelectionEnd));
        assert!(!events.contains(&UiEvent::ClickTool("c1".into())));
    }

    /// 从按下点开始选，而不是达到阈值的当前点。
    #[test]
    fn selection_starts_at_origin_not_threshold_point() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(10, 5, PointerHit::text_at(tp(1, 10))));
        // 第一次超过阈值的 Drag 到 (14,5)：anchor 必须是 (1,10)（按下点），
        // 不是当前位置 (1,14)。
        let events = g.feed(drag(14, 5, PointerHit::text_at(tp(1, 14))));
        assert!(events.contains(&UiEvent::SelectionUpdate(tp(1, 14))));
        assert!(events.contains(&UiEvent::SelectionStart(tp(1, 10))));
        match &g {
            PointerGesture::Selecting { anchor, .. } => {
                assert_eq!(*anchor, tp(1, 10), "anchor 必须是按下点");
            }
            other => panic!("应进入 Selecting: {other:?}"),
        }
    }

    /// 反向拖动：anchor 保持按下点，focus 更新到反向位置。
    #[test]
    fn reverse_drag_keeps_anchor_and_updates_focus() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(14, 5, PointerHit::text_at(tp(1, 14))));
        let _ = g.feed(drag(8, 5, PointerHit::text_at(tp(1, 8))));
        match &g {
            PointerGesture::Selecting { anchor, focus } => {
                assert_eq!(*anchor, tp(1, 14));
                assert_eq!(*focus, tp(1, 8));
            }
            other => panic!("应进入 Selecting: {other:?}"),
        }
        let sel = TextSelection {
            anchor: tp(1, 14),
            focus: tp(1, 8),
        };
        assert_eq!(sel.normalized(), (tp(1, 8), tp(1, 14)));
    }

    /// 阈值内位移 → 仍是 Pressed，不误入 selection。
    #[test]
    fn sub_threshold_drag_does_not_select() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(5, 5, PointerHit::text_at(tp(1, 5))));
        assert!(g.feed(drag(6, 5, PointerHit::text_at(tp(1, 6)))).is_empty());
        assert!(matches!(g, PointerGesture::Pressed { .. }));
    }

    /// scrollbar 按下后拖动 → 持续滚动；抬起结束。
    #[test]
    fn scrollbar_drag_scrolls_and_release_ends() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(79, 10, PointerHit::scrollbar()));
        // 第一次超过阈值的 Drag：进入 DraggingScrollbar（无事件）。
        let events = g.feed(drag(79, 15, PointerHit::scrollbar()));
        assert!(events.is_empty(), "进入拖动状态本身不产生事件");
        assert!(matches!(g, PointerGesture::DraggingScrollbar));
        // 后续 Drag：持续滚动。
        let events = g.feed(drag(79, 18, PointerHit::scrollbar()));
        assert!(events.contains(&UiEvent::ScrollbarClick(18)));
        assert!(g.feed(up(79, 18, PointerHit::scrollbar())).is_empty());
        assert!(matches!(g, PointerGesture::Idle));
    }

    /// 抬起在空区域（与按下动作不同）→ 无 click、无 selection。
    #[test]
    fn up_off_target_is_noop() {
        let mut g = PointerGesture::Idle;
        let action = PointerAction::Tool("c1".into());
        let _ = g.feed(down(5, 5, PointerHit::actionable(tp(1, 0), action)));
        let events = g.feed(up(20, 20, PointerHit::none()));
        assert!(events.is_empty());
    }

    /// 纯文本按下→抬起（无位移）不产生任何事件（防误触）。
    #[test]
    fn tap_transcript_text_is_noop() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(text(5, 5, tp(1, 5)));
        let events = g.feed(up(5, 5, PointerHit::text_at(tp(1, 5))));
        assert!(events.is_empty());
    }

    /// §成熟化：Selecting 拖出视口下边缘 → 附带 MouseScrollDown（自动滚动）。
    #[test]
    fn drag_below_viewport_auto_scrolls_down() {
        use ratatui::layout::Rect;
        let mut g = PointerGesture::Idle;
        let viewport = Rect::new(0, 0, 80, 20);
        let _ = g.feed_in_viewport(down(5, 5, PointerHit::text_at(tp(1, 5))), Some(viewport));
        // 超过阈值并拖到视口下方（row 25 ≥ y+height=20）。
        let events =
            g.feed_in_viewport(drag(9, 25, PointerHit::text_at(tp(1, 25))), Some(viewport));
        assert!(
            events.contains(&UiEvent::SelectionUpdate(tp(1, 25))),
            "焦点仍更新: {events:?}"
        );
        assert!(
            events.contains(&UiEvent::MouseScrollDown),
            "拖出下边缘必须自动下滚: {events:?}"
        );
        assert!(!events.contains(&UiEvent::MouseScrollUp));
    }

    /// §成熟化：Selecting 拖出视口上边缘 → 附带 MouseScrollUp（向上看更早内容）。
    #[test]
    fn drag_above_viewport_auto_scrolls_up() {
        use ratatui::layout::Rect;
        let mut g = PointerGesture::Idle;
        let viewport = Rect::new(0, 10, 80, 20);
        let _ = g.feed_in_viewport(down(5, 15, PointerHit::text_at(tp(1, 15))), Some(viewport));
        let events = g.feed_in_viewport(drag(9, 8, PointerHit::text_at(tp(1, 8))), Some(viewport));
        assert!(events.contains(&UiEvent::SelectionUpdate(tp(1, 8))));
        assert!(
            events.contains(&UiEvent::MouseScrollUp),
            "拖出上边缘必须自动上滚: {events:?}"
        );
        assert!(!events.contains(&UiEvent::MouseScrollDown));
    }

    /// §成熟化：视口内拖动不产生滚动事件（回归）。
    #[test]
    fn drag_inside_viewport_does_not_scroll() {
        use ratatui::layout::Rect;
        let mut g = PointerGesture::Idle;
        let viewport = Rect::new(0, 0, 80, 20);
        let _ = g.feed_in_viewport(down(5, 5, PointerHit::text_at(tp(1, 5))), Some(viewport));
        let events =
            g.feed_in_viewport(drag(9, 10, PointerHit::text_at(tp(1, 10))), Some(viewport));
        assert!(events.contains(&UiEvent::SelectionUpdate(tp(1, 10))));
        assert!(!events.contains(&UiEvent::MouseScrollUp));
        assert!(!events.contains(&UiEvent::MouseScrollDown));
    }

    /// §成熟化：链接文本轻点 → ClickLink（按下/抬起 action 一致才触发）。
    #[test]
    fn tap_link_produces_click_link() {
        let mut g = PointerGesture::Idle;
        let action = PointerAction::Link("https://example.com".into());
        let hit = PointerHit::actionable(tp(1, 3), action);
        // Down：清除旧选区（§用户诉求）。
        assert_eq!(
            g.feed(down(5, 5, hit.clone())),
            vec![UiEvent::SelectionClear]
        );
        let events = g.feed(up(5, 5, hit));
        assert!(
            events.contains(&UiEvent::ClickLink("https://example.com".into())),
            "链接轻点必须产生 ClickLink: {events:?}"
        );
    }

    /// §成熟化：链接文本上拖动（超阈值）→ 进入选择，不触发 ClickLink。
    #[test]
    fn drag_on_link_selects_and_does_not_click() {
        let mut g = PointerGesture::Idle;
        let action = PointerAction::Link("https://example.com".into());
        let _ = g.feed(down(5, 5, PointerHit::actionable(tp(1, 5), action)));
        let events = g.feed(drag(10, 5, PointerHit::text_at(tp(1, 10))));
        assert!(events.contains(&UiEvent::SelectionStart(tp(1, 5))));
        let up_events = g.feed(up(10, 5, PointerHit::text_at(tp(1, 10))));
        assert!(!up_events.contains(&UiEvent::ClickLink("https://example.com".into())));
        assert!(up_events.contains(&UiEvent::SelectionEnd));
    }

    /// §用户诉求：移除 hover 悬浮高亮——未按键移动不产生任何事件。
    #[test]
    fn move_without_button_produces_no_events() {
        let mut g = PointerGesture::Idle;
        let action = PointerAction::Tool("c1".into());
        let hit = PointerHit::actionable(tp(1, 0), action);
        let events = g.feed(PointerInput::Move {
            column: 5,
            row: 5,
            hit,
        });
        assert!(
            events.is_empty(),
            "移动不得上报 hover（已移除悬浮高亮）: {events:?}"
        );
        let events = g.feed(PointerInput::Move {
            column: 5,
            row: 5,
            hit: PointerHit::none(),
        });
        assert!(events.is_empty(), "移出卡片同样不产生事件: {events:?}");
    }

    /// §InteractionRefactor：cell → char 映射必须按 cell 宽度而非字符数。
    /// `abc你好xyz`：a/b/c=1cell，你/好=2cells，x/y/z=1cell。
    #[test]
    fn cell_to_char_maps_cjk_by_cell_width() {
        let text = "abc你好xyz";
        // cell 0-2 → char 0-2（abc）。
        assert_eq!(cell_to_char(text, 0), 0);
        assert_eq!(cell_to_char(text, 2), 2);
        // cell 3-4 → char 3（你，占 2 cells）。
        assert_eq!(cell_to_char(text, 3), 3);
        assert_eq!(cell_to_char(text, 4), 3);
        // cell 5-6 → char 4（好）。
        assert_eq!(cell_to_char(text, 5), 4);
        assert_eq!(cell_to_char(text, 6), 4);
        // cell 7 → char 5（x）。
        assert_eq!(cell_to_char(text, 7), 5);
        // 越界 → 文本长度（8 个字符）。
        assert_eq!(cell_to_char(text, 999), 8);
    }

    /// chars_to_cells：char 范围 → cell 宽度（CJK 计 2）。
    #[test]
    fn chars_to_cells_sums_cjk_width() {
        assert_eq!(chars_to_cells("abc", 3), 3);
        assert_eq!(chars_to_cells("abc你好", 5), 3 + 4);
        assert_eq!(chars_to_cells("你好", 1), 2);
        assert_eq!(chars_to_cells("你好", 0), 0);
    }

    /// 混合文本（ASCII + CJK + emoji）的 cell 映射。
    #[test]
    fn cell_to_char_handles_emoji_and_mixed() {
        // 👨‍💻 是 ZWJ 序列：👨(2) + ZWJ(0) + 💻(2)。此处只验证非零宽组合的基本点：
        let text = "a你😀b";
        // a=1, 你=2, 😀=2, b=1 → cell 3-4 是 😀。
        assert_eq!(cell_to_char(text, 3), 2);
        assert_eq!(cell_to_char(text, 5), 3);
    }

    /// P0②：端到端——Gesture → reducer → ViewModel.selection。
    /// 只测 Gesture 会漏掉「SelectionStart 无 producer → selection 恒 None」。
    #[test]
    fn drag_through_reducer_sets_view_selection() {
        use crate::tui::model::ViewModel;
        use crate::tui::reducer;
        use crate::tui::state::UiState;
        let mut state = UiState::new(ViewModel::default());
        let mut g = PointerGesture::default();
        let p1 = TextPosition {
            entry_id: EntryId(1),
            offset: 10,
        };
        let p5 = TextPosition {
            entry_id: EntryId(1),
            offset: 14,
        };
        // Down（记录 Pressed + 清除旧选区）。
        assert_eq!(
            g.feed(down(10, 5, PointerHit::text_at(p1))),
            vec![UiEvent::SelectionClear]
        );
        // Drag 超阈值 → begin_selecting 发出 SelectionStart + SelectionUpdate。
        let events = g.feed(drag(14, 5, PointerHit::text_at(p5)));
        assert!(
            events.contains(&UiEvent::SelectionStart(p1)),
            "进入 Selecting 必须发 SelectionStart(anchor): {events:?}"
        );
        for event in events {
            let _ = reducer::update(&mut state, event);
        }
        // 关键断言：经过 reducer 后 view.selection 必须非 None。
        assert!(
            state.view.selection.is_some(),
            "端到端链路必须让 view.selection 非 None（否则 Ctrl+C 复制永远失效）"
        );
        let sel = state.view.selection.unwrap();
        assert_eq!(sel.anchor, p1);
        assert_eq!(sel.focus, p5);
    }

    /// §用户诉求：已有选区时再次按下（点击其他地方）→ 发 SelectionClear，
    /// reducer 据此清除旧选区（点击别处取消选中）。
    #[test]
    fn new_press_clears_previous_selection() {
        let mut g = PointerGesture::default();
        // 第一次：按下 + 拖动超阈值 → 选中，抬起保留选区。
        let _ = g.feed(down(5, 5, PointerHit::text_at(tp(1, 5))));
        let _ = g.feed(drag(9, 5, PointerHit::text_at(tp(1, 9))));
        let _ = g.feed(up(9, 5, PointerHit::text_at(tp(1, 9))));
        assert!(matches!(g, PointerGesture::Idle));
        // 第二次：点击别处 → Down 必须发 SelectionClear。
        let events = g.feed(down(20, 10, PointerHit::text_at(tp(2, 0))));
        assert!(
            events.contains(&UiEvent::SelectionClear),
            "新按压必须清除旧选区: {events:?}"
        );
        assert!(matches!(g, PointerGesture::Pressed { .. }));
    }

    /// §用户诉求：侧边栏大纲行轻点 → 只发 SidebarJump（不产生 selection），
    /// 且拖拽超阈值不触发跳转（纯控件区，不进入文本选择）。
    #[test]
    fn tap_sidebar_outline_jumps_without_selection() {
        let mut g = PointerGesture::Idle;
        let entry = EntryId(42);
        let hit = PointerHit::sidebar_jump(entry);
        // Down：清除旧选区（与其它区域一致），不产生 selection。
        assert_eq!(
            g.feed(down(60, 5, hit.clone())),
            vec![UiEvent::SelectionClear]
        );
        // 抬起（无位移）→ 只发 SidebarJump。
        let events = g.feed(up(60, 5, hit));
        assert!(
            events.contains(&UiEvent::SidebarJump(entry)),
            "轻点大纲行必须发 SidebarJump: {events:?}"
        );
        assert!(!events.contains(&UiEvent::SelectionStart(TextPosition {
            entry_id: EntryId(0),
            offset: 0
        })));
    }

    /// §用户诉求：侧边栏内拖拽超阈值不发起文本选择（无 text，纯控件区）。
    #[test]
    fn drag_in_sidebar_does_not_select() {
        let mut g = PointerGesture::Idle;
        let entry = EntryId(7);
        let _ = g.feed(down(60, 5, PointerHit::sidebar_jump(entry)));
        // 拖拽超阈值（4 > 2）：Sidebar 区域不进入 Selecting。
        let events = g.feed(drag(64, 5, PointerHit::sidebar_blank()));
        assert!(
            !events.contains(&UiEvent::SelectionStart(TextPosition {
                entry_id: EntryId(0),
                offset: 0
            })),
            "Sidebar 拖拽不得发起文本选择: {events:?}"
        );
        assert!(matches!(g, PointerGesture::Pressed { .. }));
        // 抬起（位移超阈值 → 非 click）→ 不跳转。
        let events = g.feed(up(64, 5, PointerHit::sidebar_blank()));
        assert!(
            !events.contains(&UiEvent::SidebarJump(entry)),
            "拖拽后抬起不得触发跳转: {events:?}"
        );
    }
}
