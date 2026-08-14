//! `request_input` 工具（AGENTS.md §13：真正的 Suspend / Resume 原语）。
//!
//! 模型需要用户决定时调用它：run 在工具执行点**挂起**（不是结束），
//! session 记录 `UserInputRequested`，控制权交回 TUI；用户回答后
//! 记录 `UserInputReceived`，后续 run 带着完整历史继续。
//!
//! 挂起信号不在工具 outcome 里（工具本身返回 Succeeded“已请求输入”），
//! 而由 batch 执行器检测本工具成功调用 → 以 `SuspendRequested` 结束 batch。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::outcome::{ModelPayload, ToolOutcome, ToolStatus};
use crate::tool::ToolContext;

/// `request_input` 参数（第一版只支持自由文本回答；`options` 仅作提示）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RequestInputArgs {
    /// 向用户提出的问题（TUI 显示并等待回答）。
    pub question: String,
    /// 可选建议选项（仅提示，用户可自由输入）。
    #[serde(default)]
    pub options: Vec<String>,
}

/// 工具入口：成功即表示“请求已发出”，run 将挂起等待用户输入。
pub async fn request_input(args: RequestInputArgs, _ctx: &ToolContext) -> ToolOutcome {
    let question = args.question.trim();
    if question.is_empty() {
        return ToolOutcome::failed(
            "request_input",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: rejected\ntool: request_input\nerror: invalid_arguments\n\nquestion 不能为空：需要明确向用户提出的问题。".into(),
                effect: None,
                artifact: None,
            },
        );
    }
    let mut text = format!(
        "status: succeeded\ntool: request_input\nquestion: {question}\n\n已请求用户输入：run 已挂起，等待用户回答后继续。"
    );
    if !args.options.is_empty() {
        text.push_str("\n建议选项：");
        text.push_str(&args.options.join(" / "));
    }
    ToolOutcome::succeeded("request_input", text)
}
