//! `request_input` 工具（AGENTS.md §13：真正的 Suspend / Resume 原语）。
//!
//! 模型需要用户决定时调用它：run 在工具执行点**挂起**（不是结束），
//! session 记录 `UserInputRequested`，控制权交回 TUI；用户回答后
//! 记录 `UserInputReceived`，后续 run 带着完整历史继续。
//!
//! 对标 Claude Code `AskUserQuestion` 的形态：一次调用可携带**多个问题**
//! （每个问题可带 `header` 分组标题与 `options` 建议选项），避免多次
//! 挂起-恢复往返。
//!
//! §9：旧兼容格式（`question` + `options`、`QuestionOption::Plain`）已删除。
//! 模型必须使用结构化的 `questions` 数组。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::ToolContext;
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};

/// `request_input` 参数。
///
/// §9：只接受 `questions` 数组作为唯一顶层表示。旧兼容格式（`question` +
/// `options`）已删除——模型必须使用 `questions` 数组。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct RequestInputArgs {
    /// 问题列表：每个问题可带 `header`（分组标题）与 `options`（建议选项）。
    /// 至少需要一个问题。
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
    pub options: Vec<QuestionOption>,
    /// 允许多选（checkbox 式；默认 false）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multiple: bool,
    /// 允许自定义回答（默认 true；false 时只能选选项）。
    #[serde(default = "default_true", skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
}

fn default_true() -> bool {
    true
}

/// §9：单个建议选项——只接受结构化 `{label, description}` 格式。
/// 旧兼容格式（裸字符串）已删除。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl QuestionOption {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

impl RequestInputArgs {
    /// §9：返回规范化的 `questions` 列表。为空时返回 `None`（调用方按
    /// invalid_arguments 拒绝）。旧兼容字段（`question`/`options`）已删除。
    pub fn normalized_questions(&self) -> Option<Vec<RequestInputQuestion>> {
        if !self.questions.is_empty() {
            return Some(self.questions.clone());
        }
        None
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
            if question.multiple {
                line.push_str("（可多选）");
            }
            lines.push(line);
            if !question.options.is_empty() {
                for (option_index, option) in question.options.iter().enumerate() {
                    let mut opt_line =
                        format!("   {}. {}", option_index + 1, option.label().trim());
                    if !option.description().trim().is_empty() {
                        opt_line.push_str(&format!(" — {}", option.description().trim()));
                    }
                    lines.push(opt_line);
                }
                if question.custom {
                    lines.push(format!("   {}. （自定义回答）", question.options.len() + 1));
                }
            }
        }
        lines.join("\n")
    }
}

/// 工具入口：成功即表示“请求已发出”，run 将挂起等待用户输入。
///
/// 非交互 run（`-p`/web：`ctx.interactive == false`）没有用户可答：
/// 挂起后 run 永远等不到输入。这里直接拒绝，让模型基于现有信息继续，
/// 而不是产生一个无人能恢复的挂起（batch 执行器只对 Succeeded 挂起）。
pub async fn request_input(args: RequestInputArgs, ctx: &ToolContext) -> ToolOutcome {
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
    if !ctx.interactive {
        return rejected(
            "status: rejected\ntool: request_input\nerror: unavailable_in_non_interactive_run\n\n当前 run 非交互（无用户可答）：request_input 不可用。请基于已有信息继续完成任务；确实缺少关键决定时，在最终回复中说明缺少的输入及其影响。"
                .into(),
        );
    }
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
