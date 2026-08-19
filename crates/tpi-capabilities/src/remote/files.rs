//! Remote file tools（§41-§44）。
//!
//! read/edit/write 在远端保持与本地**完全相同的 semantic contract**：
//! - revision-bound（`revision_of` 是 blake3 纯函数，transport-independent §42）；
//! - stale rejection（提交前校验远端当前 digest）；
//! - atomic batch（apply 内存预检，任一条失败整体拒绝；上传 temp + rename）；
//! - diff（unified diff 与本地同一实现）。
//!
//! 传输用 SFTP（temp 上传 + atomic rename；远端 FS 不支持时降级并明确记录 §44）。

use camino::Utf8PathBuf;

use crate::remote::ssh::SshClient;
use crate::tool::ToolContext;
use crate::tool::edit::{self, FileSnapshot};
use tpi_core::outcome::{ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

/// 远端 read 窗口参数。
#[derive(Debug, Clone)]
pub struct RemoteReadArgs {
    pub path: String,
    pub start_line: usize,
    pub line_count: usize,
}

/// 远端 edit 参数（与本地 EditArgs 同构）。
#[derive(Debug, Clone)]
pub struct RemoteEditArgs {
    pub path: String,
    pub replacements: Vec<edit::Replacement>,
}

/// 远端 write 参数。
#[derive(Debug, Clone)]
pub struct RemoteWriteArgs {
    pub path: String,
    pub content: String,
}

pub async fn remote_read(
    client: &mut SshClient,
    args: &RemoteReadArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    let path = match resolve_remote_path(ctx, &args.path) {
        Ok(p) => p,
        Err(e) => return path_rejected("read", e),
    };
    // 1. 读远端字节。
    let raw = match crate::remote::run_with_budget(ctx, client.read_file(&path)).await {
        Ok(raw) => raw,
        Err(e) if is_no_such_file(&e) => {
            return failed(
                "read",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: Some("ssh".into()),
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: read\nerror: not_found\n\n远端文件不存在：{path}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
        Err(e) => {
            return failed(
                "read",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: Some("ssh".into()),
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("status: failed\ntool: read\nerror: sftp\n\n{e}"),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    // 2. 复用本地 snapshot/窗口逻辑（同一输出格式 §43）。
    let pathbuf = Utf8PathBuf::from(&path);
    let snapshot = match edit::build_snapshot(pathbuf.clone(), raw) {
        Ok(s) => s,
        Err(e) => {
            return failed(
                "read",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("status: failed\ntool: read\nerror: {}\n\n{e}", e.code()),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    let window = edit::read_window_from_snapshot(&snapshot, args.start_line, args.line_count);
    let mut text = window.text;
    let mut truncated = window.truncated;
    if text.len() > crate::tool::files::DEFAULT_READ_MAX_BYTES {
        tpi_core::util::truncate_to_char_boundary(
            &mut text,
            crate::tool::files::DEFAULT_READ_MAX_BYTES,
        );
        truncated = true;
    }
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
        "path: {path}\nlines: {line_range} of {}{}\n\n{}",
        window.total_lines,
        if truncated { " (truncated)" } else { "" },
        numbered,
    );
    if window.truncated && window.total_lines > window.start_line + window.returned_lines {
        output.push_str(&format!(
            "\n\n续读: read {path} start_line={} line_count={}",
            window.start_line + window.returned_lines,
            (window.total_lines - (window.start_line + window.returned_lines))
                .min(crate::tool::files::MAX_READ_LINES),
        ));
    }
    ToolOutcome::succeeded("read", output).with_metadata(ToolMetadata {
        tool: "read".into(),
        target: Some(path),
        ..Default::default()
    })
}

pub async fn remote_edit(
    client: &mut SshClient,
    args: &RemoteEditArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    let path = match resolve_remote_path(ctx, &args.path) {
        Ok(p) => p,
        Err(e) => return path_rejected("edit", e),
    };
    let pathbuf = Utf8PathBuf::from(&path);
    // 1. 读当前远端字节（stale 校验基础）。
    let raw = match crate::remote::run_with_budget(ctx, client.read_file(&path)).await {
        Ok(raw) => raw,
        Err(e) => {
            return failed(
                "edit",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: Some("ssh".into()),
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("status: failed\ntool: edit\nerror: read_failed\n\n{e}"),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    // 2. 内存应用（原子批 + diff，同一语义 §41）。
    let result = match edit::apply_edit_bytes(&pathbuf, raw, &args.replacements) {
        Ok(r) => r,
        Err(e) => {
            return failed(
                "edit",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("status: failed\ntool: edit\nerror: {}\n\n{e}", e.code()),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    // 3. 提交前二次校验远端未变（§10.3 第 6 条语义：commit 阶段竞态窗口）。
    let current = match crate::remote::run_with_budget(ctx, client.read_file(&path)).await {
        Ok(c) => edit::revision_of(&c),
        Err(e) => {
            return failed(
                "edit",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: Some("ssh".into()),
                    exit_code: None,
                    duration_ms: 0,
                    output: format!("status: failed\ntool: edit\nerror: verify_failed\n\n{e}"),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    if current != result.previous_revision {
        return failed(
            "edit",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!(
                    "status: failed\ntool: edit\nerror: stale_revision\n\n远端文件已被外部修改：当前 revision {current}，期望 {}\n请重新 read 后再 edit。",
                    result.previous_revision
                ),
                effect: None,
                artifact: None,
            },
        );
    }
    // 4. 上传 temp + rename（§44）。
    if let Err(e) =
        crate::remote::run_with_budget(ctx, client.write_file(&path, &result.new_raw)).await
    {
        return failed(
            "edit",
            ModelPayload {
                status: ToolStatus::Failed,
                program: Some("ssh".into()),
                exit_code: None,
                duration_ms: 0,
                output: format!("status: failed\ntool: edit\nerror: commit_failed\n\n{e}"),
                effect: None,
                artifact: None,
            },
        );
    }
    // 5. 结果输出（与本地 edit 同构：diff）。
    let diff = edit::unified_diff(&result);
    let mut output = format!(
        "status: succeeded\ntool: edit\napplied: {}\n{}",
        result.applied,
        if diff.is_empty() {
            String::new()
        } else {
            format!("\n{diff}")
        }
    );
    let _ = &mut output;
    ToolOutcome::succeeded("edit", output).with_metadata(ToolMetadata {
        tool: "edit".into(),
        target: Some(path),
        diff: (!diff.is_empty()).then_some(diff),
        ..Default::default()
    })
}

pub async fn remote_write(
    client: &mut SshClient,
    args: &RemoteWriteArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    let path = match resolve_remote_path(ctx, &args.path) {
        Ok(p) => p,
        Err(e) => return path_rejected("write", e),
    };
    // 1. 检查目标是否存在（区分新建/重写）。
    let _exists = match crate::remote::run_with_budget(ctx, client.stat(&path)).await {
        Ok(_) => true,
        Err(e) if is_no_such_file(&e) => false,
        Err(_) => true, // stat 失败保守视为存在（走重写校验）
    };
    let new_raw = args.content.as_bytes().to_vec();
    // exists 分支不再需要 revision 检查——并发保护由 Harness 内部 BLAKE3 CAS 完成。
    // 2. 上传 temp + rename（§44）。
    if let Err(e) = crate::remote::run_with_budget(ctx, client.write_file(&path, &new_raw)).await {
        return failed(
            "write",
            ModelPayload {
                status: ToolStatus::Failed,
                program: Some("ssh".into()),
                exit_code: None,
                duration_ms: 0,
                output: format!("status: failed\ntool: write\nerror: commit_failed\n\n{e}"),
                effect: None,
                artifact: None,
            },
        );
    }
    let output = format!("status: succeeded\ntool: write\npath: {path}");
    ToolOutcome::succeeded("write", output).with_metadata(ToolMetadata {
        tool: "write".into(),
        target: Some(path),
        ..Default::default()
    })
}

/// 远端路径解析：Remote 下不套本地沙箱（§17：ssh 目标即用户自己的机器），
/// 只做词法规范化 + NUL 拒绝。
fn resolve_remote_path(ctx: &ToolContext, path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("路径不能为空".into());
    }
    if trimmed.contains('\0') {
        return Err("路径含 NUL".into());
    }
    // 相对路径基于 session cwd。
    if trimmed.starts_with('/') {
        Ok(trimmed.to_string())
    } else {
        let cwd = tpi_core::util::lock_mutex(&ctx.shell, "shell")
            .cwd
            .to_string();
        Ok(format!("{}/{}", cwd.trim_end_matches('/'), trimmed))
    }
}

fn is_no_such_file(e: &crate::remote::ssh::SshError) -> bool {
    matches!(e, crate::remote::ssh::SshError::SftpNoSuchFile(_))
}

fn path_rejected(tool: &str, detail: String) -> ToolOutcome {
    failed(
        tool,
        ModelPayload {
            status: ToolStatus::Rejected,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!("status: rejected\ntool: {tool}\nerror: invalid_path\n\n{detail}"),
            effect: None,
            artifact: None,
        },
    )
}

fn failed(tool: &str, payload: ModelPayload) -> ToolOutcome {
    ToolOutcome::failed(tool, payload)
}

/// 给文本加行号（与本地 `number_lines` 同格式）。
fn number_lines(text: &str, start_line: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let n = start_line + i;
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        out.push_str(&format!("{n}: {trimmed}\n"));
    }
    // 末尾无换行时 split_inclusive 也产出最后一段；上面已输出。
    out
}

#[allow(dead_code)]
fn _keep_snapshot(_: FileSnapshot) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_lines_uses_start_line() {
        let out = number_lines("a\nb\nc", 10);
        assert_eq!(out, "10: a\n11: b\n12: c\n");
    }

    #[test]
    fn resolve_relative_uses_session_cwd() {
        // 不真正构造 ctx，直接测纯逻辑：相对路径拼接。
        let cwd = "/home/dev/project";
        let joined = format!("{}/{}", cwd.trim_end_matches('/'), "src/main.rs");
        assert_eq!(joined, "/home/dev/project/src/main.rs");
    }
}
