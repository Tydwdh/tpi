//! Mutation Journal（§B1）：文件变更的持久化记录，undo 与崩溃恢复的数据源。
//!
//! - 每次 edit/write 成功提交后，capabilities 层把 before/after 快照 append
//!   到 `<artifacts_root>/<session_id>/journal.jsonl`（JSONL，一行一条）。
//! - [`load_journal`] 从该文件重建 journal（顺序 = 提交顺序）。
//! - [`undo_mutation`] 恢复指定 mutation 的 before 内容；[`undo_all`] 回滚
//!   整个 session 的文件变更（多文件序列整体 undo）。
//!
//! 与 recovery.rs 的分工：recovery 处理「工具调用中断」（未完成的写操作，
//! 用 temp/backup 判定）；journal 处理「已成功提交的编辑」的回滚（undo）。

use crate::protocol::MutationCommittedPayload;
use serde::{Deserialize, Serialize};

/// 一条已提交的 mutation（文件变更组）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalMutation {
    pub mutation_id: String,
    pub files: Vec<crate::protocol::MutationFile>,
}

/// journal 文件路径（`<artifacts_root>/<session_id>/journal.jsonl`）。
pub fn journal_path(artifacts_root: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    artifacts_root.join(session_id).join("journal.jsonl")
}

/// append 一条 mutation 到 journal 文件（JSONL）。
pub fn append_mutation(
    artifacts_root: &std::path::Path,
    session_id: &str,
    payload: &MutationCommittedPayload,
) -> std::io::Result<()> {
    let path = journal_path(artifacts_root, session_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = serde_json::to_string(payload)?;
    use std::io::Write;
    writeln!(file, "{line}")?;
    Ok(())
}

/// 从 journal 文件重建 mutation journal（提交顺序；文件不存在 = 空）。
pub fn load_journal(path: &std::path::Path) -> std::io::Result<Vec<JournalMutation>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut mutations = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(payload) = serde_json::from_str::<MutationCommittedPayload>(line) {
            mutations.push(JournalMutation {
                mutation_id: payload.mutation_id,
                files: payload.files,
            });
        }
        // 损坏行跳过（best-effort；undo 数据源不因单行损坏整体失败）。
    }
    Ok(mutations)
}

/// undo 单条 mutation：把每个文件恢复到 before_content。
/// 返回实际恢复的文件数；文件当前内容已与 before 一致时跳过（幂等）。
pub fn undo_mutation(
    mutations: &[JournalMutation],
    mutation_id: &str,
    workspace_root: &std::path::Path,
) -> std::io::Result<usize> {
    let Some(mutation) = mutations.iter().find(|m| m.mutation_id == mutation_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("mutation not found: {mutation_id}"),
        ));
    };
    let mut restored = 0usize;
    for file in &mutation.files {
        if restore_file(file, workspace_root)? {
            restored += 1;
        }
    }
    Ok(restored)
}

/// undo 全部 mutations：把每个文件恢复到**最早一次**变更前的内容。
/// 多文件序列整体回滚（§B1：agent 多次 edit 的隐式事务）。
pub fn undo_all(
    mutations: &[JournalMutation],
    workspace_root: &std::path::Path,
) -> std::io::Result<usize> {
    // 按文件聚合：取该文件所有 mutation 中**最早**的 before_content。
    let mut earliest: std::collections::HashMap<String, &crate::protocol::MutationFile> =
        std::collections::HashMap::new();
    for mutation in mutations {
        for file in &mutation.files {
            earliest.entry(file.path.clone()).or_insert(file);
        }
    }
    let mut restored = 0usize;
    for (path, file) in earliest {
        let target = resolve_target(&path, workspace_root);
        let before = &file.before_content;
        if std::fs::read(&target)
            .map(|cur| cur != *before)
            .unwrap_or(true)
        {
            write_atomic(&target, before)?;
            restored += 1;
        }
    }
    Ok(restored)
}

/// 恢复单个文件到 before 内容。幂等：当前内容已等于 before 时跳过。
fn restore_file(
    file: &crate::protocol::MutationFile,
    workspace_root: &std::path::Path,
) -> std::io::Result<bool> {
    let target = resolve_target(&file.path, workspace_root);
    let before = &file.before_content;
    match std::fs::read(&target) {
        Ok(current) if current == *before => Ok(false), // 已一致，跳过
        _ => {
            write_atomic(&target, before)?;
            Ok(true)
        }
    }
}

/// 目标路径解析：绝对路径直接用；相对路径拼到 workspace root。
fn resolve_target(path: &str, workspace_root: &std::path::Path) -> std::path::PathBuf {
    let candidate = std::path::PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    }
}

/// 原子写入：同目录 temp + rename（与 edit 的 temp+sync 语义一致）。
fn write_atomic(target: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    let dir = target.parent().unwrap_or(std::path::Path::new("."));
    let temp = dir.join(format!(
        ".tpi-journal-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::write(&temp, content)?;
    if let Ok(f) = std::fs::File::open(&temp) {
        let _ = f.sync_all();
    }
    std::fs::rename(&temp, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MutationFile;

    fn mutation(id: &str, path: &str, before: &[u8], after: &[u8]) -> JournalMutation {
        JournalMutation {
            mutation_id: id.into(),
            files: vec![MutationFile {
                path: path.into(),
                before_revision: "b3:b".into(),
                after_revision: "b3:a".into(),
                before_content: before.to_vec(),
                after_content: after.to_vec(),
            }],
        }
    }

    #[test]
    fn append_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let session = "s1";
        let payload = crate::protocol::MutationCommittedPayload {
            mutation_id: "m1".into(),
            files: vec![MutationFile {
                path: "/tmp/x.rs".into(),
                before_revision: "b3:b".into(),
                after_revision: "b3:a".into(),
                before_content: b"old".to_vec(),
                after_content: b"new".to_vec(),
            }],
        };
        append_mutation(dir.path(), session, &payload).unwrap();
        let loaded = load_journal(&journal_path(dir.path(), session)).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].mutation_id, "m1");
        assert_eq!(loaded[0].files[0].before_content, b"old");
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_journal(&journal_path(dir.path(), "nope")).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn undo_mutation_restores_before_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "new\n").unwrap();
        let m = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        let restored = undo_mutation(&[m], "m1", dir.path()).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    }

    #[test]
    fn undo_mutation_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "old\n").unwrap();
        let m = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        // 当前内容已等于 before → 跳过（不重复写）。
        let restored = undo_mutation(&[m], "m1", dir.path()).unwrap();
        assert_eq!(restored, 0);
    }

    #[test]
    fn undo_all_restores_earliest_before() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "v2\n").unwrap();
        // 两次 mutation：v0→v1→v2。undo_all 应恢复到最早的 v0。
        let m1 = mutation("m1", &target.to_string_lossy(), b"v0\n", b"v1\n");
        let m2 = mutation("m2", &target.to_string_lossy(), b"v1\n", b"v2\n");
        let restored = undo_all(&[m1, m2], dir.path()).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v0\n");
    }

    #[test]
    fn undo_unknown_mutation_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = undo_mutation(&[], "nope", dir.path());
        assert!(result.is_err());
    }
}
