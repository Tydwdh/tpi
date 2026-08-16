//! 结构化错误（web_desktop.md §二十七）。
//!
//! UI 根据 `code` 做行为，不解析英文字符串。协议**绝不**发送
//! `{"error": "something failed"}` 这种无结构错误。

use serde::{Deserialize, Serialize};

/// 机器可读错误码。新增错误时在此扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 操作被取消（用户取消 / 运行中取消）。
    Cancelled,
    /// 超时（wall-time / 网络）。
    Timeout,
    /// 模型 provider 失败（连接/协议/内容过滤）。
    ProviderFailure,
    /// 重试预算耗尽。
    RetryBudgetExhausted,
    /// session 不存在或无法恢复。
    SessionNotFound,
    /// 命令非法（缺参 / 状态不允许 / 未知命令）。
    InvalidCommand,
    /// 该 `request_id` 的输入已被回答（重复回答被安全拒绝）。
    InputAlreadyAnswered,
    /// 后台进程不存在。
    ProcessNotFound,
    /// 权限不足（token 校验失败等）。
    PermissionDenied,
    /// edit 的 revision 过期（stale_revision）。
    StaleRevision,
    /// 协议版本不匹配。
    ProtocolVersionMismatch,
    /// 运行中状态不允许（如 run 进行中再次 submit 同 session）。
    Busy,
    /// 内部错误（Rust 侧 panic/IO 等不可预期错误）。
    InternalError,
}

/// 结构化错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    /// 是否可重试（UI 据此决定是否显示 Retry）。
    #[serde(default)]
    pub retryable: bool,
    /// 附加结构化细节（JSON value；无细节时为空对象）。
    #[serde(default = "default_details")]
    pub details: serde_json::Value,
}

fn default_details() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            details: serde_json::Value::Object(Default::default()),
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidCommand, message)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorCode::InternalError, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_serializes_structure() {
        let err = AppError::new(ErrorCode::StaleRevision, "revision 过期")
            .with_details(serde_json::json!({"current": "b3:abc"}));
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "stale_revision");
        assert_eq!(json["retryable"], false);
        assert_eq!(json["details"]["current"], "b3:abc");
        let back: AppError = serde_json::from_value(json).unwrap();
        assert_eq!(back.code, ErrorCode::StaleRevision);
    }

    #[test]
    fn unknown_error_code_is_rejected() {
        // 前端永远不该发未知 code；未知 code 反序列化失败 = 协议不兼容。
        let bad = r#"{"code":"no_such_code","message":"x","retryable":false,"details":{}}"#;
        assert!(serde_json::from_str::<AppError>(bad).is_err());
    }

    #[test]
    fn defaults_are_tolerant() {
        // 前端省略 retryable/details 时仍可解析（向后兼容字段新增）。
        let partial = r#"{"code":"session_not_found","message":"missing"}"#;
        let err: AppError = serde_json::from_str(partial).unwrap();
        assert_eq!(err.code, ErrorCode::SessionNotFound);
        assert!(!err.retryable);
        assert!(err.details.is_object());
    }
}
