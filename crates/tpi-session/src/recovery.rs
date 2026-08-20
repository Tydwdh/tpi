//! Session 与未完成文件提交的恢复。
//!
//! 恢复规则：
//! - 最后一行因崩溃不完整 → 只丢弃残行（见 [`super::read_events`]）。
//! - 每个已请求但缺少 `ToolCompleted` 的 call 追加合成 `ToolCompleted(status=Interrupted)`；
//!   model payload 明确写 `effect`。
//! - **绝不自动重跑**可能产生写入的工具；模型必须先重新读取相关文件或状态（§4.3）。

use std::path::Path;

use std::io::Read;

use crate::{RecoveryMetadata, SessionEvent, read_events};
use tpi_core::outcome::Effect;
use tpi_core::outcome::{ModelPayload, StoredToolOutcome, ToolMetadata, ToolStatus};
use tpi_core::outcome::{ToolRecoveryPolicy, tool_recovery_policy};

/// 恢复结果。
pub struct RecoveryOutcome {
    /// 原始完整事件（不含合成的 Interrupted）。
    pub events: Vec<SessionEvent>,
    /// 合成的 Interrupted 结果（按 call 顺序；provider_id, outcome）。
    /// 已请求但未完成的 call（恢复期间合成 Tool 结果）。
    /// 每项 = (call_id, provider_id, outcome)。
    pub interrupted: Vec<(String, String, StoredToolOutcome)>,
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
    recover_from_events(&events)
}

/// 已持有事件时的恢复（避免 TOCTOU：调用方已持锁/已快照 events，无需二次读文件）。
pub fn recover_from_events(events: &[SessionEvent]) -> std::io::Result<RecoveryOutcome> {
    let mut pending: Vec<PendingCall> = Vec::new();

    for event in events {
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
            // §PointerHit：保留 call_id 供 resume 持久化合成结果（避免重复合成）。
            (entry.call_id, entry.provider_id, outcome)
        })
        .collect();

    Ok(RecoveryOutcome {
        events: events.to_vec(),
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
/// `pub(crate)`：session repair 在重建 interrupted tool outcome 时复用。
pub fn classify_effect(tool_name: &str, recovery: Option<&RecoveryMetadata>) -> Effect {
    let policy = tool_recovery_policy(tool_name);
    match policy {
        ToolRecoveryPolicy::NoEffect => Effect::NotApplied,
        ToolRecoveryPolicy::Unknown => Effect::Unknown,
        ToolRecoveryPolicy::FileCommit => {
            let Some(metadata) = recovery else {
                return Effect::Unknown;
            };
            let Ok(target_digest) = revision_of_path(std::path::Path::new(&metadata.target_path))
            else {
                return Effect::Unknown;
            };
            let Ok(temp_digest) = revision_of_path(std::path::Path::new(&metadata.temp_path))
            else {
                return Effect::Unknown;
            };
            let backup_digest = match metadata.backup_path.as_deref() {
                Some(path) => match revision_of_path(std::path::Path::new(path)) {
                    Ok(digest) => digest,
                    Err(_) => return Effect::Unknown,
                },
                None => None,
            };
            let expected = if metadata.expected_revision.is_empty() {
                None
            } else {
                Some(metadata.expected_revision.as_str())
            };
            match (
                target_digest,
                temp_digest,
                backup_digest,
                expected,
                metadata.candidate_revision.as_deref(),
            ) {
                // target 仍是调用前 revision：即使候选内容相同，也没有可观察副作用。
                (Some(target), _, _, Some(expected), _) if target == expected => Effect::NotApplied,
                // 新建文件且 target 仍不存在：尚未提交。
                (None, _, _, None, _) => Effect::NotApplied,
                // temp 仍在且 target == temp：已提交（commit 后 temp 未清理即崩溃）。
                (Some(target), Some(temp), _, _, _) if target == temp => Effect::Committed,
                // 新建成功后 temp 已移动且无 backup，用持久化的候选 revision 确认。
                (Some(target), None, _, None, Some(candidate)) if target == candidate => {
                    Effect::Committed
                }
                // temp 已清理但 backup 是 expected 且 target 已变化：已提交（ToolCompleted 前崩溃）。
                (Some(_), None, Some(backup), Some(expected), _) if backup == expected => {
                    Effect::Committed
                }
                (Some(_), None, None, Some(_expected), _) if !metadata.temp_path.is_empty() => {
                    Effect::Unknown
                }
                _ => Effect::Unknown,
            }
        }
    }
}

fn revision_of_path(path: &Path) -> std::io::Result<Option<String>> {
    const MAX_RECOVERY_HASH_BYTES: usize = 64 * 1024 * 1024;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_RECOVERY_HASH_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("恢复哈希：文件超过 {MAX_RECOVERY_HASH_BYTES} 字节上限"),
        ));
    }
    let mut hasher = blake3::Hasher::new();
    // 有界拷贝：即使 metadata 与实际长度存在 TOCTOU，也不会无界读。
    std::io::copy(
        &mut file.take((MAX_RECOVERY_HASH_BYTES as u64).saturating_add(1)),
        &mut hasher,
    )?;
    Ok(Some(format!(
        "{}{}",
        tpi_core::revision::REVISION_PREFIX,
        hasher.finalize().to_hex()
    )))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_file_recovery_uses_candidate_revision() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("created.txt");
        let temp = dir.path().join("missing.tmp");
        let content = b"new content";
        let metadata = RecoveryMetadata {
            tool: "write".into(),
            target_path: target.to_string_lossy().into_owned(),
            expected_revision: String::new(),
            candidate_revision: Some(tpi_core::revision::revision_of(content)),
            temp_path: temp.to_string_lossy().into_owned(),
            backup_path: None,
        };

        assert_eq!(
            classify_effect("write", Some(&metadata)),
            Effect::NotApplied
        );
        std::fs::write(&target, content).unwrap();
        assert_eq!(classify_effect("write", Some(&metadata)), Effect::Committed);
        std::fs::write(&target, b"different").unwrap();
        assert_eq!(classify_effect("write", Some(&metadata)), Effect::Unknown);
    }
}
