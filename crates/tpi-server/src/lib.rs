//! TPI 网络适配层（web_desktop.md §九 / Phase 4）。
//!
//! Server 是 **Application API 的 Network Adapter**，不含任何 Agent domain
//! logic--所有业务经 `RuntimeHandle`（command -> ack / subscribe -> events）。
//!
//! ## 职责划分（§十）
//!
//! - **HTTP**：`GET /api/health` / `GET /api/version` / 静态资源（不做庞大 REST）。
//! - **WebSocket** `/ws`：
//!   - 握手（ClientHello/ServerHello + 协议版本检查）；
//!   - ClientCommand -> runtime -> CommandAck；
//!   - RuntimeEvent 流（带 seq 信封）持续推送；
//!   - 断线重连（`subscribe { after_seq }` 回放，页面刷新后运行状态恢复）。
//!
//! ## 安全（§二十二/§二十三）
//!
//! - 默认仅监听 `127.0.0.1`（显式 `--listen` 才暴露局域网）；
//! - 可选随机/固定 token：HTTP 与 WebSocket 均校验；
//! - CORS 默认只允许配置的 frontend origin（禁止生产 `*`）。

pub mod auth;
pub mod embedded;
pub mod http;
pub mod websocket;

use std::net::SocketAddr;
use std::sync::Arc;

use tpi_runtime::RuntimeHandle;

/// Server 共享状态。
pub struct ServerState {
    pub handle: RuntimeHandle,
    pub auth: auth::AuthConfig,
    pub server_version: String,
    /// SPA 静态资源目录（None = 不提供静态服务）。
    pub web_dist: Option<std::path::PathBuf>,
}

/// 启动 HTTP + WebSocket 服务器（阻塞至 shutdown 或监听失败）。
pub async fn serve(
    handle: RuntimeHandle,
    addr: SocketAddr,
    auth: auth::AuthConfig,
    web_dist: Option<std::path::PathBuf>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let state = Arc::new(ServerState {
        handle,
        auth,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        web_dist,
    });

    let app = http::router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    tracing::info!("tpi-server 监听 http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            tracing::info!("tpi-server 收到关闭信号");
        })
        .await
        .map_err(|e| format!("server 运行失败: {e}"))?;
    Ok(())
}
