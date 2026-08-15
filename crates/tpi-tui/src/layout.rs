//! P6-02：LayoutPolicy——终端尺寸下的布局决策（不引入 Taffy）。
//!
//! 用 Ratatui Flex/Constraint；对 0/1/narrow/wide 尺寸做 property tests。
//! 决策输出：transcript/sidebar/input 的约束比例。

/// 布局策略：按可用宽度决定侧边栏与主区比例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPolicy {
    /// 超窄（< 40）：无侧边栏，输入全宽。
    Minimal,
    /// 窄（40..80）：侧边栏小比例。
    Narrow,
    /// 常规（80..160）：标准比例。
    Standard,
    /// 超宽（>= 160）：侧边栏大比例。
    Wide,
}

impl LayoutPolicy {
    /// 按可用宽度选择策略。
    pub fn for_width(width: u16) -> Self {
        match width {
            0 => Self::Minimal,
            1..=39 => Self::Minimal,
            40..=79 => Self::Narrow,
            80..=159 => Self::Standard,
            _ => Self::Wide,
        }
    }

    /// 侧边栏宽度（百分比；Minimal = 0）。
    pub fn sidebar_percent(self) -> u16 {
        match self {
            Self::Minimal => 0,
            Self::Narrow => 20,
            Self::Standard => 25,
            Self::Wide => 30,
        }
    }

    /// 主区（transcript）宽度百分比。
    pub fn main_percent(self) -> u16 {
        100 - self.sidebar_percent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单调性：宽度越宽，侧边栏占比不减小（0/1/narrow/wide 全覆盖）。
    #[test]
    fn sidebar_percent_monotonic() {
        let widths = [0u16, 1, 39, 40, 79, 80, 159, 160, 300];
        let mut prev = 0u16;
        for w in widths {
            let policy = LayoutPolicy::for_width(w);
            assert!(
                policy.sidebar_percent() >= prev,
                "w={w}: sidebar {} < 上一档 {prev}",
                policy.sidebar_percent()
            );
            prev = policy.sidebar_percent();
        }
    }

    /// property：所有宽度下主区 + 侧边栏 = 100%（约束总和守恒）。
    #[test]
    fn constraints_always_sum_to_100() {
        for w in 0..=300u16 {
            let policy = LayoutPolicy::for_width(w);
            assert_eq!(
                policy.main_percent() + policy.sidebar_percent(),
                100,
                "w={w}: 约束必须守恒"
            );
        }
    }

    /// 0 宽与 1 宽：Minimal（无侧边栏）。
    #[test]
    fn zero_and_one_width_are_minimal() {
        assert_eq!(LayoutPolicy::for_width(0), LayoutPolicy::Minimal);
        assert_eq!(LayoutPolicy::for_width(1), LayoutPolicy::Minimal);
        assert_eq!(LayoutPolicy::for_width(0).sidebar_percent(), 0);
    }

    /// 边界：39/40、79/80、159/160 分档正确。
    #[test]
    fn boundary_widths_map_correctly() {
        assert_eq!(LayoutPolicy::for_width(39), LayoutPolicy::Minimal);
        assert_eq!(LayoutPolicy::for_width(40), LayoutPolicy::Narrow);
        assert_eq!(LayoutPolicy::for_width(79), LayoutPolicy::Narrow);
        assert_eq!(LayoutPolicy::for_width(80), LayoutPolicy::Standard);
        assert_eq!(LayoutPolicy::for_width(159), LayoutPolicy::Standard);
        assert_eq!(LayoutPolicy::for_width(160), LayoutPolicy::Wide);
    }
}
