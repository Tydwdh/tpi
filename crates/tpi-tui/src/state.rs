use std::collections::{HashMap, VecDeque};

use crate::editor::Editor;
use crate::keymap::Keymap;
use crate::model::ViewModel;

/// 是否抢占型 slash 命令：这类命令会结束/重建/重试/取消当前 run，run 中
/// 提交时必须立即打断 run（不被排队中的普通消息挡住）。其余信息/切换型
/// 命令（help/settings/model/session/sessions/theme/thinking/diff/doctor/mcp）
/// 与 agent turn 无关，run 中提交只排队、run 结束后生效，不得取消 run。
fn is_preemptive_slash(message: &str) -> bool {
    let trimmed = message.trim();
    ["/new", "/cancel", "/compact", "/retry"]
        .iter()
        .any(|cmd| trimmed == *cmd || trimmed.starts_with(&format!("{cmd} ")))
        || trimmed == "/quit"
        || trimmed == "/exit"
}

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
    /// /model 菜单 Enter 选中的模型名（app 重建 provider + 更新 config.model）。
    pub pending_model: Option<String>,
    /// 待重试的上一次失败 turn（`/retry`；app 消费时以空 user_message 发起 run，
    /// 不重复记录 UserSubmitted，也不追加 User 消息）。
    pub pending_retry: Option<String>,
    /// goal 自动续跑信号：非空表示应无用户输入自动再跑一轮（下一轮 round 号）。
    /// 与 pending_retry 相同语义——以空 user_message 发 run，**不追加 User 消息**、
    /// 不写入 history，避免每轮 goal_round 文本累积污染上下文（参考 Claude Code：
    /// 目标作为每 turn 紧凑 condition 注入，绝不进入对话历史）。
    pub pending_goal_continue: Option<u32>,
    /// 是否正在 run（Esc 取消语义、spinner 状态）。
    pub running: bool,
    /// P1-10：手动 /compact 请求（下一次 run 开始时在完整边界压缩一次）。
    pub force_compaction: bool,
    /// 大粘贴占位符存储（§用户诉求）：id → 真实内容。输入框只渲染
    /// `[Pasted Content N chars]` 占位符；提交时经
    /// [`crate::paste::expand_paste_placeholders`] 展开成全文一起发送。
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
            pending_model: None,
            pending_retry: None,
            pending_goal_continue: None,
            running: false,
            force_compaction: false,
            pasted: HashMap::new(),
        }
    }

    /// 存储一段大粘贴内容，返回插入编辑器的用户可见占位符。
    /// 内容在提交、清空或取消 composer 时统一释放；不能中途淘汰旧条目，
    /// 否则屏幕上仍存在的占位符将无法还原。
    pub fn store_paste(&mut self, text: String) -> String {
        let placeholder = crate::paste::next_paste_placeholder(&self.pasted, &text);
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

    /// 请求 goal 自动续跑一轮（下一轮 round 号）。不产生用户消息、不写 history。
    pub fn request_goal_continue(&mut self, round: u32) {
        self.pending_goal_continue = Some(round);
    }

    /// 取出待执行的 goal 续跑信号（消费一次）。
    pub fn take_goal_continue(&mut self) -> Option<u32> {
        self.pending_goal_continue.take()
    }

    /// 是否存在待消费的排队输入（app 主循环据此决定是否可跳过键盘阻塞等待；
    /// BUG-003：`tpi "prompt"` 与 run 结束后排队的消息必须无需按键即可执行）。
    /// §13 修复：/model 菜单选中结果（pending_model）也是待消费输入——
    /// 漏掉它会让主循环阻塞在 key_rx.recv()，切换模型后必须再按一个键才生效。
    /// （`request_input` 模态答案存在 app 层局部变量，由主循环的
    /// modal_answer_pending 条件处理，不在此队列。）
    pub fn has_pending_work(&self) -> bool {
        !self.pending_messages.is_empty()
            || self.pending_session.is_some()
            || self.pending_theme.is_some()
            || self.pending_retry.is_some()
            || self.pending_model.is_some()
            || self.pending_goal_continue.is_some()
    }

    /// 入队一条待提交消息（Enter 提交）。超上限时丢弃最旧并写入系统行提示
    /// （避免无限增长，同时让“消息被丢弃”对用户可见）。
    pub fn push_pending(&mut self, message: String) {
        // ISSUE-026：占位符机制只豁免编辑/上屏阶段，提交时超限仍会被截断——
        // 必须让用户看到截断发生（此前静默截断且无提示）。
        let was_truncated = message.len() > crate::editor::MAX_INPUT_BYTES;
        let message = crate::text::truncate_middle_utf8(
            &message,
            crate::editor::MAX_INPUT_BYTES,
            "\n…[input truncated]…\n",
        );
        if message.is_empty() {
            return;
        }
        if was_truncated {
            self.view.push_line(
                crate::model::LineKind::System,
                format!(
                    "输入超过 {} KiB 上限，已截断中段（首尾保留）",
                    crate::editor::MAX_INPUT_BYTES / 1024
                ),
            );
        }
        if self.pending_messages.len() >= PENDING_CAP {
            let dropped = self.pending_messages.pop_front().unwrap_or_default();
            self.pending_messages.push_back(message);
            self.view.push_line(
                crate::model::LineKind::System,
                format!(
                    "排队消息超过 {PENDING_CAP} 条，最旧消息已丢弃：{}（请等当前 run 结束后再提交）",
                    crate::text::truncate_middle_utf8(&dropped, 160, "…")
                ),
            );
            // ISSUE-052：丢弃分支也要同步 footer 计数（此前 return 前漏调）。
            self.sync_pending_len();
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

    /// 查看队首待提交消息（不移除；§run 中 /命令：reducer 提交后 app 层
    /// 需要知道队首是不是 slash 命令，以便立即取消 run 执行它）。
    pub fn peek_pending(&self) -> Option<&str> {
        self.pending_messages.front().map(String::as_str)
    }

    /// 队列中是否存在需要抢占当前 run 的 `/` 命令（任意位置）。
    ///
    /// 只有会结束/重建/重试/取消 run 的命令（quit/new/cancel/compact/retry）
    /// 才必须立即打断 run——它们是本地即时操作，不被排队中的普通消息挡住。
    /// 其余信息/切换型命令（help/settings/model/session/theme/thinking/diff/
    /// doctor/mcp 等）与 agent turn 无关，run 结束后排队消费即可，**不得**
    /// 因用户只是打开菜单看一眼就取消正在进行的 run。
    pub fn has_pending_slash(&self) -> bool {
        self.pending_messages.iter().any(|m| is_preemptive_slash(m))
    }

    /// 把队列中第一个抢占型 `/` 命令提到队首（app 消费时优先执行）。
    pub fn promote_pending_slash(&mut self) {
        if let Some(idx) = self
            .pending_messages
            .iter()
            .position(|m| is_preemptive_slash(m))
            && let Some(cmd) = self.pending_messages.remove(idx)
        {
            self.pending_messages.push_front(cmd);
        }
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
