//! M6 验收契约（§21 M6、§17）。
//!
//! - `web_search` 使用免费 DuckDuckGo 端点（无需 API key，零配置；§17）；
//!   解析器契约在此 + src/tool/web.rs 单测覆盖（广告过滤、uddg 链接还原）；
//! - 不打开浏览器、不调用隐藏模型（§17）；
//! - `web_fetch` 有界（redirect/body/timeout）且 HTML 转换（§17）；
//! - release profile 已配置（`cargo install --path . --locked` 可用）。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_tool_context;
use tpi::tool::outcome::ToolStatus;
use tpi::tool::web::{
    WebFetchArgs, WebSearchArgs, parse_ddg_results, web_fetch, web_fetch_allowing_private_for_test,
};

/// §17：web_search 免费方案（无 key）——解析器对真实端点 fixture 的契约。
#[test]
fn web_search_is_keyless_ddg_parser() {
    // fixture 来自真实 html.duckduckgo.com 响应（1 广告 + 4 完整结果 + 1 截断残片）。
    let html = include_str!("fixtures/ddg_results.html");
    let hits = parse_ddg_results(html);
    assert_eq!(hits.len(), 4, "广告与不完整块必须被过滤（§17：不展示广告）");
    assert!(
        hits.iter().any(|h| h.url == "https://rust-lang.org/"),
        "必须包含 rust-lang.org（uddg 还原）: {:?}",
        hits.iter().map(|h| h.url.as_str()).collect::<Vec<_>>()
    );
    assert!(hits.iter().all(|h| h.url.starts_with("http")));
}

/// §17：web_search 对人机验证页返回明确错误而非乱码（解析层判定）。
#[test]
fn web_search_challenge_page_is_detected() {
    let challenge = "<div class=\"anomaly-modal\">Unfortunately, bots use DuckDuckGo too.</div>";
    assert!(tpi::tool::web::is_ddg_bot_challenge(challenge));
}

/// §17：真实端点 opt-in 冒烟（免费、无 key；默认忽略）。
///
/// 运行条件：`TPI_RUN_LIVE_TESTS=1`。DDG 对异常流量可能返回人机验证页，
/// 此时测试应报告失败而非假装成功。
#[tokio::test]
#[ignore = "live DDG search: set TPI_RUN_LIVE_TESTS=1"]
async fn live_ddg_search_returns_results() {
    if std::env::var("TPI_RUN_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skip: TPI_RUN_LIVE_TESTS != 1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);
    let outcome = tpi::tool::web::web_search(
        WebSearchArgs {
            query: "rust programming language".into(),
            count: 5,
            freshness: None,
            domains: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(
        outcome.status,
        ToolStatus::Succeeded,
        "{}",
        outcome.model_text()
    );
}

/// web_fetch 对私有地址默认执行 SSRF 拦截。
#[tokio::test]
async fn web_fetch_failure_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);
    let outcome = web_fetch(
        WebFetchArgs {
            url: "http://127.0.0.1:1/".into(),
            prompt: None,
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Failed);
    assert!(outcome.model_text().contains("ssrf_blocked"));
}

/// web_fetch 对 HTML 做转换，正文有界。
#[tokio::test]
async fn web_fetch_converts_html_and_bounds_body() {
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
    let outcome = web_fetch_allowing_private_for_test(
        WebFetchArgs {
            url: format!("http://{addr}/page"),
            prompt: None,
        },
        &ctx,
    )
    .await;
    handle.join().unwrap();
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
        true,
    );
    assert_eq!(access, tpi::agent::scheduler::ToolAccess::Pure);
}

/// §18.4：auth 凭据读写（keyring 后端在 CI/无凭据服务时可能不可用——跳过不硬失败）。
#[test]
fn auth_round_trip_when_keyring_available() {
    let provider = "tpi-test-provider";
    match tpi::auth::auth_set(provider, "test-token") {
        Ok(()) => {
            assert_eq!(
                tpi::auth::auth_get(provider).unwrap().as_deref(),
                Some("test-token")
            );
            tpi::auth::auth_clear(provider).unwrap();
            assert_eq!(tpi::auth::auth_get(provider).unwrap(), None);
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
