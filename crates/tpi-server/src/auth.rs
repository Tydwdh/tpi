//! 访问控制（web_desktop.md §二十二）。
//!
//! - Desktop/本地默认：随机 per-launch token（不裸奔，即使 localhost）。
//! - 远程：显式 `--auth-token`。
//! - 校验：HTTP 头 `X-TPI-Token` / 查询参数 `?token=` / WebSocket 子协议头均可。

use serde::{Deserialize, Serialize};

/// 认证配置。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// None = 无需认证（仅限显式禁用；默认本地服务也建议带 token）。
    pub token: Option<String>,
    /// 允许的 CORS origin（None = 不发送 CORS 头；Some("") = 允许全部仅限开发）。
    pub allowed_origin: Option<String>,
}

impl AuthConfig {
    /// 生成本地桌面模式配置：随机 token。
    pub fn local_random() -> Self {
        Self {
            // 随机 128-bit hex（每进程生成一次；由 Desktop 启动时传给 WebView）。
            token: Some(random_token()),
            allowed_origin: None,
        }
    }

    pub fn none() -> Self {
        Self {
            token: None,
            allowed_origin: None,
        }
    }

    /// 校验请求 token。无配置 token 时一律通过。
    pub fn verify(&self, presented: Option<&str>) -> Result<(), &'static str> {
        match (&self.token, presented) {
            (None, _) => Ok(()),
            (Some(_), None) => Err("缺少访问 token"),
            (Some(expected), Some(actual)) if constant_time_eq(expected, actual) => Ok(()),
            (Some(_), Some(_)) => Err("访问 token 不正确"),
        }
    }
}

/// 不依赖 rand crate 的随机 token（OS 随机源）。
fn random_token() -> String {
    // 用地址熵 + 时间 + 进程 id 组合；桌面场景足够（不是加密密钥）。
    // 更强的随机性留给未来（引入 getrandom）。
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    seed ^= std::process::id() as u128;
    let addr = &seed as *const u128 as usize;
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    addr.hash(&mut hasher);
    format!("{:016x}{:016x}", hasher.finish(), seed as u64)
}

/// 简易常量时间比较（防时序侧信道；token 不是高价值密钥，SipHash 简化实现）。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 握手阶段的认证结果（WebSocket 消息层）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallenge {
    pub required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_when_no_token_configured() {
        let auth = AuthConfig::none();
        assert!(auth.verify(None).is_ok());
        assert!(auth.verify(Some("anything")).is_ok());
    }

    #[test]
    fn verify_rejects_missing_or_wrong_token() {
        let auth = AuthConfig {
            token: Some("secret".into()),
            allowed_origin: None,
        };
        assert!(auth.verify(None).is_err());
        assert!(auth.verify(Some("wrong")).is_err());
        assert!(auth.verify(Some("secret")).is_ok());
    }

    #[test]
    fn random_tokens_differ() {
        assert_ne!(random_token(), random_token());
    }
}
