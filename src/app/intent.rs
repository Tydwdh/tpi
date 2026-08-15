//! P3-01：`UiIntent` / `AppCommand` / `AppEffect`——surface 与 controller 的语义边界。
//!
//! - [`AppCommand`]：app 层要执行的命令（来自任何 surface：键盘/鼠标/slash/headless）。
//!   不引用 Crossterm/Ratatui——纯语义。
//! - [`UiIntent`]：surface 的语义意图 = `AppCommand` + 来源（P3-02 controller 输入）。
//! - [`AppEffect`]：副作用（渲染/剪贴板/打开 URL/terminal title/file picker），
//!   由 controller 返回、platform adapter 执行（P3-04）。
//!
//! P3-01 验收：recorded input trace 的 command sequence 固定——即给定同一
//! trace（key/scroll/输入），映射出的 `AppCommand` 序列确定不变（测试
//! `command_sequence_is_deterministic`）。

use crate::ids::{RequestId, SessionId};

/// 应用层命令：从任何 surface 来，语义级、平台无关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    /// 提交输入（Enter；slash 命令在此以原文进入，由 app 解释）。
    SubmitInput(String),
    /// 退出 TPI（Ctrl+D / /quit）。
    Quit,
    /// 取消当前 run（Esc 分级：菜单 > 取消 run > 清空输入）。
    CancelRun,
    /// 开始新会话（/new）。
    StartNewSession,
    /// 手动压缩上下文（/compact）。
    CompactNow,
    /// 重试上一次失败/中断的 turn（/retry）。
    RetryLast,
    /// 切换右侧边栏（Ctrl+B）。
    ToggleSidebar,
    /// 切换推理显示（Alt+T）。
    ToggleReasoning,
    /// 打开一个 modal（/mcp、/settings、/help、/session、/sessions、/theme、/diff、/doctor）。
    OpenModal {
        /// 命令名（如 "mcp"、"settings"）；`body` 由命令处理器填充。
        name: String,
    },
    /// 打开最近工具卡片详情（Alt+E）。
    OpenLastTool,
    /// 打开最近失败工具卡片详情（Alt+O）。
    OpenFailedTool,
    /// 打开 transcript 搜索（Ctrl+F）。
    OpenSearch,
    /// 恢复一个历史会话（/sessions 选择）。
    OpenSession(SessionId),
    /// 回答 request_input 挂起问题（用户输入答案）。
    RequestInputAnswer(RequestId, String),
    /// 粘贴多行文本（鼠标中键/粘贴事件）。
    Paste(String),
}

/// surface 语义意图：命令 + 来源（P3-02 controller 输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiIntent {
    pub command: AppCommand,
    /// 来源（诊断/审计用；不影响语义）。
    pub source: IntentSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentSource {
    Keyboard,
    Mouse,
    SlashCommand,
    Headless,
    Paste,
}

impl UiIntent {
    pub fn new(command: AppCommand, source: IntentSource) -> Self {
        Self { command, source }
    }
}

/// 副作用：controller 返回、platform adapter 执行（P3-04 前由 TUI/adapter 处理）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEffect {
    /// 渲染一帧（TUI 特有；headless 忽略）。
    Draw,
    /// 复制文本到剪贴板。
    CopyToClipboard(String),
    /// 打开 URL（/help 文档链接等）。
    OpenUrl(String),
    /// 设置终端标题。
    SetTerminalTitle(String),
    /// 打开文件选择器（返回结果经 request_input 回 controller）。
    OpenFilePicker { filter: Option<String> },
    /// 通知（一次性 toast/状态栏）。
    Notify(String),
}
