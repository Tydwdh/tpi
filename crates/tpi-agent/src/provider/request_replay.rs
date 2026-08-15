//! P5-08：O3 request reconstruction——RequestHeader snapshot + shadow compare。
//!
//! - [`RequestHeader`]：frozen adapter request 的关键字段快照（dispatch 前冻结）；
//! - [`RequestManifest`]：committed prefix 重建的请求描述（session 侧）；
//! - [`compare`]：逐字段 shadow compare → [`Drift`] 列表；compare 失败在开发/CI
//!   fail loud（调用方决定是否硬失败）；
//! - provider secret/header/body typed scrub：manifest 不含正文与 credential
//!   （`ScrubPolicy`：drop secret → hash → truncate → allow 的保守默认）。
//!
//! 验收：recorded provider fixtures 可逐字段定位 header/message/tool schema drift。

use crate::provider::ModelRequest;

/// 请求 header 快照（dispatch 前冻结；不含正文与 secret）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHeader {
    pub model: String,
    /// 消息角色序列（user/assistant/tool...；正文 hash 不进 manifest）。
    pub role_sequence: Vec<String>,
    pub tool_schema_fingerprint: String,
    pub message_count: usize,
}

/// 从 committed prefix 重建的请求 manifest（session 侧描述）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestManifest {
    pub model: String,
    pub role_sequence: Vec<String>,
    pub tool_schema_fingerprint: String,
    pub message_count: usize,
}

/// 一处字段漂移（逐字段定位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub field: &'static str,
    pub expected: String,
    pub actual: String,
}

impl RequestHeader {
    /// 从实际 request 冻结 header（不复制正文/secret）。
    pub fn freeze(request: &ModelRequest, tool_schema_fingerprint: &str) -> Self {
        Self {
            model: request.model.clone(),
            role_sequence: request
                .messages
                .iter()
                .map(|m| match m {
                    crate::provider::ChatMessage::System(_) => "system".to_string(),
                    crate::provider::ChatMessage::User(_) => "user".to_string(),
                    crate::provider::ChatMessage::Assistant { .. } => "assistant".to_string(),
                    crate::provider::ChatMessage::Tool { .. } => "tool".to_string(),
                })
                .collect(),
            tool_schema_fingerprint: tool_schema_fingerprint.to_string(),
            message_count: request.messages.len(),
        }
    }
}

impl RequestManifest {
    /// 从 committed prefix 重建（模型/角色序列/工具指纹/消息数）。
    pub fn rebuild(
        model: String,
        role_sequence: Vec<String>,
        tool_schema_fingerprint: String,
        message_count: usize,
    ) -> Self {
        Self {
            model,
            role_sequence,
            tool_schema_fingerprint,
            message_count,
        }
    }
}

/// shadow compare：header（实际 frozen）vs manifest（committed 重建）。
/// 返回逐字段 drift；空 = 一致。
pub fn compare(manifest: &RequestManifest, header: &RequestHeader) -> Vec<Drift> {
    let mut drifts = Vec::new();
    if manifest.model != header.model {
        drifts.push(Drift {
            field: "model",
            expected: manifest.model.clone(),
            actual: header.model.clone(),
        });
    }
    if manifest.role_sequence != header.role_sequence {
        drifts.push(Drift {
            field: "role_sequence",
            expected: format!("{:?}", manifest.role_sequence),
            actual: format!("{:?}", header.role_sequence),
        });
    }
    if manifest.tool_schema_fingerprint != header.tool_schema_fingerprint {
        drifts.push(Drift {
            field: "tool_schema_fingerprint",
            expected: manifest.tool_schema_fingerprint.clone(),
            actual: header.tool_schema_fingerprint.clone(),
        });
    }
    if manifest.message_count != header.message_count {
        drifts.push(Drift {
            field: "message_count",
            expected: manifest.message_count.to_string(),
            actual: header.message_count.to_string(),
        });
    }
    drifts
}

/// scrub 保守默认：manifest/header 不含正文（只含角色序列）与 credential。
/// （正文 hash/tokenize 与 secret canary 在 P7-07 diagnostic bundle 完整化。）
pub const SCRUB_POLICY: &str = "drop-body;drop-secret;keep-role-sequence";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ChatMessage;

    fn fingerprint(tools: &[&str]) -> String {
        tools.join("|")
    }

    /// 一致：无 drift。
    #[test]
    fn consistent_request_has_no_drift() {
        let request = ModelRequest {
            model: "gpt-4o".into(),
            messages: vec![
                ChatMessage::User("hi".into()),
                ChatMessage::Assistant {
                    content: "hello".into(),
                    tool_calls: vec![],
                },
            ],
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: None,
            context_window: None,
        };
        let header = RequestHeader::freeze(&request, &fingerprint(&["read"]));
        let manifest = RequestManifest::rebuild(
            "gpt-4o".into(),
            vec!["user".into(), "assistant".into()],
            fingerprint(&["read"]),
            2,
        );
        assert!(compare(&manifest, &header).is_empty(), "一致请求无 drift");
    }

    /// 逐字段定位 drift：model / tool schema / message_count。
    #[test]
    fn drifts_are_field_localizable() {
        let request = ModelRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::User("hi".into())],
            tools: Vec::new(),
            max_output_tokens: None,
            reasoning: None,
            context_window: None,
        };
        let header = RequestHeader::freeze(&request, &fingerprint(&["read", "bash"]));
        let manifest = RequestManifest::rebuild(
            "gpt-4o-mini".into(),
            vec!["user".into()],
            fingerprint(&["read"]),
            1,
        );
        let drifts = compare(&manifest, &header);
        let fields: Vec<&str> = drifts.iter().map(|d| d.field).collect();
        assert!(fields.contains(&"model"), "model drift 定位: {fields:?}");
        assert!(
            fields.contains(&"tool_schema_fingerprint"),
            "tool schema drift 定位: {fields:?}"
        );
        // 角色序列一致（无正文内容）。
        assert!(!fields.contains(&"role_sequence"));
        // scrub：header 不含正文（只有角色序列）。
        assert!(
            !header.role_sequence.iter().any(|r| r.contains("hi")),
            "无正文泄露"
        );
    }
}
