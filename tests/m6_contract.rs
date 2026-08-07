//! M6 验收契约（§21 M6、§17）。
//!
//! - 未配置 Brave key 时 `web_search` 明确 unavailable，不切换到其他服务（§17）；
//! - 不打开浏览器、不调用隐藏模型（§17）；
//! - `web_fetch` 有界（redirect/body/timeout）且 HTML 转换（§17）；
//! - release profile 已配置（`cargo install --path . --locked` 可用）。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_tool_context;
use tpi::tool::outcome::ToolStatus;
use tpi::tool::web::{WebFetchArgs, WebSearchArgs, web_fetch, web_search};

/// 串行化依赖全局 allow_private 测试开关的两个 web_fetch 测试（并行互相覆盖会死锁）。
static WEB_FETCH_TESTS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// §17：未配置 Brave key → 明确 unavailable（不自动切换到其他服务）。
#[tokio::test]
async fn web_search_without_key_is_explicitly_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let mut ctx = test_tool_context(&workspace);
    // 指向一个必然不存在的环境变量。
    ctx.web_brave_key_env = "TPI_NO_SUCH_BRAVE_KEY".into();
    let outcome = web_search(
        WebSearchArgs {
            query: "rust".into(),
            count: 5,
            freshness: None,
            domains: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Failed);
    let text = outcome.model_text();
    assert!(text.contains("unavailable"));
    assert!(
        text.contains("不自动切换到其他服务"),
        "必须明确声明不切换到其他服务（§17）: {text}"
    );
}

/// §17：web_fetch 对私有地址默认 SSRF 拦截。
///
/// 与 web_fetch_converts_html_and_bounds_body 串行（二者共享全局
/// allow_private 测试开关，并行时互相覆盖会导致 accept 死锁）。
#[tokio::test]
async fn web_fetch_failure_is_explicit() {
    let _guard = WEB_FETCH_TESTS_LOCK.lock().await;
    tpi::tool::web::set_allow_private_web_targets_for_tests(false);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);
    let outcome = web_fetch(
        WebFetchArgs {
            url: "http://127.0.0.1:1/".into(),
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Failed);
    assert!(outcome.model_text().contains("ssrf_blocked"));
}

/// §17：web_fetch 对 HTML 做转换，正文有界。
#[tokio::test]
async fn web_fetch_converts_html_and_bounds_body() {
    let _guard = WEB_FETCH_TESTS_LOCK.lock().await;
    tpi::tool::web::set_allow_private_web_targets_for_tests(true);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);
    // 本地 HTTP 服务返回一个简单 HTML 页面。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = format!(
        "<html><head><title>Test Page</title><script>evil()</script></head><body><h1>Hello TPI</h1><p>{}</p></body></html>",
        "content ".repeat(50)
    );
    let handle = std::thread::spawn(move || {
        // 非阻塞轮询 + 超时：即使请求被 SSRF 拦截/失败也返回，避免 accept 永久阻塞。
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    use std::io::{Read, Write};
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });
    let outcome = web_fetch(
        WebFetchArgs {
            url: format!("http://{addr}/page"),
        },
        &ctx,
    )
    .await;
    handle.join().unwrap();
    tpi::tool::web::set_allow_private_web_targets_for_tests(false);
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    let text = outcome.model_text();
    assert!(text.contains("http: 200"), "{text}");
    assert!(
        text.contains("Hello TPI"),
        "正文必须转换（h1 可见）: {text}"
    );
    assert!(text.contains("content content"), "正文内容必须可见: {text}");
    // 有界正文（§8.4：48 KiB）。
    assert!(
        text.len() <= tpi::tool::web::FETCH_BODY_BUDGET + 512,
        "正文有界"
    );
}

/// §17：web_search/web_fetch 是 Pure（不打开浏览器、不调用隐藏模型——
/// 由实现保证：搜索只调 Brave Web Search endpoint，fetch 只做转换）。
#[test]
fn web_tools_are_pure_access() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let access = tpi::agent::scheduler::tool_access(
        tpi::tool::BuiltinTool::WebSearch,
        &tpi::tool::ValidatedArgs::WebSearch(WebSearchArgs {
            query: "q".into(),
            count: 5,
            freshness: None,
            domains: None,
        }),
        &workspace,
    );
    assert_eq!(access, tpi::agent::scheduler::ToolAccess::Pure);
}

/// §18.4：auth 凭据读写（keyring 后端在 CI/无凭据服务时可能不可用——跳过不硬失败）。
#[test]
fn auth_round_trip_when_keyring_available() {
    let provider = "tpi-test-provider";
    match tpi::auth::auth_set(provider, "test-token") {
        Ok(()) => {
            assert_eq!(tpi::auth::auth_get(provider).as_deref(), Some("test-token"));
            tpi::auth::auth_clear(provider).unwrap();
            assert_eq!(tpi::auth::auth_get(provider), None);
        }
        Err(error) => {
            // 无凭据服务（如 headless CI）时明确跳过并说明。
            eprintln!("keyring 不可用，跳过 auth round-trip: {error}");
        }
    }
}

/// release profile 已在 Cargo.toml 配置。
#[test]
fn release_profile_is_configured() {
    let manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    assert!(
        manifest.contains("[profile.release]"),
        "Cargo.toml 应包含 [profile.release]"
    );
}
