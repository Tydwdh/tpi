//! UI 效果（TPI_TUI_V2_TASK §27）：reducer 只改状态，跨边界动作以
//! effect 返回，由 app 层执行。

/// Reducer 返回的跨边界效果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEffect {
    /// 退出 TUI（/quit、/exit）。
    Quit,
    /// 取消当前 run（Esc 在 run 中、Ctrl-C 语义）。
    CancelRun,
    /// 恢复指定 session（/sessions 菜单 Enter 选中）。
    ResumeSession(String),
}
