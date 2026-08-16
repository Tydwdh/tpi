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

/// Journal 完整性状态（§B3）：损坏行导致 destructive undo 被拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalIntegrity {
    /// 全部行可解析，undo/redo 允许。
    Clean,
    /// 存在损坏行（无法解析），history 可看但 undo/redo 拒绝（除非 --force）。
    Tainted,
}

/// 加载结果：mutations + 完整性标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalState {
    pub mutations: Vec<JournalMutation>,
    pub integrity: JournalIntegrity,
    /// 损坏行数（integrity==Tainted 时 >0）。
    pub corrupt_lines: usize,
}

impl JournalState {
    pub fn is_tainted(&self) -> bool {
        self.integrity == JournalIntegrity::Tainted
    }
}

/// 从 journal 文件重建 mutation journal（提交顺序；文件不存在 = 空 + Clean）。
///
/// §B3：损坏行不再静默丢弃——计数并标记 [`JournalIntegrity::Tainted`]，
/// 调用方（undo/redo）据此拒绝 destructive 操作。
pub fn load_journal(path: &std::path::Path) -> std::io::Result<JournalState> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalState {
                mutations: Vec::new(),
                integrity: JournalIntegrity::Clean,
                corrupt_lines: 0,
            });
        }
        Err(e) => return Err(e),
    };
    let mut mutations = Vec::new();
    let mut corrupt_lines = 0usize;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(payload) = serde_json::from_str::<MutationCommittedPayload>(line) {
            mutations.push(JournalMutation {
                mutation_id: payload.mutation_id,
                files: payload.files,
            });
        } else {
            corrupt_lines += 1;
        }
    }
    let integrity = if corrupt_lines > 0 {
        JournalIntegrity::Tainted
    } else {
        JournalIntegrity::Clean
    };
    Ok(JournalState {
        mutations,
        integrity,
        corrupt_lines,
    })
}

/// 检查 journal 完整性：Tainted 时拒绝 destructive undo/redo。
/// `force` 为 true（用户 `--force`）时放行。
pub fn assert_can_mutate(state: &JournalState, force: bool) -> std::io::Result<()> {
    if !force && state.is_tainted() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "journal 已损坏（{} 行无法解析），拒绝 destructive 操作；\n  history 可查看，但 undo/redo 需修复 journal 或显式 --force",
                state.corrupt_lines
            ),
        ));
    }
    Ok(())
}

/// 单文件 undo/redo 的 CAS 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasVerdict {
    /// 应用成功（文件已写入目标内容）。
    Applied,
    /// 文件已是目标状态（幂等跳过）。
    AlreadyDone,
    /// 文件内容不是预期的 before/after（外部修改/分歧）——不写文件。
    Conflict,
}

/// undo 单条 mutation（CAS）：
///
/// ```text
/// current == mutation.after  -> 恢复 before（Applied）
/// current == mutation.before -> AlreadyDone（幂等）
/// otherwise                   -> Conflict，不写文件
/// ```
///
/// 返回逐文件判定；任一文件 Conflict 时**不写任何文件**（原子性：
/// 防部分应用——roadmap 示例禁止 `Agent A→B / User B→C / Undo -> C→A`）。
pub fn undo_mutation(
    mutations: &[JournalMutation],
    mutation_id: &str,
    workspace_root: &std::path::Path,
) -> std::io::Result<Vec<(String, CasVerdict)>> {
    let Some(mutation) = mutations.iter().find(|m| m.mutation_id == mutation_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("mutation not found: {mutation_id}"),
        ));
    };
    // 先全部判定（不写文件）：任一 Conflict → 整体拒绝。
    let mut verdicts = Vec::with_capacity(mutation.files.len());
    for file in &mutation.files {
        let target = resolve_target(&file.path, workspace_root);
        let current = std::fs::read(&target).unwrap_or_default();
        let verdict = if current == file.before_content {
            CasVerdict::AlreadyDone
        } else if current == file.after_content {
            CasVerdict::Applied // 待写入
        } else {
            CasVerdict::Conflict
        };
        verdicts.push((file.path.clone(), verdict));
    }
    if verdicts.iter().any(|(_, v)| *v == CasVerdict::Conflict) {
        return Ok(verdicts); // 调用方见 Conflict 不写文件
    }
    // 无冲突：应用（只写 Applied 的文件）。
    for (file, (_, verdict)) in mutation.files.iter().zip(&verdicts) {
        if *verdict == CasVerdict::Applied {
            let target = resolve_target(&file.path, workspace_root);
            write_atomic(&target, &file.before_content)?;
        }
    }
    Ok(verdicts)
}

/// redo 单条 mutation（CAS）：
///
/// ```text
/// current == mutation.before -> 恢复 after（Applied）
/// current == mutation.after  -> AlreadyRedone（幂等）
/// otherwise                   -> Conflict，不写文件
/// ```
pub fn redo_mutation(
    mutations: &[JournalMutation],
    mutation_id: &str,
    workspace_root: &std::path::Path,
) -> std::io::Result<Vec<(String, CasVerdict)>> {
    let Some(mutation) = mutations.iter().find(|m| m.mutation_id == mutation_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("mutation not found: {mutation_id}"),
        ));
    };
    let mut verdicts = Vec::with_capacity(mutation.files.len());
    for file in &mutation.files {
        let target = resolve_target(&file.path, workspace_root);
        let current = std::fs::read(&target).unwrap_or_default();
        let verdict = if current == file.after_content {
            CasVerdict::AlreadyDone
        } else if current == file.before_content {
            CasVerdict::Applied // 待写入
        } else {
            CasVerdict::Conflict
        };
        verdicts.push((file.path.clone(), verdict));
    }
    if verdicts.iter().any(|(_, v)| *v == CasVerdict::Conflict) {
        return Ok(verdicts);
    }
    for (file, (_, verdict)) in mutation.files.iter().zip(&verdicts) {
        if *verdict == CasVerdict::Applied {
            let target = resolve_target(&file.path, workspace_root);
            write_atomic(&target, &file.after_content)?;
        }
    }
    Ok(verdicts)
}

/// undo 最近一条 mutation（最后一个提交的编辑）。
/// journal 为空时返回 Err(NotFound)。
pub fn undo_last(
    mutations: &[JournalMutation],
    workspace_root: &std::path::Path,
) -> std::io::Result<Vec<(String, CasVerdict)>> {
    let Some(last) = mutations.last() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "journal is empty: no mutation to undo",
        ));
    };
    undo_mutation(mutations, &last.mutation_id, workspace_root)
}

/// redo 最近一条 mutation（最后一个提交的编辑的 after 内容）。
/// journal 为空时返回 Err(NotFound)。
pub fn redo_last(
    mutations: &[JournalMutation],
    workspace_root: &std::path::Path,
) -> std::io::Result<Vec<(String, CasVerdict)>> {
    let Some(last) = mutations.last() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "journal is empty: no mutation to redo",
        ));
    };
    redo_mutation(mutations, &last.mutation_id, workspace_root)
}

/// undo 全部 mutations：把每个文件恢复到**最早一次**变更前的内容。
/// 多文件序列整体回滚（agent 多次 edit 的隐式事务）。
///
/// CAS：文件当前内容必须是其**最后一次**变更的 after（或已是最早 before），
/// 否则 Conflict 且不写该文件。返回逐文件判定。
pub fn undo_all(
    mutations: &[JournalMutation],
    workspace_root: &std::path::Path,
) -> std::io::Result<Vec<(String, CasVerdict)>> {
    // 每文件聚合：(earliest_before, latest_after)。key = identity（§B4）。
    let mut agg: std::collections::HashMap<
        String,
        (
            &crate::protocol::MutationFile,
            &crate::protocol::MutationFile,
        ),
    > = std::collections::HashMap::new();
    for mutation in mutations {
        for file in &mutation.files {
            let key = path_identity(&file.path);
            match agg.get_mut(&key) {
                None => {
                    agg.insert(key, (file, file));
                }
                Some((_, latest_after)) => {
                    // latest_after 更新为最晚（后续 mutation 的 after）。
                    *latest_after = file;
                }
            }
        }
    }
    let mut verdicts = Vec::with_capacity(agg.len());
    let mut conflicts = false;
    for (path, (earliest, latest_after)) in &agg {
        let target = resolve_target(path, workspace_root);
        let current = std::fs::read(&target).unwrap_or_default();
        let verdict = if current == earliest.before_content {
            CasVerdict::AlreadyDone
        } else if current == latest_after.after_content {
            CasVerdict::Applied
        } else {
            CasVerdict::Conflict
        };
        if verdict == CasVerdict::Conflict {
            conflicts = true;
        }
        verdicts.push((path.clone(), verdict));
    }
    if conflicts {
        return Ok(verdicts);
    }
    for ((path, (earliest, _)), (_, verdict)) in agg.iter().zip(&verdicts) {
        if *verdict == CasVerdict::Applied {
            let target = resolve_target(path, workspace_root);
            write_atomic(&target, &earliest.before_content)?;
        }
    }
    Ok(verdicts)
}

/// redo 全部 mutations：把每个文件恢复到**最后一次**变更后的内容。
/// 与 undo_all 镜像（undo_all 恢复最早 before；redo_all 恢复最晚 after）。
pub fn redo_all(
    mutations: &[JournalMutation],
    workspace_root: &std::path::Path,
) -> std::io::Result<Vec<(String, CasVerdict)>> {
    // 每文件聚合：(earliest_before, latest_after)。key = identity（§B4）。
    let mut agg: std::collections::HashMap<
        String,
        (
            &crate::protocol::MutationFile,
            &crate::protocol::MutationFile,
        ),
    > = std::collections::HashMap::new();
    for mutation in mutations {
        for file in &mutation.files {
            let key = path_identity(&file.path);
            match agg.get_mut(&key) {
                None => {
                    agg.insert(key, (file, file));
                }
                Some((_, latest_after)) => {
                    *latest_after = file;
                }
            }
        }
    }
    let mut verdicts = Vec::with_capacity(agg.len());
    let mut conflicts = false;
    for (path, (earliest, latest_after)) in &agg {
        let target = resolve_target(path, workspace_root);
        let current = std::fs::read(&target).unwrap_or_default();
        let verdict = if current == latest_after.after_content {
            CasVerdict::AlreadyDone
        } else if current == earliest.before_content {
            CasVerdict::Applied
        } else {
            CasVerdict::Conflict
        };
        if verdict == CasVerdict::Conflict {
            conflicts = true;
        }
        verdicts.push((path.clone(), verdict));
    }
    if conflicts {
        return Ok(verdicts);
    }
    for ((path, (_, latest_after)), (_, verdict)) in agg.iter().zip(&verdicts) {
        if *verdict == CasVerdict::Applied {
            let target = resolve_target(path, workspace_root);
            write_atomic(&target, &latest_after.after_content)?;
        }
    }
    Ok(verdicts)
}

/// §B4：Windows 路径 identity（大小写折叠）。`Foo.rs`/`foo.rs` 同一物理
/// 文件——journal 按 identity 聚合/匹配，否则 case 变体产生重复条目。
/// 非 Windows 原样返回。
#[cfg(windows)]
fn path_identity(path: &str) -> String {
    path.to_lowercase()
}

#[cfg(not(windows))]
fn path_identity(path: &str) -> String {
    path.to_string()
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
        assert_eq!(loaded.mutations.len(), 1);
        assert_eq!(loaded.mutations[0].mutation_id, "m1");
        assert_eq!(loaded.mutations[0].files[0].before_content, b"old");
        assert!(!loaded.is_tainted());
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_journal(&journal_path(dir.path(), "nope")).unwrap();
        assert!(loaded.mutations.is_empty());
        assert!(!loaded.is_tainted());
    }

    /// CAS：current == after → Applied（写入 before）。
    #[test]
    fn undo_mutation_applies_when_current_is_after() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "new\n").unwrap();
        let m = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        let result = undo_mutation(&[m], "m1", dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Applied]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    }

    /// CAS：current == before → AlreadyDone（幂等跳过）。
    #[test]
    fn undo_mutation_already_done_when_current_is_before() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "old\n").unwrap();
        let m = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        let result = undo_mutation(&[m], "m1", dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::AlreadyDone]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    }

    /// CAS：current 是无关内容（用户/外部修改）→ Conflict，**不写文件**。
    /// roadmap 示例：Agent A→B / User B→C / Undo 必须 Conflict（禁止 C→A）。
    #[test]
    fn undo_conflicts_with_external_change_and_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "user-edited\n").unwrap(); // C（外部修改）
        let m = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        let result = undo_mutation(&[m], "m1", dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Conflict]);
        // 禁止 C→A：文件保持用户内容。
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "user-edited\n");
    }

    /// CAS：redo——current == before → Applied；current == after → AlreadyDone。
    #[test]
    fn redo_mutation_applies_and_already_done() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "old\n").unwrap(); // before
        let m = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        let result = redo_mutation(std::slice::from_ref(&m), "m1", dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Applied]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");

        // 已是 after → AlreadyDone。
        let result2 = redo_mutation(std::slice::from_ref(&m), "m1", dir.path()).unwrap();
        assert_eq!(verdicts(&result2), vec![CasVerdict::AlreadyDone]);
    }

    /// CAS：redo 遇到无关内容 → Conflict 不写文件。
    #[test]
    fn redo_conflicts_with_external_change() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "user-edited\n").unwrap();
        let m = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        let result = redo_mutation(&[m], "m1", dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Conflict]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "user-edited\n");
    }

    /// 原子性：mutation 内多文件，一个 Conflict → 全部不写（含本应 Applied 的）。
    #[test]
    fn undo_atomic_rejects_all_on_any_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = dir.path().join("a.rs");
        let t2 = dir.path().join("b.rs");
        std::fs::write(&t1, "new\n").unwrap(); // == after，本应 Applied
        std::fs::write(&t2, "user\n").unwrap(); // 无关内容 → Conflict
        let m = JournalMutation {
            mutation_id: "m1".into(),
            files: vec![
                MutationFile {
                    path: t1.to_string_lossy().to_string(),
                    before_revision: "b3:b".into(),
                    after_revision: "b3:a".into(),
                    before_content: b"old\n".to_vec(),
                    after_content: b"new\n".to_vec(),
                },
                MutationFile {
                    path: t2.to_string_lossy().to_string(),
                    before_revision: "b3:b".into(),
                    after_revision: "b3:a".into(),
                    before_content: b"old2\n".to_vec(),
                    after_content: b"new2\n".to_vec(),
                },
            ],
        };
        let result = undo_mutation(&[m], "m1", dir.path()).unwrap();
        assert!(verdicts(&result).contains(&CasVerdict::Conflict));
        // 原子性：a.rs 也不得被写（保持 new，不回到 old）。
        assert_eq!(std::fs::read_to_string(&t1).unwrap(), "new\n");
    }

    #[test]
    fn undo_all_restores_earliest_before() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "v2\n").unwrap();
        let m1 = mutation("m1", &target.to_string_lossy(), b"v0\n", b"v1\n");
        let m2 = mutation("m2", &target.to_string_lossy(), b"v1\n", b"v2\n");
        let result = undo_all(&[m1, m2], dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Applied]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v0\n");
    }

    #[test]
    fn redo_all_restores_latest_after() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("b.rs");
        std::fs::write(&target, "v0\n").unwrap();
        let m1 = mutation("m1", &target.to_string_lossy(), b"v0\n", b"v1\n");
        let m2 = mutation("m2", &target.to_string_lossy(), b"v1\n", b"v2\n");
        let mutations = vec![m1, m2];
        let result = redo_all(&mutations, dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Applied]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2\n");
    }

    #[test]
    fn undo_last_and_redo_last_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("c.rs");
        std::fs::write(&target, "v2\n").unwrap();
        let m1 = mutation("m1", &target.to_string_lossy(), b"v0\n", b"v1\n");
        let m2 = mutation("m2", &target.to_string_lossy(), b"v1\n", b"v2\n");
        let mutations = vec![m1, m2];
        let result = undo_last(&mutations, dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Applied]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1\n");

        let result = redo_last(&mutations, dir.path()).unwrap();
        assert_eq!(verdicts(&result), vec![CasVerdict::Applied]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2\n");
    }

    #[test]
    fn undo_last_empty_journal_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(undo_last(&[], dir.path()).is_err());
        assert!(redo_last(&[], dir.path()).is_err());
    }

    #[test]
    fn undo_unknown_mutation_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = undo_mutation(&[], "nope", dir.path());
        assert!(result.is_err());
    }

    /// §B3：损坏行 → Tainted，undo/redo 拒绝；Clean 放行。
    #[test]
    fn corrupt_line_taints_journal_and_rejects_undo() {
        let dir = tempfile::tempdir().unwrap();
        let session = "s-taint";
        // 先写一条合法 mutation，再写一条损坏行。
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
        let jpath = journal_path(dir.path(), session);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jpath)
            .unwrap();
        writeln!(f, "{{not valid json\n").unwrap();

        let state = load_journal(&jpath).unwrap();
        assert_eq!(state.mutations.len(), 1, "合法行仍加载");
        assert!(state.is_tainted(), "损坏行 → Tainted");
        assert_eq!(state.corrupt_lines, 1);

        // Tainted 拒绝 undo（force=false）。
        assert!(assert_can_mutate(&state, false).is_err());
        // force=true 放行。
        assert!(assert_can_mutate(&state, true).is_ok());
    }

    /// §B3：全合法 → Clean，undo 允许。
    #[test]
    fn clean_journal_allows_undo() {
        let dir = tempfile::tempdir().unwrap();
        let session = "s-clean";
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
        let state = load_journal(&journal_path(dir.path(), session)).unwrap();
        assert!(!state.is_tainted());
        assert!(assert_can_mutate(&state, false).is_ok());
    }

    /// §B4：同文件不同大小写路径聚合为同一条目（Windows）。
    #[test]
    fn undo_all_unifies_case_variants() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.rs");
        let target_upper = dir.path().join("A.RS");
        std::fs::write(&target, "new\n").unwrap();
        let m1 = mutation("m1", &target.to_string_lossy(), b"old\n", b"new\n");
        let m2 = mutation("m2", &target_upper.to_string_lossy(), b"old\n", b"new\n");
        let result = undo_all(&[m1, m2], dir.path()).unwrap();
        #[cfg(windows)]
        {
            // 两个 case 变体是同一物理文件 → 聚合为 1 条，Applied 一次。
            assert_eq!(result.len(), 1, "大小写变体必须聚合（§B4）");
            assert_eq!(verdicts(&result), vec![CasVerdict::Applied]);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
        }
        #[cfg(not(windows))]
        {
            // 大小写敏感：两个不同文件。
            assert_eq!(result.len(), 2);
        }
    }

    /// 辅助：提取 verdict 列表。
    fn verdicts(result: &[(String, CasVerdict)]) -> Vec<CasVerdict> {
        result.iter().map(|(_, v)| v.clone()).collect()
    }
}
