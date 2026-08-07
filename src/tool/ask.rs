//! `ask_user` 工具（文档 §8.1 P0）。
//!
//! 只有真正阻塞时才请求用户输入；必须独占一个 tool-call batch（§12）。
//! 非交互模式（`-p`）返回 `interactive_input_unavailable`。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::ToolContext;
use crate::tool::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct AskUserArgs {
    pub question: String,
    /// 可选选项（最多 8 项）。
    #[serde(default)]
    pub options: Option<Vec<String>>,
}

pub fn ask_user(args: AskUserArgs, ctx: &ToolContext) -> ToolOutcome {
    if !ctx.interactive {
        return ToolOutcome::failed(
            "ask_user",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: failed\ntool: ask_user\nerror: interactive_input_unavailable\n\n非交互模式无法等待用户输入；请改用交互会话或在 prompt 中给出明确选择。".into(),
                effect: None,
                artifact: None,
            },
        );
    }

    let question = args.question.trim();
    if question.is_empty() {
        return ToolOutcome::failed(
            "ask_user",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: rejected\ntool: ask_user\nerror: empty_question".into(),
                effect: None,
                artifact: None,
            },
        );
    }

    let mut output = format!("status: succeeded\ntool: ask_user\nquestion: {question}\n");
    if let Some(options) = &args.options {
        for (index, option) in options.iter().take(8).enumerate() {
            output.push_str(&format!("{}. {option}\n", index + 1));
        }
    }
    output.push_str("\n请在聊天中回复你的选择或答案。");

    ToolOutcome::succeeded("ask_user", output).with_metadata(ToolMetadata {
        tool: "ask_user".into(),
        target: Some(question.to_string()),
        ..Default::default()
    })
}
