//! Session 恢复（文档 §4.3/§14.2/§10.7）。
//!
//! 恢复规则：
//! - 最后一行因崩溃不完整 → 只丢弃残行（见 [`super::read_events`]）。
//! - 每个已请求但缺少 `ToolCompleted` 的 call 追加合成 `ToolCompleted(status=Interrupted)`；
//!   model payload 明确写 `effect`。
//! - **绝不自动重跑**可能产生写入的工具；模型必须先重新读取相关文件或状态（§4.3）。

use std::path::Path;

use crate::session::{RecoveryMetadata, SessionEvent, read_events};
use crate::tool::edit::Effect;
use crate::tool::outcome::{ModelPayload, StoredToolOutcome, ToolMetadata, ToolStatus};

/// 恢复结果。
pub struct RecoveryOutcome {
    /// 原始完整事件（不含合成的 Interrupted）。
    pub events: Vec<SessionEvent>,
    /// 合成的 Interrupted 结果（按 call 顺序；provider_id, outcome）。
    pub interrupted: Vec<(String, StoredToolOutcome)>,
}

/// 已请求但未完成的 call（恢复期间的跟踪条目）。
struct PendingCall {
    call_id: String,
    tool_name: String,
    provider_id: String,
    recovery: Option<RecoveryMetadata>,
}

/// 恢复一个 session 文件。
///
/// `interrupted` 中的 outcome 会把 `effect_*` 作为原 tool call 的结果发送给模型
/// （§4.3）；调用方不得执行这些工具。
pub fn recover(path: &Path) -> std::io::Result<RecoveryOutcome> {
    let events = read_events(path)?;
    let mut pending: Vec<PendingCall> = Vec::new();

    for event in &events {
        match event {
            SessionEvent::ToolRequested { call } => {
                pending.push(PendingCall {
                    call_id: call.call_id.to_string(),
                    tool_name: call.name.clone(),
                    provider_id: call.provider_id.clone(),
                    recovery: None,
                });
            }
            SessionEvent::ToolStarted { call_id, recovery } => {
                if let Some(entry) = pending
                    .iter_mut()
                    .find(|entry| entry.call_id == call_id.to_string())
                {
                    entry.recovery = recovery.clone();
                }
            }
            SessionEvent::ToolCompleted { call_id, .. } => {
                pending.retain(|entry| entry.call_id != call_id.to_string());
            }
            _ => {}
        }
    }

    let interrupted = pending
        .into_iter()
        .map(|entry| {
            let effect = classify_effect(&entry.tool_name, entry.recovery.as_ref());
            let outcome = interrupted_outcome(&entry.tool_name, &entry.provider_id, effect);
            (entry.provider_id, outcome)
        })
        .collect();

    Ok(RecoveryOutcome {
        events,
        interrupted,
    })
}

/// 写工具缺 ToolCompleted 时的 effect 判定（§10.7 第 6 条）。
///
/// 崩溃后根据已持久化的 recovery metadata 检查 target/temp/backup 三者：
/// - target == temp digest → 已提交（committed）；
/// - target == expected → 未提交（not_applied）；
/// - 其他 → unknown。
///
/// 纯读工具无副作用，记为 not_applied。
fn classify_effect(tool_name: &str, recovery: Option<&RecoveryMetadata>) -> Effect {
    match tool_name {
        "read" | "list" | "search" | "web_search" | "web_fetch" | "update_plan" => {
            Effect::NotApplied
        }
        _ => {
            let Some(metadata) = recovery else {
                return Effect::Unknown;
            };
            if metadata.temp_path.is_empty() {
                // run/bash：无法从文件状态判定（进程可能已执行）。
                return Effect::Unknown;
            }
            let target_digest = std::path::Path::new(&metadata.target_path)
                .exists()
                .then(|| std::fs::read(&metadata.target_path).ok())
                .flatten()
                .map(|raw| crate::tool::edit::revision_of(&raw));
            let temp_digest = std::path::Path::new(&metadata.temp_path)
                .exists()
                .then(|| std::fs::read(&metadata.temp_path).ok())
                .flatten()
                .map(|raw| crate::tool::edit::revision_of(&raw));
            let backup_digest = metadata
                .backup_path
                .as_deref()
                .filter(|p| std::path::Path::new(p).exists())
                .and_then(|p| std::fs::read(p).ok())
                .map(|raw| crate::tool::edit::revision_of(&raw));
            let expected = if metadata.expected_revision.is_empty() {
                None
            } else {
                Some(metadata.expected_revision.as_str())
            };
            match (target_digest, temp_digest, backup_digest, expected) {
                // temp 仍在且 target == temp：已提交（commit 后 temp 未清理即崩溃）。
                (Some(target), Some(temp), _, _) if target == temp => Effect::Committed,
                // target 仍是 expected：未提交（replace 前崩溃，或失败后已恢复）。
                (Some(target), _, _, Some(expected)) if target == expected => Effect::NotApplied,
                // temp 已清理但 backup 是 expected 且 target 已变化：已提交（ToolCompleted 前崩溃）。
                (Some(_), None, Some(backup), Some(expected)) if backup == expected => {
                    Effect::Committed
                }
                (Some(_), None, None, Some(_expected)) if !metadata.temp_path.is_empty() => {
                    Effect::Unknown
                }
                _ => Effect::Unknown,
            }
        }
    }
}

/// 合成 Interrupted outcome（§4.3：model payload 明确写 effect）。
pub fn interrupted_outcome(
    tool_name: &str,
    provider_id: &str,
    effect: Effect,
) -> StoredToolOutcome {
    StoredToolOutcome {
        status: ToolStatus::Interrupted,
        model_payload: ModelPayload {
            status: ToolStatus::Interrupted,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!(
                "status: interrupted\ntool: {tool_name}\ntool_call_id: {provider_id}\neffect: {effect}\nnote: session 中断，未自动重跑；如需继续请重新读取相关文件。"
            ),
            effect: Some(effect),
            artifact: None,
        },
        session_metadata: ToolMetadata {
            tool: tool_name.to_string(),
            ..Default::default()
        },
    }
}
