//! `request_input` 工具（AGENTS.md §13：真正的 Suspend / Resume 原语）。
//!
//! 模型需要用户决定时调用它：run 在工具执行点**挂起**（不是结束），
//! session 记录 `UserInputRequested`，控制权交回 TUI；用户回答后
//! 记录 `UserInputReceived`，后续 run 带着完整历史继续。
//!
//! 对标 Claude Code `AskUserQuestion` 的形态：一次调用可携带**多个问题**
//! （每个问题可带 `header` 分组标题与 `options` 建议选项），避免多次
//! 挂起-恢复往返；同时保留旧单问题格式（`question` + `options`）兼容。
//!
//! 挂起信号不在工具 outcome 里（工具本身返回 Succeeded“已请求输入”），
//! 而由 batch 执行器检测本工具成功调用 → 以 `SuspendRequested` 结束 batch。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::outcome::{ModelPayload, ToolOutcome, ToolStatus};
use crate::tool::ToolContext;

/// `request_input` 参数。
///
/// 主路径是 `questions` 数组（一次请求多个问题，各带 header/options）；
/// `question` + `options` 是旧单问题格式的兼容快捷方式，两者同时提供时
/// `questions` 优先。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RequestInputArgs {
    /// 兼容旧调用：单个问题快捷方式（等价于 `questions=[{question}]`，
    /// `options` 作为该问题的建议选项）。新调用建议使用 `questions` 数组；
    /// 两者同时提供时 `questions` 优先。
    #[serde(default)]
    pub question: Option<String>,
    /// 兼容旧调用：单问题（`question` 字段）的建议选项。用户可直接选择或
    /// 输入自定义内容；与 `questions` 同时提供时被忽略。
    #[serde(default)]
    pub options: Vec<String>,
    /// 问题列表：每个问题可带 `header`（分组标题）与 `options`（建议选项）。
    /// 为空时回退到 `question` 字段构造单个问题。
    #[serde(default)]
    pub questions: Vec<RequestInputQuestion>,
}

/// 单个待提问的问题。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RequestInputQuestion {
    /// 向用户提出的问题（TUI 显示并等待回答）。
    pub question: String,
    /// 可选分组标题（如“部署目标”），渲染文本与 TUI 中展示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// 建议选项：用户可直接选择（可按编号引用）或输入自定义内容。
    #[serde(default)]
    pub options: Vec<String>,
}

impl RequestInputArgs {
    /// 规范化问题列表：`questions` 非空时使用它；否则用兼容字段
    /// （`question` + `options`）构造单个问题。没有任何问题时返回 `None`
    /// （调用方按 invalid_arguments 拒绝）。
    pub fn normalized_questions(&self) -> Option<Vec<RequestInputQuestion>> {
        if !self.questions.is_empty() {
            return Some(self.questions.clone());
        }
        self.question.as_ref().map(|question| {
            vec![RequestInputQuestion {
                question: question.clone(),
                header: None,
                options: self.options.clone(),
            }]
        })
    }

    /// 渲染为模型/用户可见的多行问题文本（编号 + header + 选项）。
    ///
    /// 空参数（没有可渲染的问题）返回空字符串；调用方应先经
    /// [`Self::normalized_questions`] 校验。每个问题的渲染文本使用
    /// `trim()` 后的内容（与工具入口校验一致）。
    pub fn render(&self) -> String {
        let Some(questions) = self.normalized_questions() else {
            return String::new();
        };
        let mut lines = Vec::new();
        for (index, question) in questions.iter().enumerate() {
            let mut line = format!("{}. ", index + 1);
            if let Some(header) = question.header.as_deref().filter(|h| !h.trim().is_empty()) {
                line.push('[');
                line.push_str(header.trim());
                line.push_str("] ");
            }
            line.push_str(question.question.trim());
            lines.push(line);
            if !question.options.is_empty() {
                let options: Vec<String> = question
                    .options
                    .iter()
                    .enumerate()
                    .map(|(option_index, option)| format!("{}. {}", option_index + 1, option))
                    .collect();
                lines.push(format!("   选项: {}", options.join(" / ")));
            }
        }
        lines.join("\n")
    }
}

/// 工具入口：成功即表示“请求已发出”，run 将挂起等待用户输入。
pub async fn request_input(args: RequestInputArgs, _ctx: &ToolContext) -> ToolOutcome {
    let rejected = |output: String| {
        ToolOutcome::failed(
            "request_input",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output,
                effect: None,
                artifact: None,
            },
        )
    };
    let Some(questions) = args.normalized_questions() else {
        return rejected(
            "status: rejected\ntool: request_input\nerror: invalid_arguments\n\nquestion 不能为空：需要明确向用户提出的问题。".into(),
        );
    };
    if questions.iter().any(|q| q.question.trim().is_empty()) {
        return rejected(
            "status: rejected\ntool: request_input\nerror: invalid_arguments\n\nquestions 中每个 question 都不能为空：需要明确向用户提出的问题。".into(),
        );
    }
    let rendered = args.render();
    let text = format!(
        "status: succeeded\ntool: request_input\nquestion: {rendered}\n\n已请求用户输入：run 已挂起，等待用户回答后继续。"
    );
    ToolOutcome::succeeded("request_input", text)
}
