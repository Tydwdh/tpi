//! Remote list/search/glob（§45-§48）。
//!
//! - list：SFTP 递归遍历（深度限制、上限），目录带 `/`，格式与本地一致；
//! - search：capability detect（`command -v rg`）→ 远端 rg；fallback grep；
//! - glob：SFTP 遍历 + globset 匹配，按 mtime 降序（与本地 glob 同语义）。
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

/// 远端 search 参数。
#[derive(Debug, Clone)]
pub struct RemoteSearchArgs {
    pub pattern: String,
    pub path: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_results: usize,
}

/// 远端 glob 参数。
#[derive(Debug, Clone)]
pub struct RemoteGlobArgs {
    pub pattern: String,
    pub path: String,
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
        let entries = match client.read_dir(&dir).await {
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

/// remote search：capability detect → rg / fallback grep（§46）。
pub async fn remote_search(
    client: &mut SshClient,
    args: &RemoteSearchArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    let root = match resolve_remote_path(ctx, &args.path) {
        Ok(p) => p,
        Err(e) => return rejected("search", &e),
    };
    if args.pattern.len() > 32 * 1024 {
        return rejected("search", "正则表达式最多 32 KiB。");
    }
    let started = Instant::now();

    // 1. capability detect：远端是否有 rg。
    let has_rg = match client
        .exec(
            "command -v rg >/dev/null 2>&1",
            None,
            &Default::default(),
            None,
        )
        .await
    {
        Ok(r) => r.exit_code == Some(0),
        Err(_) => false,
    };

    // 2. 构造搜索命令。
    // glob 必须 shell_quote：include/exclude 来自模型参数，原样拼接会经
    // 远端 shell 展开（`;`/`$()`/反引号 = 任意命令执行，I3）。
    let max_results = args.max_results.clamp(1, MAX_SEARCH_RESULTS);
    let mut cmd = String::new();
    let include_globs: Vec<String> = args
        .include
        .iter()
        .map(|g| format!("--glob={}", shell_quote(g)))
        .collect();
    let exclude_globs: Vec<String> = args
        .exclude
        .iter()
        .map(|g| format!("--glob=!{}", shell_quote(g)))
        .collect();
    if has_rg {
        // rg --no-heading --line-number -m N --glob=include --glob=!exclude pattern path
        cmd.push_str(&format!(
            "rg --no-heading --line-number --color never -m {max_results} {} {} -- {} {} 2>/dev/null | head -{max_results}",
            include_globs.join(" "),
            exclude_globs.join(" "),
            shell_quote(&args.pattern),
            shell_quote(&root),
        ));
    } else {
        // fallback grep -rn（POSIX 基础能力）：grep 用 --include=PATTERN（
        // 逐个 glob），不能传 rg 的 --glob= 语法。
        let include_extra: String = args
            .include
            .iter()
            .map(|g| format!(" --include={}", shell_quote(g)))
            .collect();
        cmd.push_str(&format!(
            "grep -rn --color never -m 1{} -- {} {} 2>/dev/null | head -{max_results}",
            include_extra,
            shell_quote(&args.pattern),
            shell_quote(&root),
        ));
    }
    let exec_result = match client
        .exec(&cmd, None, &Default::default(), Some(&ctx.cancel))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return failed("search", &format!("远端搜索失败: {e}"));
        }
    };
    let stdout = String::from_utf8_lossy(&exec_result.stdout);
    // 3. 解析输出：path:line:content → items（与本地 search 相同"相对路径"展示）。
    let mut items: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // rg/grep 输出 "path:linenum:content"；去掉远端 root 前缀显示相对路径。
        let trimmed = strip_root_prefix(&root, line);
        items.push(truncate_line(&trimmed, 300));
        if items.len() >= max_results {
            break;
        }
    }
    let scanned_files = items.len() as u64;
    let stop_reason = if items.len() >= max_results {
        "result_limit"
    } else {
        "complete"
    };
    build_scan_outcome("search", items, scanned_files, 0, started, stop_reason)
}

/// remote glob：SFTP 遍历 + globset 匹配（§45/§47）。
pub async fn remote_glob(
    client: &mut SshClient,
    args: &RemoteGlobArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    let root = match resolve_remote_path(ctx, &args.path) {
        Ok(p) => p,
        Err(e) => return rejected("glob", &e),
    };
    // 编译 globset（与本地 glob 同一库）。
    let matcher = match globset::Glob::new(&args.pattern).map_err(|e| e.to_string()) {
        Ok(glob) => match globset::GlobSet::builder().add(glob).build() {
            Ok(m) => m,
            Err(e) => return rejected("glob", &format!("无效 globset: {e}")),
        },
        Err(e) => return rejected("glob", &format!("无效 glob: {e}")),
    };
    let started = Instant::now();
    let mut items: Vec<(String, u64)> = Vec::new();
    let mut scanned_files = 0u64;

    // 迭代式 DFS。
    let mut stack: Vec<String> = vec![root.clone()];
    'scan: while let Some(dir) = stack.pop() {
        if ctx.cancel.is_cancelled() {
            break;
        }
        if started.elapsed() > SCAN_DEADLINE {
            break;
        }
        let entries = match client.read_dir(&dir).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for (name, is_dir) in entries {
            let path = join(&dir, &name);
            let rel = relative_of(&root, &dir, &name);
            if is_dir {
                stack.push(path);
                continue;
            }
            scanned_files = scanned_files.saturating_add(1);
            if scanned_files >= MAX_SCAN_FILES {
                break 'scan;
            }
            // mtime（glob 按最近修改降序，同本地语义）。
            let mtime = match client.stat(&path).await {
                Ok(attrs) => attrs.mtime.unwrap_or(0) as u64,
                Err(_) => 0,
            };
            if matcher.is_match(&rel) {
                items.push((rel, mtime));
            }
        }
    }
    // 按 mtime 降序，同 mtime 按路径字典序。
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let items: Vec<String> = items
        .into_iter()
        .map(|(p, _)| p)
        .take(MAX_RESULTS)
        .collect();
    build_scan_outcome("glob", items, scanned_files, 0, started, "complete")
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
        output.push_str("\n\n结果达上限。可加 exclude 排除目录或收窄 path 后重新搜索。");
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

/// 从匹配行去 root 前缀（rg/grep 输出绝对路径时）。
fn strip_root_prefix(root: &str, line: &str) -> String {
    let prefix = format!("{root}/");
    if let Some(rest) = line.strip_prefix(&prefix) {
        rest.to_string()
    } else if let Some(rest) = line.strip_prefix(root) {
        rest.trim_start_matches(':').to_string()
    } else {
        line.to_string()
    }
}

fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        line.to_string()
    } else {
        let mut end = max;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &line[..end])
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

/// 与本地一致的 search 结果上限。
pub const MAX_SEARCH_RESULTS: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_of_computes_display_path() {
        assert_eq!(relative_of("/p", "/p", "a.txt"), "a.txt");
        assert_eq!(relative_of("/p", "/p/src", "main.rs"), "src/main.rs");
    }

    #[test]
    fn strip_root_prefix_removes_absolute() {
        assert_eq!(
            strip_root_prefix("/p", "/p/src/main.rs:3:x"),
            "src/main.rs:3:x"
        );
        assert_eq!(strip_root_prefix("/p", "main.rs:1:hi"), "main.rs:1:hi");
    }

    #[test]
    fn join_keeps_single_slash() {
        assert_eq!(join("/p", "a"), "/p/a");
        assert_eq!(join("/p/", "a"), "/p/a");
    }
}
