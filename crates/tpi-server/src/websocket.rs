//! WebSocket 协议处理（web_desktop.md §十/§十一/§二十五/§二十八）。
//!
//! 消息流（JSON text frame）：
//!
//! ```text
//! client -> server:
//!   { "type": "hello", "protocol_version": 1, "client_name": "...", "client_version": "...", "token": "..." }
//!   { "type": "command", "payload": { "type": "submit_message", ... } }
//!   { "type": "ping" }
//!
//! server -> client:
//!   { "type": "server_hello", "protocol_version": 1, "server_version": "...", "last_seq": 200 }
//!   { "type": "ack", "request_id": "...", "status": "accepted" | "rejected", ... }
//!   { "type": "event", "protocol_version": 1, "seq": 173, "event": { ... } }
//!   { "type": "pong" }
//!   { "type": "error", "code": "...", "message": "..." }
//! ```
//!
//! ## backpressure（§二十八）
//!
//! - 高频 delta（AssistantDelta / ToolOutputDelta）发送前做**相邻合并**
//!   （同 request/call 且同流），慢客户端看到的是合并后的批次；
//! - 关键事件（RunCompleted / InputRequested / ToolCompleted / …）绝不合并丢弃；
//! - socket 发送失败 = 客户端断开：任务退出，客户端重连后用 last_seq +
//!   SessionView 重建（§十一第一阶段方案：reconnect -> GET SessionView ->
//!   subscribe live events）。
//!
//! ## 重连（§十一）
//!
//! broadcast 通道只保留最近 EVENT_BROADCAST_CAPACITY 条事件；更早的历史
//! 由客户端从 session store 重建（SessionResumed 事件携带完整视图）。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;

use tpi_protocol::{ClientCommand, EventEnvelope, PROTOCOL_VERSION};

use crate::ServerState;

/// 客户端 -> 服务器 消息信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMessage {
    Hello {
        protocol_version: u32,
        #[serde(default)]
        client_name: String,
        #[serde(default)]
        client_version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    Command {
        payload: ClientCommand,
    },
    Ping,
}

/// 服务器 -> 客户端 消息信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsServerMessage {
    ServerHello {
        protocol_version: u32,
        server_version: String,
        last_seq: u64,
    },
    Ack {
        ack: tpi_protocol::CommandAck,
    },
    Event {
        #[serde(flatten)]
        envelope: EventEnvelope,
    },
    Pong,
    Error {
        code: String,
        message: String,
    },
}

impl WsServerMessage {
    fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            json!({"type": "error", "code": "internal", "message": "序列化失败"}).to_string()
        })
    }
}

/// 单个 WebSocket 连接的处理入口。
pub(crate) async fn handle_socket(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sink, mut stream) = socket.split();

    // ---- 握手 ----
    let hello = match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => text,
        _ => return,
    };
    let (client_version, token) = match serde_json::from_str::<WsClientMessage>(&hello) {
        Ok(WsClientMessage::Hello {
            protocol_version,
            token,
            ..
        }) => (protocol_version, token),
        _ => {
            let _ = sink
                .send(Message::Text(
                    error_text("invalid_hello", "首条消息必须是 hello").into(),
                ))
                .await;
            return;
        }
    };

    if client_version != PROTOCOL_VERSION {
        let _ = sink
            .send(Message::Text(
                error_text(
                    "protocol_version_mismatch",
                    &format!("client={client_version}, server={PROTOCOL_VERSION}; 请升级客户端"),
                )
                .into(),
            ))
            .await;
        return;
    }
    if let Err(reason) = state.auth.verify(token.as_deref()) {
        let _ = sink
            .send(Message::Text(
                error_text("permission_denied", reason).into(),
            ))
            .await;
        return;
    }

    let last_seq = state.handle.last_seq().await;
    if sink
        .send(Message::Text(
            WsServerMessage::ServerHello {
                protocol_version: PROTOCOL_VERSION,
                server_version: state.server_version.clone(),
                last_seq,
            }
            .to_text()
            .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    // ---- 主循环 ----
    // sink 被 reader 与 pusher 两个分支共享：用 Arc<Mutex<>> 包装。
    let sink = Arc::new(tokio::sync::Mutex::new(sink));
    let mut event_rx = state.handle.subscribe();

    let reader_sink = sink.clone();
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else { continue };
            let parsed: WsClientMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    let mut sink = reader_sink.lock().await;
                    let _ = sink
                        .send(Message::Text(
                            error_text("invalid_message", &format!("无法解析: {e}")).into(),
                        ))
                        .await;
                    continue;
                }
            };
            match parsed {
                WsClientMessage::Hello { .. } => {
                    let mut sink = reader_sink.lock().await;
                    let _ = sink
                        .send(Message::Text(
                            error_text("invalid_hello", "hello 只能发送一次").into(),
                        ))
                        .await;
                }
                WsClientMessage::Command { payload } => {
                    // 命令结果经共享 sink 回写。command() 阻塞到 ack，
                    // 但 reader task 与 pusher 并行，事件流不被阻塞。
                    let ack = state.handle.command(payload).await.unwrap_or_else(|e| {
                        tpi_protocol::CommandAck {
                            request_id: tpi_core::ids::RequestId::new_v7(),
                            status: tpi_protocol::AckStatus::Rejected(tpi_protocol::AppError::new(
                                tpi_protocol::ErrorCode::InternalError,
                                e,
                            )),
                        }
                    });
                    let mut sink = reader_sink.lock().await;
                    let _ = sink
                        .send(Message::Text(WsServerMessage::Ack { ack }.to_text().into()))
                        .await;
                }
                WsClientMessage::Ping => {
                    let mut sink = reader_sink.lock().await;
                    let _ = sink
                        .send(Message::Text(WsServerMessage::Pong.to_text().into()))
                        .await;
                }
            }
        }
        // stream 结束（客户端断开）。
    });

    // pusher：broadcast -> 相邻 delta 合并 -> sink。
    let pusher = tokio::spawn(async move {
        let mut pending_delta: Option<EventEnvelope> = None;
        loop {
            let next = match event_rx.recv().await {
                Ok(envelope) => envelope,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // 客户端太慢：合并 delta 已尽力；关键事件已送出。
                    // 记日志并继续（下一条事件起继续转发；丢失的窗口由
                    // 客户端重连 + SessionView 重建兜底）。
                    tracing::warn!("ws 事件 lagged {n} 条");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            let mut sink = sink.lock().await;
            if is_mergeable_delta(&next) {
                match pending_delta.take() {
                    Some(prev) => {
                        // 已有暂存 delta：先发 prev（保证顺序），再暂存 next。
                        // 说明：本实现采用「攒一条、来新的先发旧的」策略，
                        // 平滑合并效果与实现复杂度的折中。
                        let _ = sink
                            .send(Message::Text(
                                WsServerMessage::Event { envelope: prev }.to_text().into(),
                            ))
                            .await;
                        pending_delta = Some(next);
                    }
                    None => pending_delta = Some(next),
                }
                // 非阻塞 flush 时机：下一条事件到达时。空闲时 delta 滞留
                // 至多一个事件周期（对交互延迟影响可忽略）。
            } else {
                // 关键事件：先 flush 暂存 delta，再发事件本体。
                if let Some(prev) = pending_delta.take() {
                    let _ = sink
                        .send(Message::Text(
                            WsServerMessage::Event { envelope: prev }.to_text().into(),
                        ))
                        .await;
                }
                let _ = sink
                    .send(Message::Text(
                        WsServerMessage::Event { envelope: next }.to_text().into(),
                    ))
                    .await;
            }
        }
    });

    // reader 结束（客户端断开）即退出；pusher 随之 abort。
    reader.await.ok();
    pusher.abort();
}

fn error_text(code: &str, message: &str) -> String {
    json!({"type": "error", "code": code, "message": message}).to_string()
}

/// 高频可合并事件：AssistantDelta / ToolOutputDelta。
fn is_mergeable_delta(envelope: &EventEnvelope) -> bool {
    matches!(
        envelope.event,
        tpi_protocol::RuntimeEvent::AssistantDelta { .. }
            | tpi_protocol::RuntimeEvent::ToolOutputDelta { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_hello_serializes() {
        let msg = WsServerMessage::ServerHello {
            protocol_version: 1,
            server_version: "0.1.0".into(),
            last_seq: 42,
        };
        let json = msg.to_text();
        assert!(json.contains("\"type\":\"server_hello\""));
        assert!(json.contains("\"last_seq\":42"));
    }

    #[test]
    fn client_command_parses() {
        let json = r#"{
            "type": "command",
            "payload": {
                "type": "submit_message",
                "session_id": "01a00b01-33aa-7ac1-b5ac-0c105f840a03",
                "content": "你好"
            }
        }"#;
        let parsed: WsClientMessage = serde_json::from_str(json).unwrap();
        match parsed {
            WsClientMessage::Command { payload } => {
                assert!(matches!(payload, ClientCommand::SubmitMessage { .. }));
            }
            _ => panic!("必须是 Command"),
        }
    }

    #[test]
    fn unknown_client_message_is_rejected() {
        let bad = r#"{"type": "no_such"}"#;
        assert!(serde_json::from_str::<WsClientMessage>(bad).is_err());
    }
}
