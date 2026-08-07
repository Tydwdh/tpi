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

fn run(cli: Cli) -> Result<(), String> {
    // auth 子命令不需要完整配置/日志。
    if let Some(Command::Auth { command }) = &cli.command {
        return match command {
            AuthCommand::Set { provider } => {
                print!("输入 token（粘贴后回车）: ");
                std::io::stdout().flush().map_err(|e| e.to_string())?;
                let mut token = String::new();
                std::io::stdin()
                    .read_line(&mut token)
                    .map_err(|e| format!("读取输入失败: {e}"))?;
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

    // 日志（§19.2）：tracing 写 ~/.tpi/logs/tpi.log，不污染 stdout。
    init_logging()?;

    let workspace_root: Utf8PathBuf = match cli.cwd {
        Some(path) => {
            Utf8PathBuf::from_path_buf(path).map_err(|p| format!("无效路径: {}", p.display()))?
        }
        None => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(|e| e.to_string())?)
            .map_err(|p| format!("无效路径: {}", p.display()))?,
    };
    let config = config::load(&workspace_root, cli.model.as_deref())?;

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
}
