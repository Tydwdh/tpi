//! 交互层（TPI Interaction Refactor Phase 1）：Pointer State Machine + 语义选区。
//!
//! 设计目标：
//! - 统一 idle / run 两条鼠标路径——同一个状态机，绝不再复制一份鼠标逻辑。
//! - Selection 指向**内容**（entry + 逻辑文本偏移），不指向屏幕格子：resize、
//!   rewrap、滚动、折叠都不会改变已选中的内容（对齐 scroll.rs 的 ScrollAnchor 思想）。
//! - Click / Selection / Scrollbar 三类手势在一个状态机内仲裁：Down 只记录，
//!   位移超阈值才进 Selection，Up 时仍在 Pressed 且按下/抬起目标一致才 Click。
//!
//! 本模块不依赖 Renderer：屏幕坐标 → 语义目标的翻译由 app 层通过
//! [`PointerHitTest`] 完成（renderer 提供布局快照）。状态机本身是纯函数，
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

/// 指针命中目标（app 层用 renderer 布局快照解析）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerTarget {
    /// 命中转录区文本（可发起 selection）。
    Transcript(TextPosition),
    /// 命中垂直 scrollbar。
    Scrollbar,
    /// 命中工具卡片（点击展开/收缩）。
    Tool(String),
    /// 命中折叠的 reasoning 行。
    Reasoning(EntryId),
    /// 空白/不可交互区域。
    None,
}

/// 指针状态机的输入（app 层把 crossterm 鼠标事件 + 布局命中合成此值）。
#[derive(Debug, Clone)]
pub enum PointerInput {
    /// 按键按下（仅左键）。
    Down {
        column: u16,
        row: u16,
        target: PointerTarget,
    },
    /// 按住左键移动。
    Drag {
        column: u16,
        row: u16,
        target: PointerTarget,
    },
    /// 释放左键。
    Up {
        column: u16,
        row: u16,
        target: PointerTarget,
    },
    /// 未按键移动（hover；无动作，仅为穷尽路由）。
    Move {
        column: u16,
        row: u16,
        target: PointerTarget,
    },
    /// 滚轮。
    ScrollUp,
    ScrollDown,
}

/// 指针手势状态机。
///
/// 状态转移（§InteractionRefactor）：
/// - `Idle + Down` → `Pressed{origin, target}`（不立即 click/select）。
/// - `Pressed + Drag`：位移超过阈值 → `Selecting`，从**按下点**的 TextPosition
///   开始；按下点在 scrollbar → `DraggingScrollbar`。
/// - `Selecting + Drag`：更新 focus。
/// - `Pressed + Up`：按下/抬起 target 一致 → `Click`；否则无动作。
/// - `Selecting + Up`：结束选择（选区保留），**不再触发 click**。
/// - `DraggingScrollbar + Drag`：滚动；`Up` → 结束。
#[derive(Debug, Clone, Default)]
pub enum PointerGesture {
    #[default]
    Idle,
    Pressed {
        origin_column: u16,
        origin_row: u16,
        target: PointerTarget,
    },
    Selecting {
        anchor: TextPosition,
        focus: TextPosition,
    },
    DraggingScrollbar,
}

/// 拖动进入 selection 的最小位移（cells；Manhattan 距离）。
const DRAG_THRESHOLD: i32 = 2;

impl PointerGesture {
    /// 喂入一个指针事件，返回要发给 reducer 的语义事件。
    pub fn feed(&mut self, input: PointerInput) -> Vec<UiEvent> {
        match input {
            PointerInput::Down {
                column,
                row,
                target,
            } => {
                *self = PointerGesture::Pressed {
                    origin_column: column,
                    origin_row: row,
                    target,
                };
                Vec::new()
            }
            PointerInput::Drag {
                column,
                row,
                target,
            } => match self {
                PointerGesture::Idle => Vec::new(),
                PointerGesture::Pressed {
                    origin_column,
                    origin_row,
                    target: origin_target,
                } => {
                    let (ox, oy) = (*origin_column as i32, *origin_row as i32);
                    let (cx, cy) = (column as i32, row as i32);
                    let moved = (cx - ox).abs() + (cy - oy).abs();
                    if moved <= DRAG_THRESHOLD {
                        return Vec::new();
                    }
                    match origin_target {
                        PointerTarget::Scrollbar => {
                            *self = PointerGesture::DraggingScrollbar;
                            Vec::new()
                        }
                        PointerTarget::Transcript(anchor) => {
                            // 从**按下点**开始选（不是当前位置）。
                            *self = PointerGesture::Selecting {
                                anchor: *anchor,
                                focus: *anchor,
                            };
                            self.feed_drag(column, row, target)
                        }
                        PointerTarget::Tool(_) | PointerTarget::Reasoning(_) => {
                            // 可点击行（工具主行 / 折叠提示）上拖动：进入文本选择。
                            // 以当前拖动位置的文本位置为锚（卡片主行本身不选）；
                            // 若当前不在文本区则不进入选择。
                            match &target {
                                PointerTarget::Transcript(pos) => {
                                    *self = PointerGesture::Selecting {
                                        anchor: *pos,
                                        focus: *pos,
                                    };
                                    self.feed_drag(column, row, target)
                                }
                                _ => Vec::new(),
                            }
                        }
                        PointerTarget::None => Vec::new(),
                    }
                }
                _ => self.feed_drag(column, row, target),
            },
            PointerInput::Up { target, .. } => {
                let origin = match self {
                    PointerGesture::Pressed { target, .. } => Some(target.clone()),
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
                        match (origin, target) {
                            // 按下/抬起目标一致 → 才是一次点击。
                            (Some(PointerTarget::Tool(id)), PointerTarget::Tool(u)) if id == u => {
                                vec![UiEvent::ClickTool(u)]
                            }
                            (Some(PointerTarget::Reasoning(id)), PointerTarget::Reasoning(u))
                                if id == u =>
                            {
                                vec![UiEvent::ClickReasoning(u)]
                            }
                            (Some(PointerTarget::Scrollbar), PointerTarget::Scrollbar) => {
                                // 纯点击 scrollbar 不跳转（由 Drag 处理）。
                                Vec::new()
                            }
                            _ => Vec::new(),
                        }
                    }
                }
            }
            PointerInput::Move { .. } => Vec::new(),
            PointerInput::ScrollUp => vec![UiEvent::MouseScrollUp],
            PointerInput::ScrollDown => vec![UiEvent::MouseScrollDown],
        }
    }

    /// Selecting / DraggingScrollbar 状态下的拖动更新。
    fn feed_drag(&mut self, _column: u16, row: u16, target: PointerTarget) -> Vec<UiEvent> {
        match self {
            PointerGesture::Selecting { anchor, focus } => {
                if let PointerTarget::Transcript(pos) = target {
                    *focus = pos;
                    vec![UiEvent::SelectionUpdate(pos)]
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

    fn down(c: u16, r: u16, t: PointerTarget) -> PointerInput {
        PointerInput::Down {
            column: c,
            row: r,
            target: t,
        }
    }

    fn drag(c: u16, r: u16, t: PointerTarget) -> PointerInput {
        PointerInput::Drag {
            column: c,
            row: r,
            target: t,
        }
    }

    fn up(c: u16, r: u16, t: PointerTarget) -> PointerInput {
        PointerInput::Up {
            column: c,
            row: r,
            target: t,
        }
    }

    /// 轻点工具卡片 → 只有 ClickTool（不产生 selection）。
    #[test]
    fn tap_tool_card_produces_click_not_selection() {
        let mut g = PointerGesture::Idle;
        let target = PointerTarget::Tool("c1".into());
        assert!(g.feed(down(5, 5, target.clone())).is_empty());
        assert!(
            g.feed(up(5, 5, target))
                .contains(&UiEvent::ClickTool("c1".into()))
        );
    }

    /// 在卡片文字上拖动超阈值 → 进入 selection，不触发 click。
    #[test]
    fn drag_on_tool_text_selects_and_does_not_click() {
        let mut g = PointerGesture::Idle;
        let t = PointerTarget::Tool("c1".into());
        let _ = g.feed(down(5, 5, t));
        // 位移超过阈值（3 cells），拖入文本区域。
        let events = g.feed(drag(8, 5, PointerTarget::Transcript(tp(1, 5))));
        assert!(events.contains(&UiEvent::SelectionUpdate(tp(1, 5))));
        // 抬起：结束选择，不是 click。
        let events = g.feed(up(8, 5, PointerTarget::Transcript(tp(1, 5))));
        assert!(events.contains(&UiEvent::SelectionEnd));
        assert!(!events.contains(&UiEvent::ClickTool("c1".into())));
    }

    /// 从按下点开始选，而不是达到阈值的当前点。
    #[test]
    fn selection_starts_at_origin_not_threshold_point() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(10, 5, PointerTarget::Transcript(tp(1, 10))));
        // 第一次超过阈值的 Drag 到 (14,5)：anchor 必须是 (1,10)（按下点），
        // 不是当前位置 (1,14)。
        let events = g.feed(drag(14, 5, PointerTarget::Transcript(tp(1, 14))));
        assert!(events.contains(&UiEvent::SelectionUpdate(tp(1, 14))));
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
        let _ = g.feed(down(14, 5, PointerTarget::Transcript(tp(1, 14))));
        let _ = g.feed(drag(8, 5, PointerTarget::Transcript(tp(1, 8))));
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
        let _ = g.feed(down(5, 5, PointerTarget::Transcript(tp(1, 5))));
        assert!(
            g.feed(drag(6, 5, PointerTarget::Transcript(tp(1, 6))))
                .is_empty()
        );
        assert!(matches!(g, PointerGesture::Pressed { .. }));
    }

    /// scrollbar 按下后拖动 → 持续滚动；抬起结束。
    #[test]
    fn scrollbar_drag_scrolls_and_release_ends() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(79, 10, PointerTarget::Scrollbar));
        // 第一次超过阈值的 Drag：进入 DraggingScrollbar（无事件）。
        let events = g.feed(drag(79, 15, PointerTarget::Scrollbar));
        assert!(events.is_empty(), "进入拖动状态本身不产生事件");
        assert!(matches!(g, PointerGesture::DraggingScrollbar));
        // 后续 Drag：持续滚动。
        let events = g.feed(drag(79, 18, PointerTarget::Scrollbar));
        assert!(events.contains(&UiEvent::ScrollbarClick(18)));
        assert!(g.feed(up(79, 18, PointerTarget::Scrollbar)).is_empty());
        assert!(matches!(g, PointerGesture::Idle));
    }

    /// 抬起在空区域（与按下目标不同）→ 无 click、无 selection。
    #[test]
    fn up_off_target_is_noop() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(5, 5, PointerTarget::Tool("c1".into())));
        let events = g.feed(up(20, 20, PointerTarget::None));
        assert!(events.is_empty());
    }

    /// 纯文本按下→抬起（无位移）不产生任何事件（防误触）。
    #[test]
    fn tap_transcript_text_is_noop() {
        let mut g = PointerGesture::Idle;
        let _ = g.feed(down(5, 5, PointerTarget::Transcript(tp(1, 5))));
        let events = g.feed(up(5, 5, PointerTarget::Transcript(tp(1, 5))));
        assert!(events.is_empty());
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
}
