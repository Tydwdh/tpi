//! 文件工具（文档 §10）：`read`（M1）；`list`/`search` 属 M2。
//!
//! `read` 结果必须分别给出 path、revision、returned_lines、total_lines、truncated
//! 和正文（§10.2）。正文统一 LF，因此模型复制出的 `old_text` 与匹配空间一致。

use crate::tool::edit::{self, EditError};
use crate::tool::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};
use crate::tool::{ToolContext, resolve_workspace_path};
use camino::Utf8PathBuf;
use schemars::JsonSchema;
use serde::Deserialize;

/// `read` 默认模型预算（§8.4：200 行，最多 32 KiB）。
pub const DEFAULT_READ_LINES: usize = 200;
pub const DEFAULT_READ_MAX_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ReadArgs {
    pub path: String,
    /// 起始行号（1-indexed）。
    #[serde(default = "default_start_line")]
    pub start_line: usize,
    /// 最多返回行数（默认 200）。
    #[serde(default = "default_line_count")]
    pub line_count: usize,
}

fn default_start_line() -> usize {
    1
}

fn default_line_count() -> usize {
    DEFAULT_READ_LINES
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

pub fn read(args: ReadArgs, ctx: &ToolContext) -> ToolOutcome {
    // §10.1：read 在合理文件大小下保存完整 snapshot（stale 诊断用）。
    // §8.4：模型通过 opaque `@artifact/<session>/<id>` 有界读取完整输出。
    if let Some(reference) = args.path.strip_prefix("@artifact/") {
        let Some((session_id, id)) = reference.split_once('/') else {
            return ToolOutcome::failed(
                "read",
                ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: rejected\ntool: read\nerror: invalid_artifact_reference\n\n{reference}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        };
        return read_artifact(ctx, session_id, id, args.start_line, args.line_count);
    }
    let path = resolve_workspace_path(&ctx.workspace_root, &args.path);
    let line_count = args.line_count.clamp(1, DEFAULT_READ_LINES);
    if let Ok(snapshot) = edit::snapshot_file(&path) {
        ctx.snapshot_store.lock().unwrap().record(snapshot);
    }
    match edit::read_window(&path, args.start_line, line_count) {
        Ok(window) => {
            let mut text = window.text;
            let mut truncated = window.truncated;
            if text.len() > DEFAULT_READ_MAX_BYTES {
                text.truncate(DEFAULT_READ_MAX_BYTES);
                truncated = true;
            }
            let revision_header = edit::format_revision_header(&window.revision);
            let output = format!(
                "{revision_header}\npath: {}\nlines: {}-{} of {}{}\n\n{}",
                display_path(&ctx.workspace_root, &path),
                window.start_line,
                window.start_line + window.returned_lines - 1,
                window.total_lines,
                if truncated { " (truncated)" } else { "" },
                text,
            );
            ToolOutcome::succeeded("read", output).with_metadata(ToolMetadata {
                tool: "read".into(),
                target: Some(display_path(&ctx.workspace_root, &path)),
                ..Default::default()
            })
        }
        Err(error) => failed_outcome("read", error),
    }
}

/// 有界读取 artifact（§8.4：模型使用 opaque 引用，不接触本机临时目录绝对路径）。
fn read_artifact(
    ctx: &ToolContext,
    session_id: &str,
    id: &str,
    start_line: usize,
    line_count: usize,
) -> ToolOutcome {
    const MAX_ARTIFACT_READ_BYTES: usize = 48 * 1024;
    let Some(record) = crate::session::artifact::find(&ctx.artifacts_root, session_id, id) else {
        return ToolOutcome::failed(
            "read",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!(
                    "status: failed\ntool: read\nerror: artifact_not_found\n\n@artifact/{session_id}/{id}"
                ),
                effect: None,
                artifact: None,
            },
        );
    };
    let bytes = match crate::session::artifact::read_bounded(&record, MAX_ARTIFACT_READ_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => {
            return ToolOutcome::failed(
                "read",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("status: failed\ntool: read\nerror: io\n\n{error}"),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let start = start_line.saturating_sub(1).min(total_lines);
    let end = (start + line_count.min(DEFAULT_READ_LINES)).min(total_lines);
    let shown = end.saturating_sub(start);
    let truncated = end < total_lines || bytes.len() >= MAX_ARTIFACT_READ_BYTES;
    let body = lines[start..end].join("\n");
    let output = format!(
        "path: @artifact/{session_id}/{id}\nbytes: {}\nlines: {}-{} of {}{}\n\n{}",
        record.byte_length,
        start + 1,
        start + shown,
        total_lines,
        if truncated { " (truncated)" } else { "" },
        body,
    );
    ToolOutcome::succeeded("read", output).with_metadata(ToolMetadata {
        tool: "read".into(),
        target: Some(format!("@artifact/{session_id}/{id}")),
        ..Default::default()
    })
}

pub fn edit(
    args: crate::tool::edit::EditArgs,
    ctx: &ToolContext,
    plan: Option<&crate::tool::edit::CommitPlan>,
) -> ToolOutcome {
    let path = resolve_workspace_path(&ctx.workspace_root, &args.path);
    // §10.7 第 1 条：提交计划（temp/backup 路径）必须在副作用前由调用方生成。
    let Some(plan) = plan else {
        return ToolOutcome::failed(
            "edit",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: rejected\ntool: edit\nerror: missing_commit_plan".to_string(),
                effect: None,
                artifact: None,
            },
        );
    };
    match crate::tool::edit::apply_edit(&path, &args.revision, &args.replacements).and_then(
        |result| {
            crate::tool::edit::commit_edit(&result, &path, plan)?;
            // §10.1：记录提交后的 snapshot（后续 stale 诊断用）。
            if let Ok(snapshot) =
                crate::tool::edit::build_snapshot(path.clone(), result.new_raw.clone())
            {
                ctx.snapshot_store.lock().unwrap().record(snapshot);
            }
            Ok(result)
        },
    ) {
        Ok(result) => {
            // §10.3 第 10 条：返回 unified diff 与修改统计。
            let diff = crate::tool::edit::unified_diff(&result);
            let diff_summary = if diff.is_empty() {
                String::new()
            } else {
                format!("\ndiff:\n{diff}")
            };
            let output = format!(
                "status: succeeded\ntool: edit\npath: {}\napplied: {}\nprevious_revision: {}\ncurrent_revision: {}{diff_summary}",
                display_path(&ctx.workspace_root, &path),
                result.applied,
                result.previous_revision,
                result.current_revision,
            );
            let mut outcome = ToolOutcome::succeeded("edit", output);
            outcome
                .observed_resources
                .push(crate::tool::outcome::ResourceVersion {
                    path: display_path(&ctx.workspace_root, &path),
                    revision: result.current_revision,
                });
            outcome.session_metadata = ToolMetadata {
                tool: "edit".into(),
                target: Some(display_path(&ctx.workspace_root, &path)),
                ..Default::default()
            };
            outcome
        }
        Err(error) => failed_outcome("edit", error),
    }
}

pub fn write(
    args: WriteArgs,
    ctx: &ToolContext,
    plan: Option<&crate::tool::edit::CommitPlan>,
) -> ToolOutcome {
    let path = resolve_workspace_path(&ctx.workspace_root, &args.path);
    let Some(plan) = plan else {
        return ToolOutcome::failed(
            "write",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: rejected\ntool: write\nerror: missing_commit_plan".to_string(),
                effect: None,
                artifact: None,
            },
        );
    };
    match edit::write_new_file(&path, args.content.as_bytes(), plan) {
        Ok(revision) => {
            let output = format!(
                "status: succeeded\ntool: write\npath: {}\nrevision: {}",
                display_path(&ctx.workspace_root, &path),
                revision,
            );
            let mut outcome = ToolOutcome::succeeded("write", output);
            outcome.session_metadata = ToolMetadata {
                tool: "write".into(),
                target: Some(display_path(&ctx.workspace_root, &path)),
                ..Default::default()
            };
            outcome
        }
        Err(error) => failed_outcome("write", error),
    }
}

fn failed_outcome(tool: &str, error: EditError) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        ModelPayload {
            status: ToolStatus::Failed,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!(
                "status: failed\ntool: {tool}\nerror: {}\n\n{error}",
                error.code()
            ),
            effect: None,
            artifact: None,
        },
    )
}

/// workspace-relative 展示路径（§9.1：普通结果显示相对路径，不显示绝对路径）。
pub fn display_path(workspace_root: &Utf8PathBuf, path: &Utf8PathBuf) -> String {
    path.strip_prefix(workspace_root)
        .map(|relative| relative.to_string())
        .unwrap_or_else(|_| path.to_string())
}
