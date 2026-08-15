//! Domain message / content（P1-02 + P7 边界下沉）。
//!
//! 目标词汇（`02-target-architecture.md` §2.1）：domain model 表达用户/模型/
//! 工具交互的稳定语义，**不再把 OpenAI-compatible `ChatMessage` 当全系统消息
//! 模型**。本模块定义：
//!
//! - [`DomainRole`] / [`DomainContentBlock`] / [`DomainMessage`]：UI-agnostic、
//!   provider-agnostic 的语义消息；
//! - [`ChatMessage`] / [`ToolCall`]：provider wire 消息类型（P7 下沉：纯数据，
//!   定义在 domain 层；`crate::provider` re-export 保持对外契约）；
//! - 双向 adapter：`ChatMessage -> DomainMessage`（provider wire → domain）与
//!   `DomainMessage -> ChatMessage`（domain → provider wire）。
//!
//! 不变量：合法 `ChatMessage` 经 `ChatMessage -> DomainMessage -> ChatMessage`
//! 往返后语义等价（`tests/domain_message.rs` 验证）。provider-specific 字段
//! （如 base_url / finish reason）不进入 domain——domain 只表达 role + content。
//!
//! 生产路径（P1-02）：`session` 投影先输出 `DomainMessage`（
//! [`crate::session::replay_domain_messages`]），provider converter 再生成旧
//! `ChatMessage`。对外 `ChatMessage` 契约不变（golden parity 由测试保证）。

use crate::ids::ToolCallId;

/// 模型发出的工具调用请求（tool argument 增量已在 adapter 内拼接完成）。
/// （P7 下沉：定义在 domain 层；provider 层 re-export。）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// TPI 内部分配的 call id（§14.2 恢复关联用）。
    pub call_id: ToolCallId,
    /// provider 原始 tool call id（回填 tool result 时必须原样返回）。
    pub provider_id: String,
    pub name: String,
    /// 完整 JSON 参数字符串；schema 校验发生在调度前（§8.2 `PreparedToolCall`）。
    pub arguments: String,
}

/// 工具定义（schema 由参数类型生成，§5.2 schemars）。
/// （P7 下沉：定义在 domain 层；provider 层 re-export。）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// 发给模型的消息（OpenAI-compatible 最小形态；provider 差异在 adapter 内吸收）。
/// （P7 下沉：定义在 domain 层；provider 层 re-export。）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMessage {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    /// 工具结果回填。
    Tool {
        tool_call_id: String,
        name: String,
        content: String,
    },
}

/// 消息角色（domain 语义；不携带 provider 特定字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainRole {
    System,
    User,
    Assistant,
    Tool,
}

/// 消息内容块（domain 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainContentBlock {
    /// 文本内容。
    Text(String),
    /// 模型发起的工具调用。
    ToolCall(ToolCall),
    /// 工具执行结果（回填到对话）。
    ToolResult {
        tool_call_id: String,
        name: String,
        content: String,
    },
}

/// 一条语义消息（P1-02：与 `ChatMessage` 并存，经双向 adapter 转换）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainMessage {
    pub role: DomainRole,
    pub content: Vec<DomainContentBlock>,
}

impl DomainMessage {
    pub fn text(role: DomainRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![DomainContentBlock::Text(text.into())],
        }
    }
}

/// ChatMessage（provider wire）→ DomainMessage。
impl From<&ChatMessage> for DomainMessage {
    fn from(message: &ChatMessage) -> Self {
        match message {
            ChatMessage::System(text) => Self::text(DomainRole::System, text.clone()),
            ChatMessage::User(text) => Self::text(DomainRole::User, text.clone()),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = Vec::with_capacity(tool_calls.len() + 1);
                if !content.is_empty() {
                    blocks.push(DomainContentBlock::Text(content.clone()));
                }
                blocks.extend(tool_calls.iter().cloned().map(DomainContentBlock::ToolCall));
                Self {
                    role: DomainRole::Assistant,
                    content: blocks,
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                name,
                content,
            } => Self {
                role: DomainRole::Tool,
                content: vec![DomainContentBlock::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    content: content.clone(),
                }],
            },
        }
    }
}

/// DomainMessage → ChatMessage（provider wire）。
///
/// - System/User：取首个 Text block（缺失则空串——合法输入总是有 Text）；
/// - Assistant：Text blocks 拼接为 content，ToolCall blocks 收集为 tool_calls
///   （顺序 = block 顺序）；
/// - Tool：取首个 ToolResult block。
impl From<&DomainMessage> for ChatMessage {
    fn from(message: &DomainMessage) -> Self {
        match message.role {
            DomainRole::System => {
                ChatMessage::System(first_text(&message.content).unwrap_or_default())
            }
            DomainRole::User => ChatMessage::User(first_text(&message.content).unwrap_or_default()),
            DomainRole::Assistant => {
                let mut content = String::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        DomainContentBlock::Text(text) => content.push_str(text),
                        DomainContentBlock::ToolCall(call) => tool_calls.push(call.clone()),
                        DomainContentBlock::ToolResult { .. } => {
                            // Assistant 不应含 ToolResult（防御：跳过）。
                            debug_assert!(false, "Assistant domain message 含 ToolResult block");
                        }
                    }
                }
                ChatMessage::Assistant {
                    content,
                    tool_calls,
                }
            }
            DomainRole::Tool => {
                let result = message
                    .content
                    .iter()
                    .find_map(|block| match block {
                        DomainContentBlock::ToolResult {
                            tool_call_id,
                            name,
                            content,
                        } => Some((tool_call_id.clone(), name.clone(), content.clone())),
                        _ => None,
                    })
                    .unwrap_or_default();
                ChatMessage::Tool {
                    tool_call_id: result.0,
                    name: result.1,
                    content: result.2,
                }
            }
        }
    }
}

fn first_text(blocks: &[DomainContentBlock]) -> Option<String> {
    blocks.iter().find_map(|block| match block {
        DomainContentBlock::Text(text) => Some(text.clone()),
        _ => None,
    })
}
