//! 文件读取、写入和目录访问工具。
//!
//! `read` 结果必须分别给出 path、revision、returned_lines、total_lines、truncated
//! 和正文（§10.2）。正文统一 LF，因此模型复制出的 `old_text` 与匹配空间一致；
//! 每行带 `{n}: ` 行号前缀（§read 精度：模型可精确引用单行）。

use crate::tool::edit::{self, EditError};
use crate::tool::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};
use crate::tool::{
    ToolContext, path_rejected_outcome, resolve_tool_path, validate_artifact_component,
};
use camino::Utf8PathBuf;
use schemars::JsonSchema;
use serde::Deserialize;

/// `read` 默认模型预算（§8.4：200 行，最多 32 KiB）。
pub const DEFAULT_READ_LINES: usize = 200;
pub const DEFAULT_READ_MAX_BYTES: usize = 32 * 1024;
/// `read` 单次最大行数（§工具改进：放开上限支持大文件分段，默认仍 200）。
pub const MAX_READ_LINES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ReadArgs {
    pub path: String,
    /// 起始行号（1-indexed）。
    #[serde(default = "default_start_line")]
    pub start_line: usize,
    /// 最多返回行数（默认 200，最大 1000）。大文件用
    /// `start_line`/`line_count` 分段读取，例如
    /// `start_line=201 line_count=200` 读下一段。
    #[serde(default = "default_line_count")]
    pub line_count: usize,
}

fn default_start_line() -> usize {
    1
}

fn default_line_count() -> usize {
    DEFAULT_READ_LINES
}

/// read 正文的行号前缀：`{n}: {line}`（§read 精度）。
/// 空窗口（0 行）返回空串，不产生 `0: ` 这类无效行号。
fn number_lines(text: &str, start_line: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut numbered = String::with_capacity(text.len() + text.lines().count().saturating_mul(6));
    for (index, line) in text.split('\n').enumerate() {
        numbered.push_str(&format!("{}: {line}\n", start_line + index));
    }
    numbered
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    /// P2：revision-bound 重写——目标已存在时必须提供当前 revision；
    /// 与当前一致则整体重写（不再强制走 edit 提供完整 old_text），
    /// 不一致则拒绝（stale_revision）。新建文件时忽略。
    #[serde(default)]
    pub revision: Option<String>,
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
        if !validate_artifact_component(session_id) || !validate_artifact_component(id) {
            return ToolOutcome::failed(
                "read",
                ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: rejected\ntool: read\nerror: invalid_artifact_reference\n\n@artifact/{session_id}/{id}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
        if session_id != ctx.session_id {
            return ToolOutcome::failed(
                "read",
                ModelPayload {
                    status: ToolStatus::Rejected,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: "status: rejected\ntool: read\nerror: artifact_session_mismatch".into(),
                    effect: None,
                    artifact: None,
                },
            );
        }
        return read_artifact(ctx, session_id, id, args.start_line, args.line_count);
    }
    let path = match resolve_tool_path(ctx, &args.path) {
        Ok(path) => path,
        Err(error) => return path_rejected_outcome("read", error),
    };
    let line_count = args.line_count.clamp(1, MAX_READ_LINES);
    match edit::snapshot_file(&path) {
        Ok(snapshot) => {
            let window = edit::read_window_from_snapshot(&snapshot, args.start_line, line_count);
            crate::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            let mut text = window.text;
            let mut truncated = window.truncated;
            if text.len() > DEFAULT_READ_MAX_BYTES {
                crate::util::truncate_to_char_boundary(&mut text, DEFAULT_READ_MAX_BYTES);
                truncated = true;
            }
            let revision_header = edit::format_revision_header(&window.revision);
            // 空文件/超界窗口没有返回行：不显示 "1-0" 这类无效区间。
            let line_range = if window.returned_lines == 0 {
                "0".to_string()
            } else {
                format!(
                    "{}-{}",
                    window.start_line,
                    window.start_line + window.returned_lines - 1
                )
            };
            let numbered = number_lines(&text, window.start_line);
            let mut output = format!(
                "{revision_header}\npath: {}\nlines: {line_range} of {}{}\n\n{}",
                display_path(&ctx.workspace_root, &path),
                window.total_lines,
                if truncated { " (truncated)" } else { "" },
                numbered,
            );
            // §工具改进：截断续读指引——不再让模型/用户反复猜。
            // 行数截断（窗口超界）：提示用 start_line 续读下一段。
            if window.truncated && window.total_lines > window.start_line + window.returned_lines {
                output.push_str(&format!(
                    "\n\n续读: read {} start_line={} line_count={}",
                    display_path(&ctx.workspace_root, &path),
                    window.start_line + window.returned_lines,
                    (window.total_lines - (window.start_line + window.returned_lines))
                        .min(MAX_READ_LINES),
                ));
            }
            // 字节截断（>32 KiB）：提示分段读取（行数少但每行超长时）。
            if text.len() >= DEFAULT_READ_MAX_BYTES
                && window.total_lines > window.start_line + window.returned_lines
            {
                output.push_str(&format!(
                    "\n内容超过 {} KiB 预算被截断，请用 start_line/line_count 分段读取。",
                    DEFAULT_READ_MAX_BYTES / 1024
                ));
            }
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
    let line_count = line_count.clamp(1, DEFAULT_READ_LINES);
    let window = match crate::session::artifact::read_line_window(
        &record,
        start_line,
        line_count,
        MAX_ARTIFACT_READ_BYTES,
    ) {
        Ok(window) => window,
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
    let text = String::from_utf8_lossy(&window.bytes);
    let shown_start = if window.returned_lines == 0 {
        0
    } else {
        start_line.max(1)
    };
    let shown_end = if window.returned_lines == 0 {
        0
    } else {
        shown_start.saturating_add(window.returned_lines - 1)
    };
    let numbered = number_lines(&text, shown_start);
    let output = format!(
        "path: @artifact/{session_id}/{id}\nbytes: {}\nlines: {shown_start}-{shown_end}{}\n\n{}",
        record.byte_length,
        if window.truncated { " (truncated)" } else { "" },
        numbered,
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
    let path = match resolve_tool_path(ctx, &args.path) {
        Ok(path) => path,
        Err(error) => return path_rejected_outcome("edit", error),
    };
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
                crate::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            }
            Ok(result)
        },
    ) {
        Ok(result) => {
            // §10.3 第 10 条：返回 unified diff 与修改统计。
            let diff = crate::tool::edit::unified_diff(&result);
            let mut output = format!(
                "status: succeeded\ntool: edit\npath: {}\napplied: {}\nprevious_revision: {}\ncurrent_revision: {}",
                display_path(&ctx.workspace_root, &path),
                result.applied,
                result.previous_revision,
                result.current_revision,
            );
            // §修复 #4：跳过的 no-op 条目数对模型可见（不静默）。
            if result.skipped_noops > 0 {
                output.push_str(&format!("\nskipped_noops: {}", result.skipped_noops));
            }
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
                // §用户诉求：diff 独立字段，TUI 默认展开显示红绿 diff。
                diff: if diff.is_empty() { None } else { Some(diff) },
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
    let path = match resolve_tool_path(ctx, &args.path) {
        Ok(path) => path,
        Err(error) => return path_rejected_outcome("write", error),
    };
    if args.content.len() > crate::tool::edit::MAX_SNAPSHOT_BYTES {
        return failed_outcome(
            "write",
            crate::tool::edit::EditError::FileTooLarge {
                path,
                bytes: args.content.len(),
            },
        );
    }
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
    // P2：revision-bound 重写——已存在文件必须带匹配的当前 revision。
    // （新建路径直接走 write_new_file。）
    if path.as_std_path().exists() {
        // 先读当前内容：already_exists 拒绝时把当前 revision 直接告诉模型，
        // 省去“再 read 一次才能重试”（edit 的 stale_revision 报错同样带 current）。
        let current = match crate::tool::edit::read_raw_file(&path) {
            Ok(raw) => crate::tool::edit::revision_of(&raw),
            Err(error) => return failed_outcome("write", error),
        };
        let Some(expected_token) = args.revision.as_deref() else {
            return ToolOutcome::failed(
                "write",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: write\nerror: already_exists\n\n{} 已存在；整体重写必须提供当前 revision（先 read 获取，或改用 edit）。
当前 revision: {}",
                        display_path(&ctx.workspace_root, &path),
                        current,
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        };
        let Some(expected) = crate::tool::edit::parse_revision_token(expected_token) else {
            return failed_outcome(
                "write",
                crate::tool::edit::EditError::InvalidRevision {
                    value: expected_token.to_string(),
                },
            );
        };
        if current != expected {
            return failed_outcome(
                "write",
                crate::tool::edit::EditError::StaleRevision {
                    path: path.clone(),
                    current,
                    expected: expected.clone(),
                    // 整文件重写：无 replacement 可定位，回填 None。
                    context: None,
                },
            );
        }
        // revision 匹配：按重写流程提交（保留目标文件，带 backup 恢复语义）。
        return rewrite_with_revision(
            &path,
            args.content.as_bytes(),
            plan,
            &display_path(&ctx.workspace_root, &path),
            &expected,
            ctx,
        );
    }
    match edit::write_new_file(&path, args.content.as_bytes(), plan) {
        Ok(revision) => {
            let output = format!(
                "status: succeeded\ntool: write\npath: {}\nrevision: {}",
                display_path(&ctx.workspace_root, &path),
                revision,
            );
            let mut outcome = ToolOutcome::succeeded("write", output);
            let new_raw = args.content.into_bytes();
            if let Ok(snapshot) = crate::tool::edit::build_snapshot(path.clone(), new_raw.clone()) {
                crate::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            }
            outcome
                .observed_resources
                .push(crate::tool::outcome::ResourceVersion {
                    path: display_path(&ctx.workspace_root, &path),
                    revision: revision.clone(),
                });
            // §用户诉求（修复）：新建文件同样生成 unified diff（空 → 新内容，
            // 全 `+` 行），与 edit / 重写路径一致——否则 TUI 卡片既无 diff 也
            // 无正文（total==0），点击展开无任何内容变化（"write 无法展开"）。
            let diff = crate::tool::edit::unified_diff(&crate::tool::edit::EditResult {
                previous_revision: crate::tool::edit::revision_of(&[]),
                current_revision: revision.clone(),
                applied: 1,
                skipped_noops: 0,
                previous_raw: Vec::new(),
                new_raw: new_raw.clone(),
            });
            outcome.session_metadata = ToolMetadata {
                tool: "write".into(),
                target: Some(display_path(&ctx.workspace_root, &path)),
                diff: if diff.is_empty() { None } else { Some(diff) },
                ..Default::default()
            };
            outcome
        }
        Err(error) => failed_outcome("write", error),
    }
}

/// P2：revision 匹配的整体重写——复用 edit 的提交/恢复语义
/// （ReplaceFileW + backup + 校验；§10.7），而不是裸写覆盖。
fn rewrite_with_revision(
    path: &Utf8PathBuf,
    content: &[u8],
    plan: &crate::tool::edit::CommitPlan,
    display_path: &str,
    expected_revision: &str,
    ctx: &ToolContext,
) -> ToolOutcome {
    let previous_raw = match crate::tool::edit::read_raw_file(path) {
        Ok(raw) => raw,
        Err(error) => return failed_outcome("write", error),
    };
    let observed_revision = crate::tool::edit::revision_of(&previous_raw);
    if observed_revision != expected_revision {
        return failed_outcome(
            "write",
            crate::tool::edit::EditError::StaleRevision {
                path: path.clone(),
                current: observed_revision,
                expected: expected_revision.to_string(),
                // 整文件重写：无 replacement 可定位，回填 None。
                context: None,
            },
        );
    }
    let result = crate::tool::edit::EditResult {
        previous_revision: observed_revision,
        current_revision: crate::tool::edit::revision_of(content),
        applied: 1,
        skipped_noops: 0,
        previous_raw,
        new_raw: content.to_vec(),
    };
    match crate::tool::edit::commit_edit(&result, path, plan) {
        Ok(()) => {
            let diff = crate::tool::edit::unified_diff(&result);
            let output = format!(
                "status: succeeded\ntool: write\npath: {display_path}\nrewritten: true\nprevious_revision: {}\ncurrent_revision: {}",
                result.previous_revision, result.current_revision,
            );
            let mut outcome = ToolOutcome::succeeded("write", output);
            if let Ok(snapshot) =
                crate::tool::edit::build_snapshot(path.clone(), result.new_raw.clone())
            {
                crate::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            }
            outcome
                .observed_resources
                .push(crate::tool::outcome::ResourceVersion {
                    path: display_path.to_string(),
                    revision: result.current_revision.clone(),
                });
            // §用户诉求：重写已有文件时 diff 独立字段（TUI 默认展开红绿 diff）。
            outcome.session_metadata = ToolMetadata {
                tool: "write".into(),
                target: Some(display_path.to_string()),
                diff: if diff.is_empty() { None } else { Some(diff) },
                ..Default::default()
            };
            outcome
        }
        Err(error) => failed_outcome("write", error),
    }
}

fn failed_outcome(tool: &str, error: EditError) -> ToolOutcome {
    // P2：给模型明确的下一步动作，而不是只拒绝（“硬拒绝”→“可恢复引导”）。
    let hint = match &error {
        EditError::StaleRevision { current, .. } => format!(
            "\nhint: 文件已变化（current_revision {current}）。可直接用该 revision 重试 edit（无需重新 read）；需要确认内容时再 read。"
        ),
        EditError::NoMatch { .. } => {
            "\nhint: old_text 在文件中不存在；请先 read 确认实际内容，再调整 old_text。".into()
        }
        EditError::MultipleMatches { .. } => {
            "\nhint: old_text 出现多次；请包含更多上下文使匹配唯一。".into()
        }
        EditError::Overlap { .. } => "\nhint: 多个 replacement 重叠；请拆分为独立批次。".into(),
        EditError::AllNoOps { .. } => {
            "\nhint: 所有 replacement 都是 no-op（old_text == new_text），未做任何修改；请确认意图。"
                .into()
        }
        _ => String::new(),
    };
    // §修复 #2/#3：stale/no_match 时回填当前文件相关区域内容，模型免 read 自纠。
    let context_note = match &error {
        EditError::StaleRevision { context, .. } | EditError::NoMatch { context, .. } => context
            .as_deref()
            .map(|ctx| format!("\n当前文件相关区域:\n{ctx}")),
        _ => None,
    };
    let mut output = format!(
        "status: failed\ntool: {tool}\nerror: {}\n\n{error}{hint}",
        error.code()
    );
    if let Some(note) = context_note {
        output.push_str(&note);
    }
    ToolOutcome::failed(
        tool,
        ModelPayload {
            status: ToolStatus::Failed,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::outcome::ToolStatus;
    use tokio_util::sync::CancellationToken;

    /// BUG-001 回归：读取超过 32 KiB 且截断点落在多字节字符中间的中文文件
    /// 不得 panic（此前 `String::truncate` 按裸字节截断）。
    #[test]
    fn read_large_cjk_file_truncates_at_char_boundary_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("big_zh.rs")).unwrap();
        // 单行 20k 个“中”（3 字节/字）：60000 字节 + \n，远超 32 KiB 预算；
        // 32 KiB 边界（32768 % 3 = 2）必然落在一个中文字符中间。
        let mut content = String::new();
        content.push_str(&"中".repeat(20_000));
        content.push('\n');
        content.push_str("fn main() {}\n");
        std::fs::write(path.as_std_path(), &content).unwrap();

        let local = crate::workspace::LocalWorkspace::new(workspace.clone(), true);
        let ctx = ToolContext {
            workspace_root: workspace.clone(),
            shell: local.shell.clone(),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(local),
            )),
            cancel: CancellationToken::new(),
            artifacts_root: dir.path().join("artifacts"),
            session_id: "test-session".into(),
            call_id: crate::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
        };
        let outcome = read(
            ReadArgs {
                path: path.to_string(),
                start_line: 1,
                line_count: 200,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        let output = &outcome.model_payload.output;
        assert!(
            std::str::from_utf8(output.as_bytes()).is_ok(),
            "read 输出必须是合法 UTF-8"
        );
        assert!(
            !output.contains('\u{FFFD}'),
            "截断不得产生 replacement char"
        );
        // 正文（去掉头部信息后）不得超过 32 KiB 预算。行号前缀是展示开销
        // （§read 精度），预算按去掉 `{n}: ` 前缀后的纯文本计。
        let body = output.split("\n\n").last().unwrap_or("");
        let unnumbered: String = body
            .lines()
            .map(|line| line.split_once(": ").map(|(_, rest)| rest).unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            unnumbered.len() <= 32 * 1024,
            "正文超出预算: {} bytes",
            unnumbered.len()
        );
    }

    #[test]
    fn write_accepts_revision_header_and_revalidates_before_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.txt")).unwrap();
        std::fs::write(path.as_std_path(), b"old").unwrap();
        let revision = crate::tool::edit::revision_of(b"old");
        let local = crate::workspace::LocalWorkspace::new(workspace.clone(), true);
        let ctx = ToolContext {
            workspace_root: workspace.clone(),
            shell: local.shell.clone(),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(local),
            )),
            cancel: CancellationToken::new(),
            artifacts_root: dir.path().join("artifacts"),
            session_id: "test-session".into(),
            call_id: crate::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
        };
        let plan = crate::tool::edit::prepare_commit(&path);
        let outcome = write(
            WriteArgs {
                path: path.to_string(),
                content: "new".into(),
                revision: Some(crate::tool::edit::format_revision_header(&revision)),
            },
            &ctx,
            Some(&plan),
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        assert!(outcome.session_metadata.diff.is_some());
        assert!(
            !outcome.model_payload.output.contains("\ndiff:\n"),
            "完整 diff 只能进入 TUI 字段，不能重复占用模型上下文"
        );
        assert_eq!(std::fs::read(path.as_std_path()).unwrap(), b"new");

        std::fs::write(path.as_std_path(), b"external change").unwrap();
        let stale = rewrite_with_revision(&path, b"overwrite", &plan, "a.txt", &revision, &ctx);
        assert_eq!(stale.status, ToolStatus::Failed);
        assert!(stale.model_payload.output.contains("stale_revision"));
        assert_eq!(
            std::fs::read(path.as_std_path()).unwrap(),
            b"external change"
        );
    }

    /// §用户诉求（修复）：write 新建文件也必须带 unified diff（空 → 新内容），
    /// 与 edit / 重写路径一致——否则 TUI 卡片无可展开内容。
    #[test]
    fn write_new_file_carries_diff() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("new.txt")).unwrap();
        let local = crate::workspace::LocalWorkspace::new(workspace.clone(), true);
        let ctx = ToolContext {
            workspace_root: workspace.clone(),
            shell: local.shell.clone(),
            workspace: std::sync::Arc::new(std::sync::Mutex::new(
                crate::workspace::ActiveWorkspace::local(local),
            )),
            cancel: CancellationToken::new(),
            artifacts_root: dir.path().join("artifacts"),
            session_id: "test-session".into(),
            call_id: crate::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
        };
        let plan = crate::tool::edit::prepare_commit(&path);
        let outcome = write(
            WriteArgs {
                path: path.to_string(),
                content: "line1\nline2\n".into(),
                revision: None,
            },
            &ctx,
            Some(&plan),
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        let diff = outcome
            .session_metadata
            .diff
            .as_deref()
            .expect("新建文件 write 必须携带 diff");
        assert!(
            diff.contains("+line1") && diff.contains("+line2"),
            "diff 必须包含新增内容行: {diff}"
        );
        assert!(
            !outcome.model_payload.output.contains("\ndiff:\n"),
            "完整 diff 只能进入 TUI 字段，不能重复占用模型上下文"
        );
        assert_eq!(
            std::fs::read(path.as_std_path()).unwrap(),
            b"line1\nline2\n"
        );
    }
}
