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
    /// 复制选中文本到剪贴板（§用户诉求：Ctrl+C 有选区时复制；无选区忽略）。
    CopySelection,
    /// Link Overlay 内确认打开 URL（§成熟化：仅显式用户动作；app 层校验 scheme）。
    OpenUrl(String),
    /// 复制 URL 到剪贴板（§成熟化：Link Overlay 内 `c`）。
    CopyText(String),
}
