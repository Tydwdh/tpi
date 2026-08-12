//! TPI CLI 入口。
//!
//! ```text
//! tpi                         # 当前目录进入交互会话
//! tpi "修复这个测试"          # 进入交互并提交首条消息
//! tpi -p "解释失败原因"       # 非交互，stdout 只输出最终答案
//! tpi --continue              # 继续当前 workspace 最近 session
//! tpi sessions                # 列出可恢复会话（摘要 + 时间 + id）
//! tpi --resume <session-id>   # 恢复指定 session（完整 id 或唯一前缀）
//! tpi --model <name>
//! tpi --no-session
//! tpi auth set <provider>     # 把 token 写入 Windows Credential Manager
//! tpi auth clear <provider>
//! tpi auth status <provider>
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use tpi::app::{self, SessionTarget};
use tpi::config;

const MAX_INTERACTIVE_INPUT_BYTES: usize = 16 * 1024;

#[derive(Parser, Debug)]
#[command(name = "tpi", version, about = "TPI: 个人终端 Coding Agent")]
struct Cli {
    /// 进入交互并提交首条消息
    prompt: Option<String>,
    /// 非交互模式：stdout 只输出最终答案
    #[arg(short = 'p')]
    prompt_mode: bool,
    /// 显式切换 primary model
    #[arg(long)]
    model: Option<String>,
    /// 不写 session
    #[arg(long, conflicts_with_all = ["continue_session", "resume"])]
    no_session: bool,
    /// 继续当前 workspace 最近 session
    // 字段名会让 clap 生成 `--continue-session`；显式保持稳定的 `--continue` CLI。
    #[arg(long = "continue", conflicts_with = "resume")]
    continue_session: bool,
    /// 恢复指定 session
    #[arg(long)]
    resume: Option<String>,
    /// 工作目录（默认当前目录）
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// 兼容模式：inline 视口（默认 fullscreen）
    #[arg(long)]
    inline: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 凭据管理（写入 Windows Credential Manager，配置只保存 label）。
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// 交互式生成配置（P2：新用户 5 分钟完成配置）。
    Init,
    /// 环境检查（P2：config/模型/API key/Git Bash/目录）。
    Doctor,
    /// 列出/修复当前 workspace 的可恢复会话（摘要 + 时间 + 完整 id），供 --resume 选择。
    Sessions {
        #[command(subcommand)]
        command: Option<SessionsCommand>,
    },
    /// 清理过期 session/artifact（P2：`tpi prune --older-than <days>`）。
    Prune {
        /// 早于该天数（按 mtime）的文件被删除（默认 30）。
        #[arg(long, default_value_t = 30)]
        older_than: u64,
        /// 只列出将删除的文件，不实际删除。
        #[arg(long)]
        dry_run: bool,
    },
    /// 自动评测（Eval Harness：真实 coding task + 可重置 repo + 验收断言）。
    ///
    /// 会调用真实 provider（产生费用）——仅在你显式运行时发生。
    Eval {
        /// 任务 ID（evals/<id>/；与 --suite 互斥）。
        #[arg(conflicts_with_all = ["suite", "list", "list_suites"])]
        task: Option<String>,
        /// 运行整个套件（expected.toml 的 suite 字段；与 task 互斥）。
        #[arg(long, conflicts_with_all = ["task", "list", "list_suites"])]
        suite: Option<String>,
        /// 列出全部任务（不运行、不花钱）。
        #[arg(long, conflicts_with_all = ["task", "suite", "list_suites"])]
        list: bool,
        /// 列出全部套件（不运行、不花钱）。
        #[arg(long, conflicts_with_all = ["task", "suite", "list"])]
        list_suites: bool,
        /// 结果目录（默认 ~/.tpi/evals/results）。
        #[arg(long)]
        results: Option<PathBuf>,
        /// evals 根目录（默认 <workspace>/evals）。
        #[arg(long)]
        evals: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum SessionsCommand {
    /// 列出当前 workspace 的可恢复会话（摘要 + 时间 + 完整 id），供 --resume 选择。
    List,
    /// 诊断并修复损坏的 session（中间坏行导致无法恢复，P0-2）。
    /// 修复前自动备份，坏行隔离到 `<session>.quarantine`。
    Repair {
        /// 只诊断（显示坏行位置），不修改文件。
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCommand {
    /// 保存凭据（token 从 stdin 读取，不回显提示由调用方控制）。
    Set { provider: String },
    /// 清除已保存凭据。
    Clear { provider: String },
    /// 查询凭据状态。
    Status { provider: String },
}

/// Windows 控制台默认代码页（中文系统为 GBK/936）会把 UTF-8 输出显示成乱码；
/// 启动时把输入/输出代码页切到 UTF-8（Win10+ 稳定支持；重定向到文件/管道不受影响）。
#[cfg(windows)]
fn setup_console_utf8() {
    use windows_sys::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
    const CP_UTF8: u32 = 65001;
    // SAFETY: These process-wide console setters accept a numeric code page and
    // have no pointer or lifetime preconditions. Failure is intentionally benign.
    unsafe {
        SetConsoleCP(CP_UTF8);
        SetConsoleOutputCP(CP_UTF8);
    }
}

#[cfg(not(windows))]
fn setup_console_utf8() {}

fn main() {
    setup_console_utf8();
    // §11.5：单二进制 process-host 模式（隐藏进程，等待控制管道上的 start token）。
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("__process-host") {
        std::process::exit(tpi::process::host::run_host());
    }
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("错误: {error}");
        std::process::exit(1);
    }
}

/// P1-14：在闭包执行期间切换控制台输入回显（Windows；非控制台场景无操作）。
/// `auth set` 的 token 输入不回显，避免凭据明文留在终端。
fn with_input_echo<T>(echo: bool, f: impl FnOnce() -> T) -> T {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        };
        struct ConsoleModeGuard {
            handle: windows_sys::Win32::Foundation::HANDLE,
            original_mode: u32,
        }

        impl Drop for ConsoleModeGuard {
            fn drop(&mut self) {
                // SAFETY: The handle was obtained from GetStdHandle and was
                // confirmed to be a console when this guard was constructed.
                unsafe {
                    SetConsoleMode(self.handle, self.original_mode);
                }
            }
        }

        // SAFETY: GetStdHandle has no caller-owned pointer arguments.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode: u32 = 0;
        // SAFETY: `mode` points to live writable storage for this call.
        let ok = unsafe { GetConsoleMode(handle, &mut mode) != 0 };
        let _guard = ok.then(|| ConsoleModeGuard {
            handle,
            original_mode: mode,
        });
        if ok {
            let requested_mode = if echo {
                mode | ENABLE_ECHO_INPUT
            } else {
                mode & !ENABLE_ECHO_INPUT
            };
            // SAFETY: GetConsoleMode established that handle is a console handle.
            unsafe {
                SetConsoleMode(handle, requested_mode);
            }
        }
        f()
    }
    #[cfg(not(windows))]
    {
        let _ = echo;
        f()
    }
}

fn read_stdin_line_bounded(max_bytes: usize) -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    match tpi::util::read_line_bounded(&mut lock, max_bytes)? {
        tpi::util::BoundedLineRead::Eof => Ok(String::new()),
        tpi::util::BoundedLineRead::TooLong => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("输入超过 {max_bytes} 字节上限"),
        )),
        tpi::util::BoundedLineRead::Line(line) => String::from_utf8(line.bytes).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("输入不是有效 UTF-8: {error}"),
            )
        }),
    }
}

/// `tpi eval` 入口（Eval Harness）。
///
/// - `--list` / `--list-suites`：不运行、不花钱；
/// - 运行模式调用真实 provider（花钱），串行执行任务。
fn run_eval_cli(
    cwd: Option<&Path>,
    task: Option<&str>,
    suite: Option<&str>,
    list: bool,
    list_suites: bool,
    results: Option<&Path>,
    evals: Option<&Path>,
) -> Result<(), String> {
    let workspace_root = current_workspace_root(cwd)?;
    let evals_root = evals
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| workspace_root.join(tpi::eval::EVALS_DIR).into());

    // 只读模式：列出任务/套件（不初始化日志/配置）。
    if list_suites {
        for suite in tpi::eval::list_suites(&evals_root)? {
            println!("{suite}");
        }
        return Ok(());
    }
    if list {
        for task in tpi::eval::discover(&evals_root)? {
            println!(
                "{} [{}] {}",
                task.id,
                if task.expected.suite.is_empty() {
                    "-"
                } else {
                    &task.expected.suite
                },
                task.expected.title.as_deref().unwrap_or("(no title)")
            );
        }
        return Ok(());
    }

    // 运行模式：需要配置与 provider 凭据。
    let tasks = tpi::eval::discover(&evals_root)?;
    let selected: Vec<tpi::eval::TaskEntry> = if let Some(task_id) = task {
        tasks.into_iter().filter(|t| t.id == task_id).collect()
    } else if let Some(suite_name) = suite {
        tasks
            .into_iter()
            .filter(|t| t.expected.suite == suite_name)
            .collect()
    } else {
        return Err("请指定任务 ID 或 --suite（或 --list 查看全部任务）".into());
    };
    if selected.is_empty() {
        let what = task
            .map(|t| format!("任务 {t}"))
            .or_else(|| suite.map(|s| format!("套件 {s}")))
            .unwrap_or_default();
        return Err(format!("{what} 不存在（--list 查看）"));
    }

    // 配置与日志（复用用户 provider 配置；评测 session 独立）。
    init_logging()?;
    let config = config::load(&workspace_root, None)?;
    let results_dir = results
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| tpi::config::tpi_home().join(tpi::eval::RESULTS_DIR));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let mut passed = 0usize;
        let mut failed = 0usize;
        for task in &selected {
            println!("==== eval {} ====", task.id);
            match tpi::eval::run_task(task, &results_dir, &config).await {
                Ok(result) => {
                    print!("{}", tpi::eval::render_summary(&result));
                    if result.success && result.verification_passed {
                        passed += 1;
                    } else {
                        failed += 1;
                    }
                }
                Err(error) => {
                    println!("task 运行失败: {error}");
                    failed += 1;
                }
            }
        }
        println!("==== 汇总: pass={passed} fail={failed} ====");
        if failed == 0 {
            Ok(())
        } else {
            Err(format!("{failed} 个 eval 任务失败"))
        }
    })
}

/// P2：`tpi prune`——清理 ~/.tpi 下超过 N 天的 session/artifact 文件。
/// 返回被删除的文件数（dry_run 时只列出）。
fn prune_old_data(older_than_days: u64, dry_run: bool) -> Result<(), String> {
    let home = tpi::config::tpi_home();
    let cutoff = retention_cutoff(std::time::SystemTime::now(), older_than_days)?;
    let sessions_root = home.join("sessions");
    let active_sessions = active_session_ids(&sessions_root)?;
    let mut removed = 0usize;
    for root in [sessions_root, home.join("artifacts")] {
        if !root.exists() {
            continue;
        }
        for entry in walk_files(&root)? {
            let file_name = entry
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            // 锁文件本身永不由 prune 删除；孤儿锁体积很小，保留它比在检查与
            // 删除间破坏另一个进程的独占锁安全得多。
            if file_name.ends_with(".jsonl.lock") {
                continue;
            }
            let belongs_to_active_session = entry.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|part| active_sessions.contains(part))
            }) || entry
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| active_sessions.contains(stem));
            if belongs_to_active_session {
                continue;
            }
            let meta = std::fs::symlink_metadata(&entry)
                .map_err(|e| format!("读取 {} 元数据失败: {e}", entry.display()))?;
            let modified = meta
                .modified()
                .map_err(|e| format!("读取 {} 修改时间失败: {e}", entry.display()))?;
            if modified < cutoff {
                if dry_run {
                    println!("[dry-run] {}", entry.display());
                } else {
                    std::fs::remove_file(&entry)
                        .map_err(|e| format!("删除 {} 失败: {e}", entry.display()))?;
                }
                removed += 1;
            }
        }
    }
    if dry_run {
        println!("将删除 {removed} 个文件（早于 {older_than_days} 天）");
    } else {
        println!("已删除 {removed} 个过期文件（早于 {older_than_days} 天）");
    }
    Ok(())
}

/// `tpi sessions repair`：诊断并修复当前 workspace 的损坏 session（P0-2）。
/// 中间坏行会导致 session 无法恢复；repair 备份 → 隔离坏行 → 重写 → 重建。
/// `--dry-run` 只诊断显示坏行位置，不修改文件。
fn repair_sessions(
    sessions_root: &std::path::Path,
    workspace_root: &Utf8PathBuf,
    dry_run: bool,
) -> Result<(), String> {
    let workspace_id = tpi::session::workspace_id_for(workspace_root.as_std_path());
    let dir = sessions_root.join(&workspace_id);
    let mut any_issue = false;
    if !dir.exists() {
        println!("当前 workspace 没有历史会话（首次提交消息后创建）");
        return Ok(());
    }
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("读取会话目录失败: {e}"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|e| e == "jsonl").unwrap_or(false))
        .collect();
    files.sort();
    for path in files {
        // 跳过修复/备份的附属文件（如 .bak-* / .quarantine 也是 .jsonl 结尾，
        // 但它们的 stem 不是 UUID，diagnose 会因 session_id 不匹配而误报）。
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if uuid::Uuid::parse_str(stem).is_err() {
            continue;
        }
        let bad = tpi::session::repair::diagnose(&path)
            .map_err(|e| format!("诊断 {} 失败: {e}", path.display()))?;
        if bad.is_empty() {
            continue;
        }
        any_issue = true;
        println!("会话 {} 损坏（{} 行）:", stem, bad.len());
        for line in &bad {
            println!("  L{}: {}", line.line, line.reason);
        }
        if dry_run {
            println!("  [dry-run] 未修改（`tpi sessions repair` 修复）\n");
            continue;
        }
        let report = tpi::session::repair::repair(&path)
            .map_err(|e| format!("修复 {} 失败: {e}", path.display()))?;
        println!(
            "  已修复：隔离 {} 行，重建 seq={}，合成 Interrupted={}\n",
            report.removed.len(),
            report.max_seq,
            report.synthesized_interrupted
        );
    }
    if !any_issue {
        println!("当前 workspace 的 session 全部健康");
    }
    Ok(())
}

fn active_session_ids(
    sessions_root: &std::path::Path,
) -> Result<std::collections::HashSet<String>, String> {
    let mut active = std::collections::HashSet::new();
    if !sessions_root.exists() {
        return Ok(active);
    }
    for path in walk_files(sessions_root)? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl.lock") else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        let locked = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => match file.try_lock() {
                Ok(()) => false,
                Err(std::fs::TryLockError::WouldBlock) => true,
                Err(std::fs::TryLockError::Error(_)) => true,
            },
            Err(_) => true,
        };
        if locked {
            active.insert(id.to_string());
        }
    }
    Ok(active)
}

fn retention_cutoff(
    now: std::time::SystemTime,
    older_than_days: u64,
) -> Result<std::time::SystemTime, String> {
    let seconds = older_than_days
        .checked_mul(86_400)
        .ok_or_else(|| "天数过大".to_string())?;
    now.checked_sub(std::time::Duration::from_secs(seconds))
        .ok_or_else(|| "天数超出系统时间范围".to_string())
}

/// 递归收集文件（prune 用）。目录链接和 Windows reparse point 一律不跟随。
fn walk_files(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, String> {
    const MAX_WALK_ENTRIES: usize = 100_000;
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|e| format!("读取目录 {} 失败: {e}", current.display()))?;
        for entry in entries {
            visited = visited.saturating_add(1);
            if visited > MAX_WALK_ENTRIES {
                return Err(format!("清理扫描超过 {MAX_WALK_ENTRIES} 个目录项，已停止"));
            }
            let entry = entry.map_err(|e| format!("读取目录项 {} 失败: {e}", current.display()))?;
            let path = entry.path();
            // 不跟随符号链目录：prune 只应在 TPI 管理目录内递归，
            // 否则链到外部目录时会把外部文件也删掉（安全陷阱）。
            let ft = entry
                .file_type()
                .map_err(|e| format!("读取 {} 类型失败: {e}", path.display()))?;
            if tpi::util::is_symlink_or_reparse(&path)
                .map_err(|e| format!("读取 {} 元数据失败: {e}", path.display()))?
            {
                if !ft.is_dir() {
                    files.push(path);
                }
                continue;
            }
            if ft.is_dir() {
                pending.push(path);
            } else if ft.is_file() || ft.is_symlink() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// 断言：prnune 遍历不得跟随符号链目录（防止删除 TPI 目录外的文件）。
#[cfg(unix)]
#[test]
fn walk_files_does_not_follow_symlink_dirs() {
    use std::os::unix::fs::symlink;
    let tmp = std::env::temp_dir().join(format!("tpi-prune-test-{}", std::process::id()));
    let sessions = tmp.join("sessions");
    let outside = tmp.join("outside");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"x").unwrap();
    std::fs::write(sessions.join("keep.txt"), b"x").unwrap();
    let link = sessions.join("link");
    symlink(&outside, &link).unwrap();
    let files = walk_files(&sessions).unwrap();
    let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    assert!(
        !paths.iter().any(|p| p.contains("secret.txt")),
        "不得跟随 symlink 目录收集外部文件: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.ends_with("keep.txt")),
        "正常文件仍应收集: {paths:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// P2：`tpi init`——交互式生成 ~/.tpi/config.toml。
/// 每个问题给默认值（回车接受）；API key 可选写入凭据管理器。
fn run_init() -> Result<(), String> {
    use std::io::Write;
    let home = tpi::config::tpi_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("创建 {} 失败: {e}", home.display()))?;
    let config_path = home.join("config.toml");
    if config_path.exists() {
        print!("{} 已存在，覆盖？(y/N) ", config_path.display());
        std::io::stdout().flush().ok();
        let answer = read_stdin_line_bounded(MAX_INTERACTIVE_INPUT_BYTES)
            .map_err(|e| format!("读取输入失败: {e}"))?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            return Err("已取消（保留现有配置）".into());
        }
    }
    let prompt = |label: &str, default: &str| -> Result<String, String> {
        print!("{label} [{default}]: ");
        std::io::stdout().flush().ok();
        let answer = read_stdin_line_bounded(MAX_INTERACTIVE_INPUT_BYTES)
            .map_err(|e| format!("读取输入失败: {e}"))?;
        let answer = answer.trim();
        Ok(if answer.is_empty() {
            default.to_string()
        } else {
            answer.to_string()
        })
    };
    let provider = prompt("provider", "opencode-go")?;
    let name = prompt("model name", "deepseek-v4-flash")?;
    let base_url = prompt("base_url", "https://<你的端点>/v1")?;
    let max_output = prompt("max_output_tokens（回车默认 16384）", "16384")?;
    let context_window = prompt("context_window（回车默认 1000000）", "1000000")?;
    let api_key_env = prompt("API key 环境变量名（回车默认 TPI_API_KEY）", "TPI_API_KEY")?;
    // 先验证配置，避免输入无效时已向凭据管理器留下副作用。
    let content = render_initial_config(
        &provider,
        &name,
        &base_url,
        &max_output,
        &context_window,
        &api_key_env,
    )?;
    print!("写入 API key 到凭据管理器？(y/N) ");
    std::io::stdout().flush().ok();
    let answer = read_stdin_line_bounded(MAX_INTERACTIVE_INPUT_BYTES)
        .map_err(|e| format!("读取输入失败: {e}"))?;
    let token = if answer.trim().eq_ignore_ascii_case("y") {
        print!("输入 token（粘贴后回车，输入不回显）: ");
        std::io::stdout().flush().ok();
        let token = with_input_echo(false, || {
            read_stdin_line_bounded(tpi::auth::MAX_TOKEN_BYTES)
        })
        .map_err(|e| format!("读取输入失败: {e}"))?;
        let token = token.trim_end_matches(['\r', '\n']);
        if token.is_empty() {
            return Err("token 为空".into());
        }
        Some(token.to_string())
    } else {
        None
    };
    std::fs::write(&config_path, content)
        .map_err(|e| format!("写入 {} 失败: {e}", config_path.display()))?;
    if let Some(token) = token {
        tpi::auth::auth_set(&provider, &token).map_err(|error| {
            format!(
                "配置已写入 {}，但保存凭据失败: {error}",
                config_path.display()
            )
        })?;
    }
    println!("已生成 {}", config_path.display());
    println!("下一步：`tpi doctor` 检查环境，然后直接运行 `tpi`。");
    Ok(())
}

fn render_initial_config(
    provider: &str,
    name: &str,
    base_url: &str,
    max_output: &str,
    context_window: &str,
    api_key_env: &str,
) -> Result<String, String> {
    let max_output: u32 = max_output
        .parse()
        .map_err(|_| "max_output_tokens 必须是 0..=4294967295 的整数".to_string())?;
    let context_window: u64 = context_window
        .parse()
        .map_err(|_| "context_window 必须是非负整数".to_string())?;
    let url = reqwest::Url::parse(base_url).map_err(|error| format!("base_url 无效: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("base_url 只支持 http 或 https".into());
    }
    if provider.trim().is_empty() || name.trim().is_empty() || api_key_env.trim().is_empty() {
        return Err("provider、model name 与 API key 环境变量名不能为空".into());
    }
    if api_key_env.contains('=') || api_key_env.chars().any(char::is_whitespace) {
        return Err("API key 环境变量名不能包含空白或 `=`".into());
    }
    if max_output == 0 || context_window == 0 || u64::from(max_output) > context_window {
        return Err("token 上限必须大于 0，且 max_output_tokens 不能超过 context_window".into());
    }
    let string_literal = |value: &str| toml::Value::String(value.to_string()).to_string();
    Ok(format!(
        "[model.primary]\nprovider = {}\nname = {}\nbase_url = {}\nmax_output_tokens = {max_output}\ncontext_window = {context_window}\napi_key_env = {}\n# price_input = 0.0       # 每百万输入 token 美元（可选）\n# price_output = 0.0      # 每百万输出 token 美元\n",
        string_literal(provider),
        string_literal(name),
        string_literal(base_url),
        string_literal(api_key_env),
    ))
}

/// 解析工作目录（默认当前目录）。
fn current_workspace_root(cwd: Option<&std::path::Path>) -> Result<Utf8PathBuf, String> {
    let requested = match cwd {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let canonical = requested
        .canonicalize()
        .map_err(|error| format!("工作目录 {} 不可用: {error}", requested.display()))?;
    if !canonical.is_dir() {
        return Err(format!("工作目录不是目录: {}", canonical.display()));
    }
    Utf8PathBuf::from_path_buf(canonical)
        .map_err(|path| format!("路径不是 UTF-8: {}", path.display()))
}

fn run(cli: Cli) -> Result<(), String> {
    // auth 子命令不需要完整配置/日志。
    if let Some(Command::Auth { command }) = &cli.command {
        return match command {
            AuthCommand::Set { provider } => {
                print!("输入 token（粘贴后回车，输入不回显）: ");
                std::io::stdout().flush().map_err(|e| e.to_string())?;
                // P1-14：隐藏回显（此前 token 明文显示在终端）。
                let token = with_input_echo(false, || {
                    read_stdin_line_bounded(tpi::auth::MAX_TOKEN_BYTES)
                })
                .map_err(|e| format!("读取输入失败: {e}"))?;
                let token = token.trim_end_matches(['\r', '\n']);
                if token.is_empty() {
                    return Err("token 为空".into());
                }
                tpi::auth::auth_set(provider, token)?;
                println!("已保存凭据: {provider}（Windows Credential Manager，§18.4）");
                Ok(())
            }
            AuthCommand::Clear { provider } => {
                tpi::auth::auth_clear(provider)?;
                println!("已清除凭据: {provider}");
                Ok(())
            }
            AuthCommand::Status { provider } => match tpi::auth::auth_get(provider)? {
                Some(_) => {
                    println!("凭据已配置: {provider}");
                    Ok(())
                }
                None => Err(format!(
                    "未配置凭据: {provider}（用 `tpi auth set {provider}` 保存）"
                )),
            },
        };
    }

    // P2：tpi init——交互式生成 ~/.tpi/config.toml。
    if matches!(cli.command, Some(Command::Init)) {
        return run_init();
    }
    // P2：tpi doctor——环境检查。
    if matches!(cli.command, Some(Command::Doctor)) {
        let workspace_root = current_workspace_root(cli.cwd.as_deref())?;
        print!("{}", tpi::doctor::render_report(&workspace_root));
        return Ok(());
    }

    // §用户诉求（恢复会话可判断）：`tpi sessions`——列出当前 workspace 的
    // 可恢复会话（首条消息摘要 + 时间 + 完整 id），不再让用户面对文件系统里
    // 的 UUID 哈希文件名。
    if let Some(Command::Sessions { command }) = &cli.command {
        let workspace_root = current_workspace_root(cli.cwd.as_deref())?;
        let sessions_root = tpi::config::tpi_home().join("sessions");
        match command {
            None | Some(SessionsCommand::List) => {
                let sessions = tpi::app::list_sessions(&sessions_root, &workspace_root)?;
                if sessions.is_empty() {
                    println!("当前 workspace 没有历史会话（首次提交消息后创建）");
                    return Ok(());
                }
                println!(
                    "{} 个会话（按最近使用排序；--resume 可接完整 id 或唯一前缀）：",
                    sessions.len()
                );
                for (i, (id, modified, count, preview)) in sessions.iter().enumerate() {
                    let title = if preview.is_empty() {
                        "(无标题)".to_string()
                    } else {
                        preview.clone()
                    };
                    println!(
                        "{}  {}  {} 事件  {}\n      id: {}",
                        i + 1,
                        fmt_session_datetime(*modified),
                        count,
                        title,
                        id
                    );
                }
                return Ok(());
            }
            Some(SessionsCommand::Repair { dry_run }) => {
                return repair_sessions(&sessions_root, &workspace_root, *dry_run);
            }
        }
    }

    // P2：`tpi prune`——清理过期 session/artifact（~/.tpi/sessions 与 artifacts）。
    if let Some(Command::Prune {
        older_than,
        dry_run,
    }) = &cli.command
    {
        return prune_old_data(*older_than, *dry_run);
    }

    // Eval Harness：`tpi eval`（真实 provider，花钱——仅在显式运行时）。
    if let Some(Command::Eval {
        task,
        suite,
        list,
        list_suites,
        results,
        evals,
    }) = &cli.command
    {
        return run_eval_cli(
            cli.cwd.as_deref(),
            task.as_deref(),
            suite.as_deref(),
            *list,
            *list_suites,
            results.as_deref(),
            evals.as_deref(),
        );
    }

    // 日志（§19.2）：tracing 写 ~/.tpi/logs/tpi.log，不污染 stdout。
    init_logging()?;
    let workspace_root: Utf8PathBuf = current_workspace_root(cli.cwd.as_deref())?;
    let mut config = config::load(&workspace_root, cli.model.as_deref())?;
    // §1.2：`--inline` 覆盖 `[ui] mode`（兼容模式，仅特殊终端环境使用）。
    if cli.inline {
        config.ui_mode = tpi::tui::terminal::ViewMode::Inline;
    }

    if cli.no_session && (cli.continue_session || cli.resume.is_some()) {
        return Err("--no-session 不能与 --continue/--resume 同时使用".into());
    }

    // §用户诉求：--resume 支持完整 UUID 或唯一前缀（resolve_session_id_prefix
    // 会补全），避免手抄/记忆 36 位哈希 id。
    let session_target = if cli.continue_session {
        SessionTarget::Continue
    } else if let Some(id) = cli.resume {
        let resolved =
            tpi::app::resolve_session_id_prefix(&id, &config.sessions_root, &workspace_root)?;
        SessionTarget::Resume(resolved.to_string())
    } else {
        SessionTarget::New
    };

    // 使用多线程 runtime（§5.3）。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    tracing::info!(workspace = %workspace_root, model = %config.model.name, "tpi starting");
    runtime.block_on(app::run(
        config,
        session_target,
        cli.prompt.as_deref().unwrap_or(""),
        cli.prompt_mode,
        cli.no_session,
    ))
}

/// `tpi sessions` 展示用的时间（YYYY-MM-DD HH:MM）。
fn fmt_session_datetime(t: std::time::SystemTime) -> String {
    // From<SystemTime> 对文件 mtime（远小于 64-bit 时间戳范围）是安全的。
    let dt = time::OffsetDateTime::from(t);
    let date = dt.date();
    let tod = dt.time();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        date.year(),
        u8::from(date.month()),
        date.day(),
        tod.hour(),
        tod.minute()
    )
}

fn init_logging() -> Result<(), String> {
    let log_dir = config::tpi_home().join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    let file = tracing_appender::rolling::daily(log_dir, "tpi.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file);
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(false),
        )
        .with(
            // 默认 INFO：try_from_default_env 在未设置 RUST_LOG 时返回 error 级
            // 默认过滤器，会把 info/warn 全部吞掉（日志文件永远为空，无法诊断）。
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy(),
        );
    subscriber.init();
    // 保持 non-blocking writer guard 存活（程序生命周期内）。
    Box::leak(Box::new(_guard));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// 稳定 CLI：`--continue` 必须可解析。
    /// （此前 `#[arg(long)] continue_session` 只生成 `--continue-session`。）
    #[test]
    fn continue_flag_matches_design_doc() {
        let cli = Cli::parse_from(["tpi", "--continue"]);
        assert!(cli.continue_session);
    }

    /// §用户诉求：`tpi sessions` 子命令可解析（列出可恢复会话）。
    #[test]
    fn sessions_subcommand_parses() {
        let cli = Cli::parse_from(["tpi", "sessions"]);
        assert!(matches!(
            cli.command,
            Some(Command::Sessions { command: None })
        ));
        let cli = Cli::parse_from(["tpi", "sessions", "list"]);
        assert!(matches!(
            cli.command,
            Some(Command::Sessions {
                command: Some(SessionsCommand::List)
            })
        ));
        // 顶层 --cwd 与 sessions 可组合（列出其他 workspace）。
        let cli = Cli::parse_from(["tpi", "--cwd", ".", "sessions"]);
        assert!(matches!(
            cli.command,
            Some(Command::Sessions { command: None })
        ));
        // P0-2：`tpi sessions repair`（P0-2 修复损坏 session）。
        let cli = Cli::parse_from(["tpi", "sessions", "repair"]);
        assert!(matches!(
            cli.command,
            Some(Command::Sessions {
                command: Some(SessionsCommand::Repair { dry_run: false })
            })
        ));
        let cli = Cli::parse_from(["tpi", "sessions", "repair", "--dry-run"]);
        assert!(matches!(
            cli.command,
            Some(Command::Sessions {
                command: Some(SessionsCommand::Repair { dry_run: true })
            })
        ));
    }

    /// §用户诉求：--resume 接受任意字符串（前缀匹配在 run 时解析），CLI 不校验。
    #[test]
    fn resume_accepts_prefix_or_full_id() {
        let cli = Cli::parse_from(["tpi", "--resume", "019feea2"]);
        assert_eq!(cli.resume.as_deref(), Some("019feea2"));
    }

    /// 稳定 CLI：`tpi auth set <provider>` 子命令形态。
    /// （此前是 `tpi auth <provider> --set`。）
    #[test]
    fn auth_set_subcommand_matches_design_doc() {
        let cli = Cli::parse_from(["tpi", "auth", "set", "opencode-go"]);
        match cli.command.expect("auth subcommand") {
            Command::Auth {
                command: AuthCommand::Set { provider },
            } => assert_eq!(provider, "opencode-go"),
            other => panic!("expected auth set, got {other:?}"),
        }
    }

    #[test]
    fn auth_clear_and_status_subcommands() {
        let cli = Cli::parse_from(["tpi", "auth", "clear", "brave"]);
        assert!(matches!(
            cli.command.expect("auth subcommand"),
            Command::Auth {
                command: AuthCommand::Clear { provider }
            } if provider == "brave"
        ));
        let cli = Cli::parse_from(["tpi", "auth", "status", "opencode-go"]);
        assert!(matches!(
            cli.command.expect("auth subcommand"),
            Command::Auth {
                command: AuthCommand::Status { provider }
            } if provider == "opencode-go"
        ));
    }

    /// P2：`tpi init` / `tpi doctor` 子命令可解析。
    #[test]
    fn init_and_doctor_subcommands_parse() {
        assert!(matches!(
            Cli::parse_from(["tpi", "init"]).command,
            Some(Command::Init)
        ));
        assert!(matches!(
            Cli::parse_from(["tpi", "doctor"]).command,
            Some(Command::Doctor)
        ));
        assert!(matches!(
            Cli::parse_from(["tpi", "prune", "--older-than", "7", "--dry-run"]).command,
            Some(Command::Prune {
                older_than: 7,
                dry_run: true
            })
        ));
    }

    /// Eval Harness：`tpi eval` 子命令形态。
    #[test]
    fn eval_subcommand_parses() {
        assert!(matches!(
            Cli::parse_from(["tpi", "eval", "rust-fix-001"]).command,
            Some(Command::Eval {
                task: Some(t),
                suite: None,
                list: false,
                list_suites: false,
                ..
            }) if t == "rust-fix-001"
        ));
        assert!(matches!(
            Cli::parse_from(["tpi", "eval", "--suite", "core"]).command,
            Some(Command::Eval {
                task: None,
                suite: Some(s),
                ..
            }) if s == "core"
        ));
        assert!(matches!(
            Cli::parse_from(["tpi", "eval", "--list"]).command,
            Some(Command::Eval { list: true, .. })
        ));
        assert!(matches!(
            Cli::parse_from(["tpi", "eval", "--list-suites"]).command,
            Some(Command::Eval {
                list_suites: true,
                ..
            })
        ));
    }

    #[test]
    fn mutually_exclusive_session_and_eval_modes_are_rejected() {
        assert!(Cli::try_parse_from(["tpi", "--continue", "--resume", "id"]).is_err());
        assert!(Cli::try_parse_from(["tpi", "--no-session", "--continue"]).is_err());
        assert!(Cli::try_parse_from(["tpi", "eval", "task", "--suite", "core"]).is_err());
        assert!(Cli::try_parse_from(["tpi", "eval", "--list", "--list-suites"]).is_err());
    }

    #[test]
    fn retention_cutoff_rejects_overflow() {
        assert!(retention_cutoff(std::time::SystemTime::now(), u64::MAX).is_err());
    }

    #[test]
    fn active_session_ids_reports_exclusively_locked_sessions() {
        let root =
            std::env::temp_dir().join(format!("tpi-active-session-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let session_id = uuid::Uuid::now_v7().to_string();
        let lock_path = root.join(format!("{session_id}.jsonl.lock"));
        let lock = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        lock.lock().unwrap();

        let active = active_session_ids(&root).unwrap();
        assert!(active.contains(&session_id));

        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_config_escapes_strings_and_honors_custom_api_key_env() {
        let text = render_initial_config(
            "provider\"quoted",
            "model",
            "https://example.invalid/v1",
            "16384",
            "1000000",
            "CUSTOM_API_KEY",
        )
        .unwrap();
        let parsed: tpi::config::ConfigFile = toml::from_str(&text).unwrap();
        let primary = parsed.model.primary.unwrap();
        assert_eq!(primary.provider, "provider\"quoted");
        assert_eq!(primary.api_key_env.as_deref(), Some("CUSTOM_API_KEY"));
    }

    #[test]
    fn init_config_rejects_invalid_numbers_and_url_scheme() {
        assert!(
            render_initial_config("p", "m", "https://example.invalid", "x", "10", "K").is_err()
        );
        assert!(render_initial_config("p", "m", "file:///tmp/model", "1", "10", "K").is_err());
    }

    #[test]
    fn workspace_root_requires_an_existing_directory_and_is_canonical() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = current_workspace_root(Some(temp.path())).unwrap();
        assert_eq!(canonical.as_std_path(), temp.path().canonicalize().unwrap());
        assert!(current_workspace_root(Some(&temp.path().join("missing"))).is_err());
        let file = temp.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(current_workspace_root(Some(&file)).is_err());
    }
}
