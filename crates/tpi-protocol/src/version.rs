//! 协议版本与握手（web_desktop.md §二十五）。
//!
//! 从第一版就带 `protocol_version`；不兼容时返回 `ProtocolVersionMismatch`，
//! 而不是等协议变复杂后再补版本控制。

/// 当前协议版本。不兼容变更时递增。
pub const PROTOCOL_VERSION: u32 = 1;

/// 客户端 → 服务器 握手（连接建立后第一条消息）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_version: String,
}

/// 服务器 → 客户端 握手响应。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerHello {
    pub protocol_version: u32,
    pub server_version: String,
}

/// 协议版本常量容器（便于前端直接引用数值；Rust 侧用 `PROTOCOL_VERSION`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersion;

impl ProtocolVersion {
    pub const fn current() -> u32 {
        PROTOCOL_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let hello = ClientHello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "tpi-web".into(),
            client_version: "0.1.0".into(),
        };
        let json = serde_json::to_string(&hello).unwrap();
        let back: ClientHello = serde_json::from_str(&json).unwrap();
        assert_eq!(hello, back);
        // 前端兼容：字段名必须是稳定 wire 名。
        assert!(json.contains("\"client_name\""));
    }

    #[test]
    fn version_is_one_for_now() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
