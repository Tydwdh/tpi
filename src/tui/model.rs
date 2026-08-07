//! TUI 渲染模型（§16.2 信息层级）。
//!
//! ViewModel 是只读渲染输入：由 app 从 ephemeral 事件构建，
//! renderer 不直接访问 Agent/tool 内部状态。

use crate::tool::plan::Plan;

/// 转录行类型（§16.2：普通工具可见、plan call 隐藏由 UI 策略决定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    System,
}

/// 状态栏内容（§16.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusLine {
    Idle,
    Running { turn: u32, tool: String },
    Compacting,
}

/// 只读渲染输入（§16.1：renderer 的唯一输入）。
#[derive(Debug, Clone)]
pub struct ViewModel {
    pub transcript: Vec<TranscriptLine>,
    pub input: String,
    pub input_cursor: usize,
    pub plan: Option<Plan>,
    pub status: StatusLine,
    pub model_name: String,
    /// 当前正在运行的 turn 数。
    pub turn: u32,
}

impl Default for ViewModel {
    fn default() -> Self {
        Self {
            transcript: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            plan: None,
            status: StatusLine::Idle,
            model_name: "?".into(),
            turn: 0,
        }
    }
}

impl ViewModel {
    /// 输入行与光标位置（编辑器渲染用）。
    pub fn input_position(&self) -> (String, usize) {
        (self.input.clone(), self.input_cursor.min(self.input.len()))
    }

    /// 追加转录行。
    pub fn push_line(&mut self, kind: LineKind, text: impl Into<String>) {
        // §16.4：普通工具调用可见；update_plan 不进入聊天流水（只显示 plan widget）。
        self.transcript.push(TranscriptLine {
            kind,
            text: text.into(),
        });
        // 有界转录（防止长会话内存无限增长）。
        if self.transcript.len() > 2000 {
            let keep = self.transcript.len() - 2000;
            self.transcript.drain(..keep);
        }
    }
}
