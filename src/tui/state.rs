//! UI 状态（TPI_TUI_V2_TASK §25、§48）：UiState 是 UI 单一事实源。
//!
//! 输入事实源 = [`Editor`]（内部 text/cursor/history），`view.input` /
//! `view.input_cursor` 只是渲染投影（P0-5 保留语义，T6 收敛为 ComposerState）。
//! `pending_message` / `pending_session` 是 run 边界排队的输入（§12）。

use crate::tui::editor::Editor;
use crate::tui::model::ViewModel;

/// TUI 全部 UI 状态（app 层唯一持有的状态对象；reducer 只改它）。
#[derive(Debug, Clone)]
pub struct UiState {
    pub view: ViewModel,
    pub editor: Editor,
    /// 排队中的下一条消息（Enter 提交；slash 命令也在此，由 app 消费时解释）。
    pub pending_message: Option<String>,
    /// /sessions 菜单 Enter 选中的 session id（app 执行恢复）。
    pub pending_session: Option<String>,
    /// 是否正在 run（Esc 取消语义、spinner 状态）。
    pub running: bool,
    /// P1-10：手动 /compact 请求（下一次 run 开始时在完整边界压缩一次）。
    pub force_compaction: bool,
}

impl UiState {
    pub fn new(view: ViewModel) -> Self {
        Self {
            view,
            editor: Editor::new(),
            pending_message: None,
            pending_session: None,
            running: false,
            force_compaction: false,
        }
    }

    /// 编辑器变更后同步输入投影（Editor 是输入事实源，view.input 只是投影）。
    pub fn sync_input(&mut self) {
        self.view.input = self.editor.text().to_string();
        self.view.input_cursor = self.editor.cursor;
    }
}
