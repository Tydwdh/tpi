//! HTTP 路由（web_desktop.md §十：HTTP 只做 health/version/静态资源）。
//!
//! 不做庞大 REST API--所有实时内容走 WebSocket。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::ServerState;
use crate::auth::AuthConfig;

/// 从请求提取 token：`X-TPI-Token` 头或 `?token=` 查询参数。
pub(crate) fn extract_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(value) = headers.get("x-tpi-token") {
        if let Ok(s) = value.to_str() {
            return Some(s.to_string());
        }
    }
    // 查询参数 `?token=...`（URL 参数里可能是 URL-encoded；此处做简单解码）。
    let query = query?;
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        let k = kv.next()?;
        if k == "token" {
            let v = kv.next().unwrap_or("");
            return Some(url_decode(v));
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 静态资源路径安全检查：percent-decode 后不允许 `..` / `.` / 空段。
///
/// `Uri::path()` 是 percent-encoded 的，`%2e%2e` 需解码后才能发现 `..`；
/// 正常 URL 解析器（如浏览器/reqwest）会提前规范化，但原始 TCP 客户端
/// 可能直接发编码路径——这里做防御性检查。
pub fn is_safe_static_path(path: &str) -> bool {
    !path.split('/').any(|seg| {
        let decoded = url_decode(seg);
        decoded == ".." || decoded == "." || seg.is_empty()
    })
}

/// CORS middleware：按 AuthConfig 附带跨域头（生产只允许显式配置 origin）。
async fn cors_middleware(
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    if let Some(origin) = &state.auth.allowed_origin {
        let headers = response.headers_mut();
        headers.insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            origin.parse().unwrap(),
        );
        headers.insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            "content-type, x-tpi-token".parse().unwrap(),
        );
        headers.insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, OPTIONS".parse().unwrap(),
        );
    }
    response
}

#[allow(dead_code)]
fn cors_headers(auth: &AuthConfig) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(origin) = &auth.allowed_origin {
        headers.insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            origin.parse().unwrap(),
        );
    }
    headers
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn version(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    Json(json!({
        "server_version": state.server_version,
        "protocol_version": tpi_protocol::PROTOCOL_VERSION,
    }))
}

async fn ws_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // 握手层：若有 token 则校验（浏览器无法加自定义头，走 ?token= 查询参数）；
    // 真正的强制校验在消息层 hello（websocket.rs），那里浏览器可带 token。
    if let Some(token) = extract_token(&headers, None) {
        if let Err(reason) = state.auth.verify(Some(&token)) {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": reason }))).into_response();
        }
    }
    ws.on_upgrade(move |socket| crate::websocket::handle_socket(socket, state))
}

/// 静态资源服务（SPA）：`apps/web/dist` 构建产物。
///
/// - `/` 与未知路径回退到 index.html（前端路由）；
/// - 已知静态文件按扩展名返回对应 content-type；
/// - 路径穿越防护：只允许 dist 目录内文件。
async fn static_handler(
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Response {
    let Some(dist) = &state.web_dist else {
        return (StatusCode::NOT_FOUND, "web dist 未配置（--web-dist）").into_response();
    };
    let path = request.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    // 路径穿越防护（防御性：正常客户端已规范化，原始客户端可能带编码 `..`）。
    if !is_safe_static_path(path) {
        return (StatusCode::BAD_REQUEST, "非法路径").into_response();
    }
    let mut file_path = dist.join(path);
    if !file_path.is_file() {
        // SPA 回退：非静态资源路径（前端路由）都服务 index.html。
        file_path = dist.join("index.html");
    }
    if !file_path.is_file() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match tokio::fs::read(&file_path).await {
        Ok(bytes) => {
            let content_type = content_type_for(&file_path);
            Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, content_type)
                .body(Body::from(bytes))
                .unwrap()
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn content_type_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// 构造 HTTP 路由。
pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/version", get(version))
        .route("/ws", get(ws_handler))
        // SPA 静态资源（放在 API 之后；API 优先匹配）。
        .fallback(static_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            cors_middleware,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_extraction_prefers_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tpi-token", "from-header".parse().unwrap());
        assert_eq!(
            extract_token(&headers, Some("token=from-query")),
            Some("from-header".to_string())
        );
    }

    #[test]
    fn token_extraction_falls_back_to_query() {
        let headers = HeaderMap::new();
        assert_eq!(
            extract_token(&headers, Some("a=1&token=abc")),
            Some("abc".to_string())
        );
        assert_eq!(extract_token(&headers, Some("a=1")), None);
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(url_decode("plain"), "plain");
    }

    #[test]
    fn safe_path_rejects_traversal() {
        assert!(!is_safe_static_path(".."));
        assert!(!is_safe_static_path("../Cargo.toml"));
        assert!(!is_safe_static_path("%2e%2e/Cargo.toml"));
        assert!(!is_safe_static_path("a/%2e/b"));
        assert!(!is_safe_static_path("a//b"));
        assert!(is_safe_static_path("index.html"));
        assert!(is_safe_static_path("assets/app.js"));
        assert!(is_safe_static_path("a/b/c.js"));
    }
}
