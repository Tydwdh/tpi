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
        &[],
        "回复两个字：你好".into(),
        tx,
        CancellationToken::new(),
        true,
        false,
    )
    .await
    .expect("live run succeeds");

    assert_eq!(outcome.reason, CompletionReason::Stop);
    assert!(!outcome.assistant_text.is_empty());
    eprintln!("live model reply: {}", outcome.assistant_text);
}
