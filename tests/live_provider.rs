//! 真实 provider smoke test（§20.1：必须显式设置凭据和 `TPI_RUN_LIVE_TESTS=1` 才运行）。
//!
//! 默认 `cargo test` 只使用 fake/recording，不能在 CI 或普通验证中产生模型费用。

mod fixtures;

use camino::Utf8PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tpi::agent;
use tpi::ids::RunId;
use tpi::provider::openai_compat::OpenAiCompatClient;
use tpi::session::{CompletionReason, SessionLog};

/// 真实 provider 最小 smoke：一次请求、一个回答。
///
/// 运行条件（§20.1）：
/// - 环境变量 `TPI_RUN_LIVE_TESTS=1`
/// - `TPI_API_KEY` 或配置的 `api_key_env`
/// - 配置 `~/.tpi/config.toml` 的 `[model.primary]`（provider/name/base_url）
#[tokio::test]
#[ignore = "live provider smoke test: set TPI_RUN_LIVE_TESTS=1 and credentials"]
async fn live_provider_smoke_opt_in() {
    if std::env::var("TPI_RUN_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skip: TPI_RUN_LIVE_TESTS != 1");
        return;
    }
    let workspace = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
    // 从真实配置加载模型与凭据。
    let config = tpi::config::load(&workspace, None)
        .expect("~/.tpi/config.toml must configure [model.primary]");
    let api_key =
        tpi::config::read_api_key(&config).expect("TPI_API_KEY or configured api_key_env");

    let mut provider = OpenAiCompatClient::new(
        config.model.base_url.clone(),
        config.model.name.clone(),
        api_key,
        config.model.reasoning.clone(),
        config.model.max_output_tokens,
        config.model.context_window,
    );
    let sessions_root = config.sessions_root.clone();
    let mut session = SessionLog::create(&sessions_root, workspace.as_std_path(), RunId::new_v7())
        .expect("session");
    let (tx, _rx) = mpsc::channel(128);

    let outcome = agent::run(
        &mut provider,
        &mut session,
        &config,
        agent::RunInput {
            history: &[],
            user_message: "回复两个字：你好".into(),
            ui: tx,
            cancel: CancellationToken::new(),
            interactive: true,
            force_compaction: false,
            workspace: None,
            registry: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::tool::registry::builtin_registry(),
            )),
            processes: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::process::managed::ProcessRegistry::new(),
            )),
            terminals: std::sync::Arc::new(std::sync::Mutex::new(
                tpi::terminal::TerminalRegistry::default(),
            )),

            agents: std::sync::Arc::new(std::sync::Mutex::new(

                tpi_agent::agent::manager::AgentManager::new(),

            )),
        },
    )
    .await
    .expect("live run succeeds");

    assert_eq!(outcome.reason, CompletionReason::Stop);
    assert!(!outcome.assistant_text.is_empty());
    eprintln!("live model reply: {}", outcome.assistant_text);
}

/// Live Canary 2（任务书 P0-5）：真实 provider 完成 read tool call 闭环。
///
/// provider request #1 → tool call: read → tool result → provider request #2
/// → final text 含标记。记录 request count / tool name / finish reason。
///
/// 运行条件同 [`live_provider_smoke_opt_in`]。
#[tokio::test]
#[ignore = "live canary 2: set TPI_RUN_LIVE_TESTS=1 and credentials"]
async fn live_canary_2_real_tool_call_loop() {
    if std::env::var("TPI_RUN_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skip: TPI_RUN_LIVE_TESTS != 1");
        return;
    }
    // 临时 workspace + probe 文件；session 也放临时目录（不污染真实数据）。
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    std::fs::write(workspace.join("probe.txt"), "TPI_LIVE_CANARY_7F31").unwrap();

    let mut config = tpi::config::load(&workspace, None)
        .expect("~/.tpi/config.toml must configure [model.primary]");
    // workspace 在 temp：config::load 应已解析到 temp 目录；sessions 放 temp。
    config.sessions_root = workspace.join(".live-sessions").into();
    config.artifacts_root = workspace.join(".live-artifacts").into();
    let api_key =
        tpi::config::read_api_key(&config).expect("TPI_API_KEY or configured api_key_env");

    let mut provider = OpenAiCompatClient::new(
        config.model.base_url.clone(),
        config.model.name.clone(),
        api_key,
        config.model.reasoning.clone(),
        config.model.max_output_tokens,
        config.model.context_window,
    );
    let mut session = SessionLog::create(
        &config.sessions_root,
        workspace.as_std_path(),
        RunId::new_v7(),
    )
    .expect("session");
    let (tx, _rx) = mpsc::channel(128);

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        agent::run(
            &mut provider,
            &mut session,
            &config,
            agent::RunInput {
                history: &[],
                user_message: "请使用 read 工具读取 probe.txt 文件，然后只回复文件中的标记内容。"
                    .into(),
                ui: tx,
                cancel: CancellationToken::new(),
                interactive: true,
                force_compaction: false,
                workspace: None,
                registry: std::sync::Arc::new(std::sync::Mutex::new(
                    tpi::tool::registry::builtin_registry(),
                )),
                processes: std::sync::Arc::new(std::sync::Mutex::new(
                    tpi::process::managed::ProcessRegistry::new(),
                )),
                terminals: std::sync::Arc::new(std::sync::Mutex::new(
                    tpi::terminal::TerminalRegistry::default(),
                )),

                agents: std::sync::Arc::new(std::sync::Mutex::new(

                    tpi_agent::agent::manager::AgentManager::new(),

                )),
            },
        ),
    )
    .await
    .expect("live tool loop 必须在 180s 内完成")
    .expect("live run succeeds");

    assert_eq!(outcome.reason, CompletionReason::Stop);
    eprintln!("live canary 2 final: {}", outcome.assistant_text);
    assert!(
        outcome.assistant_text.contains("TPI_LIVE_CANARY_7F31"),
        "最终回答必须包含文件标记: {}",
        outcome.assistant_text
    );
    // 会话里必须有 read 工具的完整记录（ToolRequested + ToolCompleted）。
    let events = tpi::session::read_events(session.path()).unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            tpi::session::SessionEvent::ToolRequested { call } if call.name == "read"
        )),
        "session 必须记录 read tool call"
    );
    eprintln!(
        "live canary 2 ok: events={} text_len={}",
        events.len(),
        outcome.assistant_text.len()
    );
}
