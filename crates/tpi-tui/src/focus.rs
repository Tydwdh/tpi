//! P6-01：FocusStack——焦点层级组件。
//!
//! 键盘焦点进入哪个层（modal/overlay/editor/transcript）由栈顶决定；
//! 打开/关闭 popup 时焦点生命周期显式（打开 → 焦点进 popup；关闭 → 焦点回退）。
//!
//! 不变量（property test）：栈顶始终是有效层；pop 后焦点回到栈内剩余层；
//! 空栈 = 根焦点（transcript/editor）。

/// 焦点层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusLayer {
    /// 根：transcript + editor（正常对话）。
    Root,
    /// 详情 overlay（tool/reasoning/link）。
    Overlay,
    /// 操作 modal（/help /settings 等）。
    Modal,
    /// 命令补全菜单。
    Menu,
}

/// 焦点层级栈（root 始终在底）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusStack {
    stack: Vec<FocusLayer>,
}

impl Default for FocusStack {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusStack {
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// 当前焦点层（空栈 = Root）。
    pub fn current(&self) -> FocusLayer {
        self.stack.last().copied().unwrap_or(FocusLayer::Root)
    }

    /// 打开一个层（焦点进入；同层重复打开不重复入栈）。
    pub fn push(&mut self, layer: FocusLayer) {
        if layer == FocusLayer::Root {
            return; // Root 不入栈（它是底）。
        }
        if self.stack.last() == Some(&layer) {
            return; // 幂等
        }
        self.stack.push(layer);
    }

    /// 关闭当前层（焦点回退到栈内剩余层；空栈 = Root）。
    pub fn pop(&mut self) {
        self.stack.pop();
    }

    /// 关闭到指定层（modal 关闭后 overlay 若在下面则焦点回 overlay）。
    pub fn pop_to(&mut self, layer: FocusLayer) {
        while let Some(top) = self.stack.last() {
            if *top == layer {
                return;
            }
            self.stack.pop();
        }
    }

    /// 栈深度（Root 不计）。
    pub fn depth(&self) -> usize {
        self.stack.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基本生命周期：打开 → 焦点进 overlay；关闭 → 焦点回退 root。
    #[test]
    fn focus_lifecycle() {
        let mut focus = FocusStack::new();
        assert_eq!(focus.current(), FocusLayer::Root);
        focus.push(FocusLayer::Overlay);
        assert_eq!(focus.current(), FocusLayer::Overlay);
        focus.push(FocusLayer::Modal);
        assert_eq!(focus.current(), FocusLayer::Modal);
        focus.pop();
        assert_eq!(focus.current(), FocusLayer::Overlay, "pop 后焦点回退");
        focus.pop();
        assert_eq!(focus.current(), FocusLayer::Root);
    }

    /// 幂等：同层重复打开不重复入栈。
    #[test]
    fn push_is_idempotent() {
        let mut focus = FocusStack::new();
        focus.push(FocusLayer::Modal);
        focus.push(FocusLayer::Modal);
        assert_eq!(focus.depth(), 1);
        focus.pop();
        assert_eq!(focus.current(), FocusLayer::Root);
    }

    /// pop_to：modal 关闭后焦点回 overlay（栈内剩余层）。
    #[test]
    fn pop_to_returns_to_underlying_layer() {
        let mut focus = FocusStack::new();
        focus.push(FocusLayer::Overlay);
        focus.push(FocusLayer::Modal);
        focus.push(FocusLayer::Menu);
        focus.pop_to(FocusLayer::Overlay);
        assert_eq!(focus.current(), FocusLayer::Overlay);
    }

    /// 不变量（property）：任意 push/pop 序列后 current() 始终是有效层，
    /// pop 不会使栈负向越界（空栈 = Root）。
    #[test]
    fn invariant_focus_always_valid() {
        let ops: Vec<FocusLayer> = vec![
            FocusLayer::Overlay,
            FocusLayer::Modal,
            FocusLayer::Menu,
            FocusLayer::Overlay,
            FocusLayer::Modal,
        ];
        let mut focus = FocusStack::new();
        for _ in 0..100 {
            for layer in &ops {
                focus.push(*layer);
                assert!(
                    matches!(
                        focus.current(),
                        FocusLayer::Root
                            | FocusLayer::Overlay
                            | FocusLayer::Modal
                            | FocusLayer::Menu
                    ),
                    "焦点始终有效: {:?}",
                    focus.current()
                );
            }
            for _ in 0..ops.len() * 2 {
                focus.pop(); // 越界 pop 安全
                assert!(
                    matches!(
                        focus.current(),
                        FocusLayer::Root
                            | FocusLayer::Overlay
                            | FocusLayer::Modal
                            | FocusLayer::Menu
                    ),
                    "pop 后焦点仍有效"
                );
            }
            assert_eq!(focus.current(), FocusLayer::Root, "清空后回 Root");
        }
    }
}
