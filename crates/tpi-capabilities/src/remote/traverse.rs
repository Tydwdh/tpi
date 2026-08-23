//! Remote list（§45-§48）。
//!
//! - list：SFTP 递归遍历（深度限制、上限），目录带 `/`，格式与本地一致。
//!
//! 曾提供 remote search/glob；已删除（内容/文件名检索交给远端 bash + rg/find）。
//!
//! 模型看到的 ToolOutcome 必须与本地一致（§47）：items + scanned_files /
//! scanned_bytes / elapsed_ms / stop_reason；transport 不泄漏给模型（§48）。

use std::time::Instant;

use crate::remote::ssh::SshClient;
use crate::tool::ToolContext;
use tpi_core::outcome::{ModelPayload, ToolOutcome, ToolStatus};

/// 与本地一致的扫描上限（§8.4）。
pub const MAX_SCAN_FILES: u64 = 100_000;
pub const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_RESULTS: usize = 100;
const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// 远端 list 参数。
#[derive(Debug, Clone)]
pub struct RemoteListArgs {
    pub path: String,
    pub depth: usize,
}

/// remote list：SFTP 递归遍历（§45-§47）。
pub async fn remote_list(
    client: &mut SshClient,
    args: &RemoteListArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    let root = match resolve_remote_path(ctx, &args.path) {
        Ok(p) => p,
        Err(e) => return rejected("list", &e),
    };
    let started = Instant::now();
    let mut items: Vec<String> = Vec::new();
    let mut scanned_files = 0u64;
    let mut stop_reason = "complete";

    // 迭代式 DFS（SFTP read_dir 单层）。
    // stack: (相对路径, 深度)
    let mut stack: Vec<(String, usize)> = vec![(root.clone(), 0)];
    'scan: while let Some((dir, depth)) = stack.pop() {
        if ctx.cancel.is_cancelled() {
            stop_reason = "cancelled";
            break;
        }
        if started.elapsed() > SCAN_DEADLINE {
            stop_reason = "deadline";
            break;
        }
        let entries = match crate::remote::run_with_budget(ctx, client.read_dir(&dir)).await {
            Ok(entries) => entries,
            Err(_) => continue, // 无权限目录跳过
        };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for (name, is_dir) in entries {
            if is_dir {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        // 目录优先（与本地 list 一致）。
        dirs.sort();
        files.sort();
        for name in &dirs {
            if depth + 1 > args.depth {
                continue;
            }
            let rel = relative_of(&root, &dir, name);
            items.push(format!("{rel}/"));
            stack.push((join(&dir, name), depth + 1));
        }
        for name in &files {
            scanned_files = scanned_files.saturating_add(1);
            if scanned_files >= MAX_SCAN_FILES {
                stop_reason = "scan_limit";
                break 'scan;
            }
            items.push(relative_of(&root, &dir, name));
        }
        if items.len() >= MAX_RESULTS {
            stop_reason = "result_limit";
            break 'scan;
        }
    }

    build_scan_outcome("list", items, scanned_files, 0, started, stop_reason)
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn build_scan_outcome(
    tool: &str,
    items: Vec<String>,
    scanned_files: u64,
    scanned_bytes: u64,
    started: Instant,
    stop_reason: &str,
) -> ToolOutcome {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let total = items.len();
    let body = items.join("\n");
    let mut output = format!(
        "status: succeeded\nscanned_files: {scanned_files}\nscanned_bytes: {scanned_bytes}\nelapsed_ms: {elapsed_ms}\nstop_reason: {stop_reason}\nitems: {total} shown of {total}\n"
    );
    if !body.is_empty() {
        output.push_str(&format!("\n{body}"));
    }
    if stop_reason == "result_limit" {
        output.push_str("\n\n结果达上限。可减小 depth 或收窄 path 后重新浏览。");
    }
    let mut outcome = ToolOutcome::succeeded(tool, output);
    outcome.session_metadata = tpi_core::outcome::ToolMetadata {
        tool: tool.to_string(),
        ..Default::default()
    };
    outcome
}

/// 远端路径解析：相对路径基于 session cwd。
fn resolve_remote_path(ctx: &ToolContext, path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("路径不能为空".into());
    }
    if trimmed.contains('\0') {
        return Err("路径含 NUL".into());
    }
    if trimmed.starts_with('/') {
        Ok(trimmed.to_string())
    } else {
        let cwd = tpi_core::util::lock_mutex(&ctx.shell, "shell")
            .cwd
            .to_string();
        Ok(format!("{}/{}", cwd.trim_end_matches('/'), trimmed))
    }
}

fn join(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

/// 计算相对 root 的展示路径。
fn relative_of(root: &str, dir: &str, name: &str) -> String {
    if dir == root {
        name.to_string()
    } else {
        let rel_dir = dir
            .strip_prefix(root)
            .unwrap_or(dir)
            .trim_start_matches('/');
        format!("{rel_dir}/{name}")
    }
}

fn rejected(tool: &str, detail: &str) -> ToolOutcome {
    failed(
        tool,
        &format!("status: rejected\ntool: {tool}\nerror: invalid\n\n{detail}"),
    )
}

fn failed(tool: &str, output: &str) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        ModelPayload {
            status: ToolStatus::Failed,
            program: Some("ssh".into()),
            exit_code: None,
            duration_ms: 0,
            output: output.to_string(),
            effect: None,
            artifact: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_of_computes_display_path() {
        assert_eq!(relative_of("/p", "/p", "a.txt"), "a.txt");
        assert_eq!(relative_of("/p", "/p/src", "main.rs"), "src/main.rs");
    }

    #[test]
    fn join_keeps_single_slash() {
        assert_eq!(join("/p", "a"), "/p/a");
        assert_eq!(join("/p/", "a"), "/p/a");
    }
}
