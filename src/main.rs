//! TPI 入口（文档 §18.3 CLI）。
//!
//! ```text
//! tpi                         # 当前目录进入交互会话
//! tpi "修复这个测试"          # 进入交互并提交首条消息
//! tpi -p "解释失败原因"       # 非交互，stdout 只输出最终答案
//! tpi --continue              # 继续当前 workspace 最近 session
//! tpi --resume <session-id>   # 恢复指定 session
//! tpi --model <name>
//! tpi --no-session
//! tpi auth set <provider>     # 把 token 写入 Windows Credential Manager（§18.4）
//! tpi auth clear <provider>
//! tpi auth status <provider>
//! ```

use std::io::Write;
use std::path::PathBuf;

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use tpi::app::{self, SessionTarget};
use tpi::config;

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
    #[arg(long)]
    no_session: bool,
    /// 继续当前 workspace 最近 session
    // P0-6：设计文档/README 写的是 `--continue`；clap 默认按字段名生成
    // `--continue-session`，显式指定 long 名对齐文档。
    #[arg(long = "continue")]
    continue_session: bool,
    /// 恢复指定 session
    #[arg(long)]
    resume: Option<String>,
    /// 工作目录（默认当前目录）
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// 兼容模式：inline 视口（默认 fullscreen，§1.2）
    #[arg(long)]
    inline: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 凭据管理（§18.4：写入 Windows Credential Manager，配置只保存 label）。
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// 交互式生成配置（P2：新用户 5 分钟完成配置）。
    Init,
    /// 环境检查（P2：config/模型/API key/Git Bash/目录）。
    Doctor,
    /// 清理过期 session/artifact（P2：`tpi prune --older-than <days>`）。
    Prune {
        /// 早于该天数（按 mtime）的文件被删除（默认 30）。
        #[arg(long, default_value_t = 30)]
        older_than: u64,
        /// 只列出将删除的文件，不实际删除。
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

fn main() {
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
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode: u32 = 0;
            let ok = GetConsoleMode(handle, &mut mode) != 0;
            if ok {
                if echo {
                    SetConsoleMode(handle, mode | ENABLE_ECHO_INPUT);
                } else {
                    SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT);
                }
            }
            let result = f();
            if ok {
                // 恢复原模式（无论闭包是否 panic）。
                SetConsoleMode(handle, mode);
            }
            result
        }
    }
    #[cfg(not(windows))]
    {
        let _ = echo;
        f()
    }
}

/// P2：`tpi prune`——清理 ~/.tpi 下超过 N 天的 session/artifact 文件。
/// 返回被删除的文件数（dry_run 时只列出）。
fn prune_old_data(older_than_days: u64, dry_run: bool) -> Result<(), String> {
    let home = tpi::config::tpi_home();
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(older_than_days * 86400))
        .ok_or_else(|| "无效天数".to_string())?;
    let mut removed = 0usize;
    for root in [home.join("sessions"), home.join("artifacts")] {
        if !root.exists() {
            continue;
        }
        for entry in walk_files(&root) {
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(modified) = meta.modified() else {
                continue;
            };
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

/// 递归收集文件（prune 用；目录为空时尝试删除）。
fn walk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files
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
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| format!("读取输入失败: {e}"))?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            return Err("已取消（保留现有配置）".into());
        }
    }
    let prompt = |label: &str, default: &str| -> Result<String, String> {
        print!("{label} [{default}]: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
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
    print!("写入 API key 到凭据管理器？(y/N) ");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| format!("读取输入失败: {e}"))?;
    if answer.trim().eq_ignore_ascii_case("y") {
        print!("输入 token（粘贴后回车，输入不回显）: ");
        std::io::stdout().flush().ok();
        let mut token = String::new();
        let read = with_input_echo(false, || std::io::stdin().read_line(&mut token));
        read.map_err(|e| format!("读取输入失败: {e}"))?;
        let token = token.trim();
        if token.is_empty() {
            return Err("token 为空".into());
        }
        tpi::auth::auth_set(&provider, token)?;
    }
    let content = format!(
        "[model.primary]\nprovider = \"{provider}\"\nname = \"{name}\"\nbase_url = \"{base_url}\"\nmax_output_tokens = {max_output}\ncontext_window = {context_window}\n# api_key_env = \"{api_key_env}\"   # 环境变量显式覆盖（§18.4）\n"
    );
    std::fs::write(&config_path, content)
        .map_err(|e| format!("写入 {} 失败: {e}", config_path.display()))?;
    println!("已生成 {}", config_path.display());
    println!("下一步：`tpi doctor` 检查环境，然后直接运行 `tpi`。");
    Ok(())
}

/// 解析工作目录（默认当前目录）。
fn current_workspace_root(cwd: Option<&std::path::Path>) -> Result<Utf8PathBuf, String> {
    match cwd {
        Some(path) => Utf8PathBuf::from_path_buf(path.to_path_buf())
            .map_err(|p| format!("无效路径: {}", p.display())),
        None => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(|e| e.to_string())?)
            .map_err(|p| format!("无效路径: {}", p.display())),
    }
}

fn run(cli: Cli) -> Result<(), String> {
    // auth 子命令不需要完整配置/日志。
    if let Some(Command::Auth { command }) = &cli.command {
        return match command {
            AuthCommand::Set { provider } => {
                print!("输入 token（粘贴后回车，输入不回显）: ");
                std::io::stdout().flush().map_err(|e| e.to_string())?;
                // P1-14：隐藏回显（此前 token 明文显示在终端）。
                let mut token = String::new();
                let read = with_input_echo(false, || std::io::stdin().read_line(&mut token));
                read.map_err(|e| format!("读取输入失败: {e}"))?;
                let token = token.trim();
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
            AuthCommand::Status { provider } => match tpi::auth::auth_get(provider) {
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

    // P2：`tpi prune`——清理过期 session/artifact（~/.tpi/sessions 与 artifacts）。
    if let Some(Command::Prune {
        older_than,
        dry_run,
    }) = &cli.command
    {
        return prune_old_data(*older_than, *dry_run);
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

    let session_target = if cli.continue_session {
        SessionTarget::Continue
    } else if let Some(id) = cli.resume {
        SessionTarget::Resume(id)
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

    /// P0-6 回归：设计文档/README 的 `--continue` 必须可解析。
    /// （此前 `#[arg(long)] continue_session` 只生成 `--continue-session`。）
    #[test]
    fn continue_flag_matches_design_doc() {
        let cli = Cli::parse_from(["tpi", "--continue"]);
        assert!(cli.continue_session);
    }

    /// P0-7 回归：`tpi auth set <provider>` 子命令形态（文档 §18.3/README）。
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
}
