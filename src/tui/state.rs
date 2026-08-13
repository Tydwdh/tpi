use std::collections::{HashMap, VecDeque};

use crate::tui::editor::Editor;
use crate::tui::keymap::Keymap;
use crate::tui::model::ViewModel;

/// TUI 全部 UI 状态（app 层唯一持有的状态对象；reducer 只改它）。
#[derive(Debug, Clone)]
pub struct UiState {
    pub view: ViewModel,
    pub editor: Editor,
    /// 生效键位（§成熟化 `[ui.keymap]`）：app 启动时注入，
    /// reducer 按键语义统一经它解析；测试用默认绑定。
    pub keymap: Keymap,
    /// 排队中的下一条消息（Enter 提交；slash 命令也在此，由 app 消费时解释）。
    ///
    /// BUG-005：单槽 Option 会被运行中第二次 Enter 覆盖（第一条消息丢失），
    /// 改为有界队列，app 按提交顺序消费。
    pub pending_messages: VecDeque<String>,
    /// /sessions 菜单 Enter 选中的 session id（app 执行恢复）。
    pub pending_session: Option<String>,
    /// /theme 菜单 Enter 选中的主题名（app 应用主题 + 写配置）。
    pub pending_theme: Option<String>,
    /// 待重试的上一次失败 turn（`/retry`；app 消费时以空 user_message 发起 run，
    /// 不重复记录 UserSubmitted，也不追加 User 消息）。
    pub pending_retry: Option<String>,
    /// 是否正在 run（Esc 取消语义、spinner 状态）。
    pub running: bool,
    /// P1-10：手动 /compact 请求（下一次 run 开始时在完整边界压缩一次）。
    pub force_compaction: bool,
    /// 大粘贴占位符存储（§用户诉求）：id → 真实内容。输入框只渲染
    /// `[Pasted Content N chars]` 占位符；提交时经
    /// [`crate::tui::paste::expand_paste_placeholders`] 展开成全文一起发送。
    /// 真实内容不进 `Editor`，因此不受 `MAX_INPUT_BYTES` 截断，也不分块上屏。
    pub pasted: HashMap<String, String>,
}

/// 排队消息上限（防运行中无限 Enter 导致内存/状态无限增长；超限丢最旧并提示）。
const PENDING_CAP: usize = 16;

impl UiState {
    pub fn new(view: ViewModel) -> Self {
        Self::with_keymap(view, Keymap::builtin())
    }

    /// 以指定键位创建状态（app 从 `[ui.keymap]` 构造；测试默认绑定）。
    pub fn with_keymap(view: ViewModel, keymap: Keymap) -> Self {
        Self {
            view,
            editor: Editor::new(),
            keymap,
            pending_messages: VecDeque::new(),
            pending_session: None,
            pending_theme: None,
            pending_retry: None,
            running: false,
            force_compaction: false,
            pasted: HashMap::new(),
        }
    }

    /// 存储一段大粘贴内容，返回插入编辑器的用户可见占位符。
    /// 内容在提交、清空或取消 composer 时统一释放；不能中途淘汰旧条目，
    /// 否则屏幕上仍存在的占位符将无法还原。
    pub fn store_paste(&mut self, text: String) -> String {
        let placeholder = crate::tui::paste::next_paste_placeholder(&self.pasted, &text);
        self.pasted.insert(placeholder.clone(), text);
        placeholder
    }

    /// 请求重试上一次失败 turn（`/retry`）。
    pub fn push_retry(&mut self, target: String) {
        self.pending_retry = Some(target);
    }

    /// 取出待重试的消息（消费一次）。
    pub fn take_pending_retry(&mut self) -> Option<String> {
        self.pending_retry.take()
    }

    /// 是否存在待消费的排队输入（app 主循环据此决定是否可跳过键盘阻塞等待；
    /// BUG-003：`tpi "prompt"` 与 run 结束后排队的消息必须无需按键即可执行）。
    pub fn has_pending_work(&self) -> bool {
        !self.pending_messages.is_empty()
            || self.pending_session.is_some()
            || self.pending_theme.is_some()
            || self.pending_retry.is_some()
    }

    /// 入队一条待提交消息（Enter 提交）。超上限时丢弃最旧并写入系统行提示
    /// （避免无限增长，同时让“消息被丢弃”对用户可见）。
    pub fn push_pending(&mut self, message: String) {
        let message = crate::tui::text::truncate_middle_utf8(
            &message,
            crate::tui::editor::MAX_INPUT_BYTES,
            "\n…[input truncated]…\n",
        );
        if message.is_empty() {
            return;
        }
        if self.pending_messages.len() >= PENDING_CAP {
            let dropped = self.pending_messages.pop_front().unwrap_or_default();
            self.pending_messages.push_back(message);
            self.view.push_line(
                crate::tui::model::LineKind::System,
                format!(
                    "排队消息超过 {PENDING_CAP} 条，最旧消息已丢弃：{}（请等当前 run 结束后再提交）",
                    crate::tui::text::truncate_middle_utf8(&dropped, 160, "…")
                ),
            );
            return;
        }
        self.pending_messages.push_back(message);
        self.sync_pending_len();
    }

    /// 取出下一条待提交消息（FIFO）。
    pub fn pop_pending(&mut self) -> Option<String> {
        let item = self.pending_messages.pop_front();
        self.sync_pending_len();
        item
    }

    /// 把队列长度同步到视图（footer“已排队 N”提示）。
    fn sync_pending_len(&mut self) {
        self.view.pending_queue_len = self.pending_messages.len();
    }
    /// 编辑器变更后同步输入投影（Editor 是输入事实源，view.input 只是投影）。
    pub fn sync_input(&mut self) {
        self.view.input = self.editor.text().to_string();
        self.view.input_cursor = self.editor.cursor;
    }
}
