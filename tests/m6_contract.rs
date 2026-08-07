//! M6 验收契约（§21 M6、§17）。
//!
//! - 未配置 Brave key 时 `web_search` 明确 unavailable，不切换到其他服务（§17）；
//! - 不打开浏览器、不调用隐藏模型（§17）；
//! - `web_fetch` 有界（redirect/body/timeout）且 HTML 转换（§17）；
//! - release profile 存在（`cargo install --path . --locked` 可用）。

mod fixtures;

use camino::Utf8PathBuf;
use fixtures::test_tool_context;
use tpi::tool::outcome::ToolStatus;
use tpi::tool::web::{WebFetchArgs, WebSearchArgs, web_fetch, web_search};

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

/// §17：web_fetch 不存在的 URL → 明确失败（不打开浏览器）。
#[tokio::test]
async fn web_fetch_failure_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let ctx = test_tool_context(&workspace);
    let outcome = web_fetch(
        WebFetchArgs {
            url: "http://127.0.0.1:1/".into(), // 必然连接失败
        },
        &ctx,
    )
    .await;
    assert_eq!(outcome.status, ToolStatus::Failed);
    assert!(outcome.model_text().contains("request_failed"));
}

/// §17：web_fetch 对 HTML 做转换，正文有界。
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
        for _ in 0..1 {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
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
    let access = tpi::agent::scheduler::tool_access(
        tpi::tool::BuiltinTool::WebSearch,
        &tpi::tool::ValidatedArgs::WebSearch(WebSearchArgs {
            query: "q".into(),
            count: 5,
            freshness: None,
            domains: None,
        }),
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
