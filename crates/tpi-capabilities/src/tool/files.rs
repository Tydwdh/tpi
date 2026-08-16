//! 文件读取、写入和目录访问工具。
//!
//! `read` 结果必须分别给出 path、revision、returned_lines、total_lines、truncated
//! 和正文（§10.2）。正文统一 LF，因此模型复制出的 `old_text` 与匹配空间一致；
//! 每行带 `{n}: ` 行号前缀（§read 精度：模型可精确引用单行）。

use crate::tool::edit::{self, EditError};
use crate::tool::{
    ToolContext, path_rejected_outcome, resolve_tool_path, validate_artifact_component,
};
use camino::Utf8PathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use tpi_core::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

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
    /// 目录模式：条目序号（1-indexed），配合 `depth` 浏览目录。
    #[serde(default = "default_line_count")]
    pub line_count: usize,
    /// 目录浏览深度（仅当 `path` 是目录时生效）：默认 1 = 只列直接子项；
    /// 传更大值递归列出更深层级（复用 list 的目录扫描语义，§list 并入 read）。
    #[serde(default)]
    pub depth: Option<usize>,
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
    // ISSUE-044：start_line=0 是无效请求（行号 1-indexed；0 此前被静默当作 1，
    // 模型用 0 翻页会与上次窗口重叠/死循环）。明确拒绝并引导。
    if args.start_line == 0 {
        return ToolOutcome::failed(
            "read",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output:
                    "status: rejected\ntool: read\nerror: invalid_start_line\n\nstart_line 必须是 ≥1 的整数（行号 1-indexed）；从文件开头读请省略该参数或填 1。"
                        .into(),
                effect: None,
                artifact: None,
            },
        );
    }
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
    // §list 并入 read：path 是目录 → 目录浏览（复用 list 的扫描语义，
    // 默认单层 depth=1；start_line/line_count 作为条目窗口分页）。
    if path.is_dir() {
        return read_dir(&path, &args, ctx);
    }
    let line_count = args.line_count.clamp(1, MAX_READ_LINES);
    match edit::snapshot_file(&path) {
        Ok(snapshot) => {
            // P0a：read 输出头的空白元信息——模型据此构造 old_text，闭合
            // read→edit 协议（tab 缩进文件不再“看不见差异”）。正文保持原样
            //（复制即精确），不引入不可逆的视觉字符。须在 snapshot 移动前分析。
            let ws = crate::tool::edit::analyze_whitespace(&snapshot);
            let window = edit::read_window_from_snapshot(&snapshot, args.start_line, line_count);
            tpi_core::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            let mut text = window.text;
            let mut truncated = window.truncated;
            if text.len() > DEFAULT_READ_MAX_BYTES {
                tpi_core::util::truncate_to_char_boundary(&mut text, DEFAULT_READ_MAX_BYTES);
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
            let indentation = match ws.indentation {
                crate::tool::edit::IndentationSummary::Tabs => "tabs".to_string(),
                crate::tool::edit::IndentationSummary::Spaces => "spaces".to_string(),
                crate::tool::edit::IndentationSummary::Mixed => "mixed".to_string(),
                crate::tool::edit::IndentationSummary::None_ => "none".to_string(),
            };
            let line_endings = match ws.line_endings {
                crate::tool::edit::LineEndingsSummary::Lf => "LF",
                crate::tool::edit::LineEndingsSummary::Crlf => "CRLF",
                crate::tool::edit::LineEndingsSummary::Mixed => "mixed",
            };
            let mut output = format!(
                "{revision_header}\npath: {}\nlines: {line_range} of {}{}\nindentation: {indentation} (tab_width {})\nline_endings: {line_endings}\ntrailing_whitespace: {}\n\n{}",
                display_path(&ctx.workspace_root, &path),
                window.total_lines,
                if truncated { " (truncated)" } else { "" },
                ws.tab_width_display,
                if ws.trailing_whitespace { "yes" } else { "no" },
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

/// read 目录分支（§list 并入 read；opencode 同款语义：read 对目录返回条目）。
///
/// 默认单层（depth=1），`depth` 参数递归；条目窗口复用 `start_line`/`line_count`
/// （1-indexed 条目序号），扫描统计与 stop_reason 一并返回。
fn read_dir(path: &Utf8PathBuf, args: &ReadArgs, ctx: &ToolContext) -> ToolOutcome {
    // §list 并入 read：start_line=0 是无效请求（与文件模式一致拒绝）。
    if args.start_line == 0 {
        return ToolOutcome::failed(
            "read",
            ModelPayload {
                status: ToolStatus::Rejected,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output:
                    "status: rejected\ntool: read\nerror: invalid_start_line\n\nstart_line 必须是 ≥1 的整数（条目序号 1-indexed）；从开头浏览请省略该参数或填 1。"
                        .into(),
                effect: None,
                artifact: None,
            },
        );
    }
    let depth = args.depth.unwrap_or(1).max(1);
    let scan = crate::tool::search::scan_dir(path, depth, ctx);
    let total = scan.items.len();
    let start = (args.start_line.saturating_sub(1)).min(total);
    let count = args.line_count.clamp(1, MAX_READ_LINES);
    let end = (start + count).min(total);
    let shown = end - start;
    let body = scan.items[start..end].join("\n");
    let stop = scan.stop_reason.as_str();
    let mut output = format!(
        "path: {}\ntype: directory\ndepth: {depth}\nentries: {shown} shown of {total}\nscanned_files: {}\nscanned_bytes: {}\nelapsed_ms: {}\nstop_reason: {stop}\n\n{body}",
        display_path(&ctx.workspace_root, path),
        scan.scanned_files,
        scan.scanned_bytes,
        scan.elapsed_ms,
    );
    // 条目窗口截断：提示用 start_line 续读下一段（与文件模式一致的续读指引）。
    if end < total {
        output.push_str(&format!(
            "\n\n续读: read {} start_line={} line_count={}",
            display_path(&ctx.workspace_root, path),
            end + 1,
            (total - end).min(MAX_READ_LINES),
        ));
    }
    // 结果达上限：引导收窄（与 list 的引导一致）。
    if stop == "result_limit" {
        output.push_str("\n\n结果达上限。可减小 depth 或收窄 path 后重新浏览。");
    }
    ToolOutcome::succeeded("read", output).with_metadata(ToolMetadata {
        tool: "read".into(),
        target: Some(display_path(&ctx.workspace_root, path)),
        ..Default::default()
    })
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
    let Some(record) = tpi_session::artifact::find(&ctx.artifacts_root, session_id, id) else {
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
    let window = match tpi_session::artifact::read_line_window(
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
    // V3：唯一编辑协议 = replacements（old_text → new_text）。
    // apply_edit 内部：revision 校验 → 匹配链（Exact → Trailing →
    // UniformOuterIndent → Fail）→ 唯一性/重叠/no-op 预检 → 原子应用。
    match crate::tool::edit::apply_edit(&path, &args.revision, &args.replacements).and_then(
        |result| {
            crate::tool::edit::commit_edit(&result, &path, plan)?;
            if let Ok(snapshot) =
                crate::tool::edit::build_snapshot(path.clone(), result.new_raw.clone())
            {
                tpi_core::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            }
            Ok(result)
        },
    ) {
        Ok(result) => edit_success_outcome(ctx, &path, result, "edit"),
        Err(error) => failed_outcome("edit", error),
    }
}

/// 编辑成功结果（edit / edit_range 共用）：unified diff + revision + resource。
fn edit_success_outcome(
    ctx: &ToolContext,
    path: &Utf8PathBuf,
    result: crate::tool::edit::EditResult,
    tool: &'static str,
) -> ToolOutcome {
    // §B1：记录 Mutation Journal（before/after 快照；undo 与崩溃恢复数据源）。
    let payload = tpi_session::protocol::MutationCommittedPayload {
        mutation_id: tpi_core::ids::EventId::new_v7().to_string(),
        files: vec![tpi_session::protocol::MutationFile {
            path: path.as_std_path().to_string_lossy().to_string(),
            before_revision: result.previous_revision.clone(),
            after_revision: result.current_revision.clone(),
            before_content: result.previous_raw.clone(),
            after_content: result.new_raw.clone(),
        }],
    };
    let _ = tpi_session::journal::append_mutation(&ctx.artifacts_root, &ctx.session_id, &payload);
    let diff = crate::tool::edit::unified_diff(&result);
    let mut output = format!(
        "status: succeeded\ntool: {tool}\npath: {}\napplied: {}\nprevious_revision: {}\ncurrent_revision: {}",
        display_path(&ctx.workspace_root, path),
        result.applied,
        result.previous_revision,
        result.current_revision,
    );
    if result.skipped_noops > 0 {
        output.push_str(&format!("\nskipped_noops: {}", result.skipped_noops));
    }
    let mut outcome = ToolOutcome::succeeded(tool, output);
    outcome
        .observed_resources
        .push(tpi_core::outcome::ResourceVersion {
            path: display_path(&ctx.workspace_root, path),
            revision: result.current_revision,
        });
    outcome.session_metadata = ToolMetadata {
        tool: tool.into(),
        target: Some(display_path(&ctx.workspace_root, path)),
        diff: if diff.is_empty() { None } else { Some(diff) },
        ..Default::default()
    };
    outcome
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
                tpi_core::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            }
            // §B1：新建文件记录 journal（before = 空）。
            let payload = tpi_session::protocol::MutationCommittedPayload {
                mutation_id: tpi_core::ids::EventId::new_v7().to_string(),
                files: vec![tpi_session::protocol::MutationFile {
                    path: path.as_std_path().to_string_lossy().to_string(),
                    before_revision: crate::tool::edit::revision_of(&[]),
                    after_revision: revision.clone(),
                    before_content: Vec::new(),
                    after_content: new_raw.clone(),
                }],
            };
            let _ = tpi_session::journal::append_mutation(
                &ctx.artifacts_root,
                &ctx.session_id,
                &payload,
            );
            outcome
                .observed_resources
                .push(tpi_core::outcome::ResourceVersion {
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
                tier: crate::tool::edit::MatchTier::Exact,
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
        tier: crate::tool::edit::MatchTier::Exact,
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
                tpi_core::util::lock_mutex(&ctx.snapshot_store, "snapshot_store").record(snapshot);
            }
            // §B1：重写已有文件记录 journal（before = 旧内容）。
            let payload = tpi_session::protocol::MutationCommittedPayload {
                mutation_id: tpi_core::ids::EventId::new_v7().to_string(),
                files: vec![tpi_session::protocol::MutationFile {
                    path: path.as_std_path().to_string_lossy().to_string(),
                    before_revision: result.previous_revision.clone(),
                    after_revision: result.current_revision.clone(),
                    before_content: result.previous_raw.clone(),
                    after_content: result.new_raw.clone(),
                }],
            };
            let _ = tpi_session::journal::append_mutation(
                &ctx.artifacts_root,
                &ctx.session_id,
                &payload,
            );
            outcome
                .observed_resources
                .push(tpi_core::outcome::ResourceVersion {
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
        EditError::NoMatch { diagnostic, .. } => {
            if let Some(d) = diagnostic {
                let detail = match d.kind {
                    crate::tool::edit::MismatchKind::Indentation => format!(
                        "差异类型: 缩进（候选行 {}~{}，第 {} 行 lstrip 后内容相同，但缩进不同：old 提供 {}，文件实际 {}）",
                        d.lines.0, d.lines.1, d.line.unwrap_or(0) + 1, d.provided_indent.as_deref().unwrap_or("?"), d.expected_indent.as_deref().unwrap_or("?")
                    ),
                    crate::tool::edit::MismatchKind::Textual => format!(
                        "差异类型: 文本（候选行 {}~{}，相似度 {:.2}；首个不同行：\n  old: {:?}\n  file: {:?}）",
                        d.lines.0, d.lines.1, d.similarity_bp as f64 / 100.0, d.first_difference.as_ref().map(|(a, _)| a.as_str()), d.first_difference.as_ref().map(|(_, b)| b.as_str())
                    ),
                    crate::tool::edit::MismatchKind::None => {
                        format!("差异类型: 未定位到相似候选（行 {}~{})", d.lines.0, d.lines.1)
                    }
                };
                format!("\nhint: old_text 在文件中不存在。{detail}。请按文件实际内容调整 old_text（缩进/行尾空白需与文件一致）。")
            } else {
                "\nhint: old_text 在文件中不存在；请先 read 确认实际内容，再调整 old_text。".into()
            }
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
    use tokio_util::sync::CancellationToken;
    use tpi_core::outcome::ToolStatus;

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
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
        };
        let outcome = read(
            ReadArgs {
                path: path.to_string(),
                start_line: 1,
                line_count: 200,
                depth: None,
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
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
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
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
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

    /// P0a：read 输出头含空白元信息（indentation/line_endings/trailing_whitespace）
    /// ——模型据此构造 old_text，闭合 read→edit 协议（tab 缩进不再“看不见”）。
    #[test]
    fn read_output_includes_whitespace_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("tabbed.rs")).unwrap();
        // tab 缩进 + CRLF + 行尾空白。
        let content = "fn a() {\r\n\twork(); \r\n}\r\n";
        std::fs::write(path.as_std_path(), content).unwrap();
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
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
        };
        let outcome = read(
            ReadArgs {
                path: path.to_string(),
                start_line: 1,
                line_count: 10,
                depth: None,
            },
            &ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        let output = &outcome.model_payload.output;
        assert!(
            output.contains("indentation: tabs (tab_width 4)"),
            "read 必须标注 tab 缩进: {output}"
        );
        assert!(output.contains("line_endings: CRLF"), "{output}");
        assert!(output.contains("trailing_whitespace: yes"), "{output}");
        // 正文不受影响（行号前缀保留）。
        assert!(output.contains("1: fn a() {"), "{output}");
        // 纯空格缩进 + LF + 无 trailing：另一个文件。
        let path2 = Utf8PathBuf::from_path_buf(dir.path().join("spaced.rs")).unwrap();
        std::fs::write(path2.as_std_path(), "fn b() {\n    work();\n}\n").unwrap();
        let outcome2 = read(
            ReadArgs {
                path: path2.to_string(),
                start_line: 1,
                line_count: 10,
                depth: None,
            },
            &ctx,
        );
        let out2 = &outcome2.model_payload.output;
        assert!(out2.contains("indentation: spaces"), "{out2}");
        assert!(out2.contains("line_endings: LF"), "{out2}");
        assert!(out2.contains("trailing_whitespace: no"), "{out2}");
    }

    // ---- edit_range（§anchor）----

    /// 构造带 snapshot_store 的 ToolContext（files 测试共用）。
    fn range_ctx(dir: &tempfile::TempDir) -> (ToolContext, Utf8PathBuf) {
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
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
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            output_tx: None,
            scan_snapshots: Default::default(),
            shell_path: None,
            snapshot_store: Default::default(),
            current_plan: Default::default(),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                crate::process::managed::ProcessRegistry::new(),
            )),
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                crate::tool::registry::ToolRegistry::new(),
            )),
            interactive: false,
            allow_outside_workspace: true,
        };
        (ctx, workspace)
    }

    // ---- V3：唯一编辑协议 = replacements（old_text → new_text）----
    // ---- V3：唯一编辑协议 = replacements（old_text → new_text）----

    /// 读文件拿 revision（并 record snapshot）。
    fn read_revision(path: &Utf8PathBuf, ctx: &ToolContext) -> String {
        let outcome = read(
            ReadArgs {
                path: path.to_string(),
                start_line: 1,
                line_count: 10,
                depth: None,
            },
            ctx,
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        crate::tool::edit::parse_revision_token(
            outcome
                .model_payload
                .output
                .lines()
                .find_map(|l| {
                    l.strip_prefix("[revision=")
                        .map(|r| r.strip_suffix(']').unwrap())
                })
                .unwrap(),
        )
        .unwrap()
    }

    fn do_edit(
        ctx: &ToolContext,
        path: &Utf8PathBuf,
        content: &str,
        replacements: Vec<crate::tool::edit::Replacement>,
    ) -> ToolOutcome {
        std::fs::write(path.as_std_path(), content).unwrap();
        let revision = read_revision(path, ctx);
        let plan = crate::tool::edit::prepare_commit(path);
        edit(
            crate::tool::edit::EditArgs {
                path: path.to_string(),
                revision,
                replacements,
            },
            ctx,
            Some(&plan),
        )
    }

    /// 基本替换：old_text → new_text（单行）。
    #[test]
    fn edit_replacements_basic_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _) = range_ctx(&dir);
        let path = Utf8PathBuf::from_path_buf(dir.path().join("basic.rs")).unwrap();
        let outcome = do_edit(
            &ctx,
            &path,
            "fn a() {\n    old();\n    keep();\n}\n",
            vec![crate::tool::edit::Replacement {
                old_text: "    old();".into(),
                new_text: "    new();".into(),
            }],
        );
        assert_eq!(
            outcome.status,
            ToolStatus::Succeeded,
            "{}",
            outcome.model_payload.output
        );
        let content = std::fs::read_to_string(path.as_std_path()).unwrap();
        assert_eq!(content, "fn a() {\n    new();\n    keep();\n}\n");
    }

    /// 删除：空 new_text；插入：空 old_text 不支持（old_text 必须非空定位）。
    /// 插入 = 用一段含上下文的 old_text 替换为"上下文+新内容"。
    #[test]
    fn edit_replacements_delete_and_insert() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _) = range_ctx(&dir);
        let path = Utf8PathBuf::from_path_buf(dir.path().join("di.rs")).unwrap();

        // 删除一行：old_text = 整行含换行，new_text = 空。
        let outcome = do_edit(
            &ctx,
            &path,
            "a\nb\nc\n",
            vec![crate::tool::edit::Replacement {
                old_text: "b\n".into(),
                new_text: String::new(),
            }],
        );
        assert_eq!(
            outcome.status,
            ToolStatus::Succeeded,
            "{}",
            outcome.model_payload.output
        );
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "a\nc\n"
        );

        // 插入：old_text = 锚点，new_text = 锚点 + 新行。
        let outcome = do_edit(
            &ctx,
            &path,
            "a\nc\n",
            vec![crate::tool::edit::Replacement {
                old_text: "a\n".into(),
                new_text: "a\ninserted\n".into(),
            }],
        );
        assert_eq!(
            outcome.status,
            ToolStatus::Succeeded,
            "{}",
            outcome.model_payload.output
        );
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "a\ninserted\nc\n"
        );
    }

    /// batch：多个 replacement 一次提交，全部基于同一 snapshot。
    #[test]
    fn edit_replacements_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _) = range_ctx(&dir);
        let path = Utf8PathBuf::from_path_buf(dir.path().join("batch.rs")).unwrap();
        let outcome = do_edit(
            &ctx,
            &path,
            "a\nb\nc\nd\n",
            vec![
                crate::tool::edit::Replacement {
                    old_text: "b\n".into(),
                    new_text: "B\n".into(),
                },
                crate::tool::edit::Replacement {
                    old_text: "d\n".into(),
                    new_text: "D\n".into(),
                },
            ],
        );
        assert_eq!(
            outcome.status,
            ToolStatus::Succeeded,
            "{}",
            outcome.model_payload.output
        );
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "a\nB\nc\nD\n"
        );
    }

    /// 歧义：old_text 在文件中重复 → MultipleMatches 拒绝（V3 核心安全约束）。
    #[test]
    fn edit_rejects_ambiguous_old_text() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _) = range_ctx(&dir);
        let path = Utf8PathBuf::from_path_buf(dir.path().join("dup.rs")).unwrap();
        let outcome = do_edit(
            &ctx,
            &path,
            "a\nb\na\n",
            vec![crate::tool::edit::Replacement {
                old_text: "a\n".into(),
                new_text: "A\n".into(),
            }],
        );
        assert_eq!(outcome.status, ToolStatus::Failed, "歧义必须拒绝");
        assert!(
            outcome.model_payload.output.contains("multiple_matches"),
            "应为 multiple_matches: {}",
            outcome.model_payload.output
        );
        // 文件不变（all-or-nothing）。
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "a\nb\na\n"
        );
    }

    /// 重叠：两个 replacement 的 old_text 在文件中重叠 → 整批拒绝。
    #[test]
    fn edit_replacements_batch_overlap_rejects_all() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _) = range_ctx(&dir);
        let path = Utf8PathBuf::from_path_buf(dir.path().join("ov.rs")).unwrap();
        let outcome = do_edit(
            &ctx,
            &path,
            "foo bar baz\n",
            vec![
                crate::tool::edit::Replacement {
                    old_text: "foo bar".into(),
                    new_text: "FOO".into(),
                },
                crate::tool::edit::Replacement {
                    old_text: "bar baz".into(),
                    new_text: "BAZ".into(),
                },
            ],
        );
        assert_eq!(outcome.status, ToolStatus::Failed);
        assert!(outcome.model_payload.output.contains("overlap"));
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "foo bar baz\n"
        );
    }

    /// 跨行替换：old_text 跨多行（含换行）。
    #[test]
    fn edit_replacements_multiline() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _) = range_ctx(&dir);
        let path = Utf8PathBuf::from_path_buf(dir.path().join("ml.rs")).unwrap();
        let outcome = do_edit(
            &ctx,
            &path,
            "fn f() {\n    a();\n    b();\n    c();\n}\n",
            vec![crate::tool::edit::Replacement {
                old_text: "    a();\n    b();".into(),
                new_text: "    a();\n    b();\n    d();".into(),
            }],
        );
        assert_eq!(
            outcome.status,
            ToolStatus::Succeeded,
            "{}",
            outcome.model_payload.output
        );
        let content = std::fs::read_to_string(path.as_std_path()).unwrap();
        assert_eq!(
            content,
            "fn f() {\n    a();\n    b();\n    d();\n    c();\n}\n"
        );
    }

    /// §B1 端到端：edit 成功后 journal 文件生成，undo_mutation 恢复 before。
    #[test]
    fn edit_writes_journal_and_undo_restores() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _) = range_ctx(&dir);
        let path = Utf8PathBuf::from_path_buf(dir.path().join("j.rs")).unwrap();
        let outcome = do_edit(
            &ctx,
            &path,
            "line1\nline2\nline3\n",
            vec![crate::tool::edit::Replacement {
                old_text: "line2\n".into(),
                new_text: "CHANGED\n".into(),
            }],
        );
        assert_eq!(outcome.status, ToolStatus::Succeeded);
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "line1\nCHANGED\nline3\n"
        );

        // journal 文件已生成且含 1 条 mutation。
        let journal_path = tpi_session::journal::journal_path(&ctx.artifacts_root, &ctx.session_id);
        let mutations = tpi_session::journal::load_journal(&journal_path).unwrap();
        assert_eq!(mutations.len(), 1, "journal 必须有 1 条 mutation");
        assert_eq!(
            mutations[0].files[0].before_content,
            b"line1\nline2\nline3\n"
        );

        // undo：恢复 before 内容。
        let restored =
            tpi_session::journal::undo_mutation(&mutations, &mutations[0].mutation_id, dir.path())
                .unwrap();
        assert_eq!(restored, 1);
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "line1\nline2\nline3\n"
        );
    }
}
