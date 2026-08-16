//! Embedded server（web_desktop.md §二十/§二十一）：Desktop 生命周期。
//!
//! Desktop 不写第二套 API：启动 embedded/local TPI server → WebSocket →
//! 同一 Application Protocol → Web 与 Desktop 使用完全相同的 TypeScript SDK。
//!
//! ```text
//! Tauri starts
//!    ↓
//! find free localhost port
//!    ↓
//! generate random auth token
//!    ↓
//! start embedded TPI server
//!    ↓
//! WebView connects: ws://127.0.0.1:<port>
//!    ↓
//! normal protocol
//! ```

use tpi_runtime::{RuntimeHandle, RuntimeTask};

use crate::auth::AuthConfig;

/// Embedded server 实例（持有关闭句柄）。
pub struct EmbeddedServer {
    pub url: String,
    pub token: String,
    pub shutdown: tokio_util::sync::CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

/// 找到一个空闲的 localhost 端口。
pub async fn find_free_port() -> u16 {
    // 绑定端口 0 让 OS 分配，随后释放。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// 启动 embedded server（阻塞至关闭）。
///
/// 由调用方提供已经组装好的 runtime 任务（provider/registry 注入）。
pub async fn spawn_embedded<P: tpi_agent::provider::Provider + 'static>(
    task: RuntimeTask<P>,
    web_dist: Option<std::path::PathBuf>,
) -> Result<EmbeddedServer, String> {
    let port = find_free_port().await;
    let auth = AuthConfig::local_random();
    let token = auth.token.clone().unwrap_or_else(|| "local".to_string());
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("invalid addr: {e}"))?;

    let (handle, _join) = RuntimeHandle::new(task);
    let shutdown = tokio_util::sync::CancellationToken::new();

    let server_shutdown = shutdown.clone();
    let server_handle = handle.clone();
    let server_join = tokio::spawn(async move {
        let _ = crate::serve(server_handle, addr, auth, web_dist, server_shutdown).await;
    });

    // 等 server 就绪。
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    Ok(EmbeddedServer {
        url: format!("http://{addr}"),
        token,
        shutdown,
        join: server_join,
    })
}

impl EmbeddedServer {
    /// 优雅关闭（bounded shutdown，web_desktop.md §二十一）。
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.join).await;
    }
}

/// 生成 Desktop 的 WebView URL（带 token 查询参数供前端自动读取）。
pub fn webview_url(server: &EmbeddedServer) -> String {
    format!("{}/?token={}", server.url, server.token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn free_port_is_reusable() {
        let p1 = find_free_port().await;
        let p2 = find_free_port().await;
        assert_ne!(p1, 0);
        assert_ne!(p2, 0);
    }
}
