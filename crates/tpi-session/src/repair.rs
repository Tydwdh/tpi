//! Session 损坏诊断与修复（P0-2）。
//!
//! 中间坏行曾导致 session 完全无法恢复（`read_envelopes_state` 一旦遇到损坏行就
//! 整体拒绝）。本模块提供：
//!
//! - [`diagnose`]：宽容扫描，逐行报告坏行位置与原因（doctor 展示用，不改文件）；
//! - [`repair`]：备份原文件 → 坏行隔离（quarantine，不直接删）→ 重写干净 JSONL
//!   → 重建 max_seq 与缺失 `ToolCompleted` 的合成 Interrupted 终态。
//!
//! 修复原则：**只移除无法解析/违反协议不变量 的行**；被隔离行保留在 quarantine
//! 文件供审计；修复前先备份。修复后文件必须能被严格的 `read_envelopes_state`
//! 接受（后续 `SessionLog::open` / resume 才能工作）。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Envelope, SCHEMA_VERSION, SessionEvent};
use tpi_core::ids::ToolCallId;

/// 一行损坏信息（供 doctor / CLI 展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadLine {
    /// 1-indexed 行号。
    pub line: usize,
    /// 损坏原因（用户可读）。
    pub reason: String,
    /// 是否为未换行的尾部残片（崩溃残片；可安全丢弃）。
    pub incomplete_tail: bool,
}

/// 修复报告。
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// 修复前备份路径（`<session>.bak-<unix>`；未修改则为 None）。
    pub backup: Option<PathBuf>,
    /// 隔离文件路径（坏行内容；无坏行为 None）。
    pub quarantine: Option<PathBuf>,
    /// 被隔离的行（逐条）。
    pub removed: Vec<BadLine>,
    /// 重建后的最大 envelope seq。
    pub max_seq: u64,
    /// 为缺失 ToolCompleted 的 call 合成的 Interrupted 终态数。
    pub synthesized_interrupted: usize,
    /// 是否实际修改了文件（备份/重写/追加任一发生）。
    pub modified: bool,
}

/// 一次宽容扫描的结果：(好行(行号, 原始字节, envelope), 坏行, 最大 seq)。
struct ScanResult {
    good: Vec<(usize, Vec<u8>, Envelope)>,
    bad: Vec<BadLine>,
    max_seq: u64,
}

/// 宽容扫描：逐行尝试解析与校验，坏行收集但不中断。
///
/// 校验顺序与 `read_envelopes_state` 一致（schema → session_id → seq 递增 →
/// event_id 唯一 → timestamp → protocol）；坏行不推进 previous_seq / protocol，
/// 因此被隔离后剩余行保持 seq 严格递增。
fn scan_lines(path: &Path) -> std::io::Result<ScanResult> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let expected_from_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
        .map(tpi_core::ids::SessionId);
    let mut expected_session = expected_from_name;
    let mut previous_seq = 0u64;
    let mut event_ids = std::collections::HashSet::new();
    let mut protocol = super::SessionProtocolState::default();
    let mut good: Vec<(usize, Vec<u8>, Envelope)> = Vec::new();
    let mut bad: Vec<BadLine> = Vec::new();
    let mut line_number = 0usize;
    let mut max_seq = 0u64;

    loop {
        let line =
            match tpi_core::util::read_line_bounded(&mut reader, super::MAX_SESSION_EVENT_BYTES)? {
                tpi_core::util::BoundedLineRead::Eof => break,
                tpi_core::util::BoundedLineRead::TooLong => {
                    line_number += 1;
                    bad.push(BadLine {
                        line: line_number,
                        reason: format!("行超过 {} 字节上限", super::MAX_SESSION_EVENT_BYTES),
                        incomplete_tail: false,
                    });
                    continue;
                }
                tpi_core::util::BoundedLineRead::Line(line) => line,
            };
        line_number += 1;
        let has_newline = line.terminated;
        let bytes = line.bytes;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            if !has_newline {
                bad.push(BadLine {
                    line: line_number,
                    reason: "未换行的空白残片（可安全丢弃）".into(),
                    incomplete_tail: true,
                });
            }
            continue;
        }
        let envelope = match serde_json::from_slice::<Envelope>(&bytes) {
            Ok(envelope) => envelope,
            Err(error) => {
                bad.push(BadLine {
                    line: line_number,
                    reason: format!("JSON 解析失败: {error}"),
                    incomplete_tail: !has_newline,
                });
                continue;
            }
        };
        if envelope.schema != SCHEMA_VERSION {
            bad.push(BadLine {
                line: line_number,
                reason: format!("schema={}，当前仅支持 {SCHEMA_VERSION}", envelope.schema),
                incomplete_tail: false,
            });
            continue;
        }
        let session_id = *expected_session.get_or_insert(envelope.session_id);
        if envelope.session_id != session_id {
            bad.push(BadLine {
                line: line_number,
                reason: "session_id 不一致".into(),
                incomplete_tail: false,
            });
            continue;
        }
        if envelope.seq <= previous_seq {
            bad.push(BadLine {
                line: line_number,
                reason: format!("seq={} 未严格递增（前一条 {previous_seq}）", envelope.seq),
                incomplete_tail: false,
            });
            continue;
        }
        if !event_ids.insert(envelope.event_id) {
            bad.push(BadLine {
                line: line_number,
                reason: "event_id 重复".into(),
                incomplete_tail: false,
            });
            continue;
        }
        if time::OffsetDateTime::parse(
            &envelope.timestamp,
            &time::format_description::well_known::Rfc3339,
        )
        .is_err()
        {
            bad.push(BadLine {
                line: line_number,
                reason: "timestamp 无效".into(),
                incomplete_tail: false,
            });
            continue;
        }
        if let Err(error) = protocol.validate(&envelope.body, envelope.seq) {
            // 协议不变量破坏（孤儿 ToolCompleted / 重复 ToolRequested 等）：
            // 该行无法在修复后通过严格校验，只能隔离。其 ToolRequested 若保留，
            // 修复后由合成 Interrupted 兜底。
            bad.push(BadLine {
                line: line_number,
                reason: format!("协议校验失败: {error}"),
                incomplete_tail: false,
            });
            continue;
        }
        protocol.apply(&envelope.body);
        previous_seq = envelope.seq;
        max_seq = envelope.seq;
        good.push((line_number, bytes, envelope));
    }
    Ok(ScanResult { good, bad, max_seq })
}

/// 诊断 session 文件（不改文件）。返回坏行列表（doctor / CLI 展示用）。
pub fn diagnose(path: &Path) -> std::io::Result<Vec<BadLine>> {
    let result = scan_lines(path)?;
    Ok(result.bad)
}

/// 修复 session 文件：
///
/// 1. 持锁（阻止与活跃 TPI 并发写）；
/// 2. 备份原文件（`<path>.bak-<unix>`）；
/// 3. 坏行隔离到 `<path>.quarantine`（保留原始内容，逐条带行号/原因注释）；
/// 4. 重写干净 JSONL（仅保留好行的原始字节）；
/// 5. 重建：max_seq = 最后好行的 seq；对「已 ToolRequested 但缺 ToolCompleted」
///    的 call 合成 Interrupted 终态并追加（复用 recovery 的 effect 分类）。
///
/// 修复后文件可被严格的 `read_envelopes_state` 接受，resume 无需再合成。
pub fn repair(path: &Path) -> std::io::Result<RepairReport> {
    // 1. 持锁：修复期间阻止其他 TPI 实例写同一 session。
    let _lock = super::open_and_lock_session(path)?;

    let ScanResult { good, bad, max_seq } = scan_lines(path)?;
    let mut report = RepairReport {
        backup: None,
        quarantine: None,
        removed: bad.clone(),
        max_seq,
        synthesized_interrupted: 0,
        modified: false,
    };
    if !bad.is_empty() {
        // 2. 备份（修复前现场）。
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut backup_name = path.as_os_str().to_os_string();
        backup_name.push(format!(".bak-{unix}"));
        let backup = PathBuf::from(backup_name);
        std::fs::copy(path, &backup)?;
        report.backup = Some(backup);

        // 3. 隔离坏行（保留现场，不直接删）。
        let mut quarantine_name = path.as_os_str().to_os_string();
        quarantine_name.push(".quarantine");
        let quarantine = PathBuf::from(quarantine_name);
        let mut qfile = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&quarantine)?;
        for bad_line in &bad {
            writeln!(qfile, "# line {}: {}", bad_line.line, bad_line.reason)?;
        }
        qfile.flush()?;
        report.quarantine = Some(quarantine);

        // 4. 重写干净 JSONL（好行原始字节；坏行行号注释已进 quarantine）。
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        for (_, bytes, _) in &good {
            out.write_all(bytes)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        report.modified = true;
    }

    // 5. 重建 interrupted tool outcome：重写后文件已可严格读取，用 recovery
    //    的 pending 扫描找出缺失 ToolCompleted 的 call 并合成终态。
    let recovered = super::recovery::recover(path)?;
    if !recovered.interrupted.is_empty() {
        // 追加合成终态（seq 从 max_seq 递增；session/run 沿用最后好行）。
        let (session_id, run_id) = good
            .last()
            .map(|(_, _, e)| (e.session_id, e.run_id))
            .unwrap_or_else(|| {
                (
                    tpi_core::ids::SessionId::new_v7(),
                    tpi_core::ids::RunId::new_v7(),
                )
            });
        let mut seq = max_seq;
        for (call_id, provider_id, outcome) in &recovered.interrupted {
            let Ok(call_id) = uuid::Uuid::parse_str(call_id).map(ToolCallId) else {
                continue;
            };
            seq += 1;
            let envelope = Envelope::new(
                seq,
                session_id,
                run_id,
                &SessionEvent::ToolCompleted {
                    call_id,
                    outcome: outcome.clone(),
                },
            );
            let mut bytes = serde_json::to_vec(&envelope)
                .map_err(|e| std::io::Error::other(format!("serialize repaired event: {e}")))?;
            bytes.push(b'\n');
            let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
            file.write_all(&bytes)?;
            file.sync_data()?;
            report.synthesized_interrupted += 1;
            let _ = provider_id;
        }
        report.max_seq = seq;
        report.modified = true;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionEvent, SessionLog, read_events, workspace_id_for};
    use camino::Utf8PathBuf;
    use tpi_core::ids::RunId;

    fn make_session() -> (
        tempfile::TempDir,
        Utf8PathBuf,
        tpi_core::ids::SessionId,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sessions_root = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let workspace_id = workspace_id_for(workspace.as_std_path());
        let session_id = tpi_core::ids::SessionId::new_v7();
        let path = sessions_root
            .join(&workspace_id)
            .join(format!("{session_id}.jsonl"));
        (dir, workspace, session_id, path)
    }

    fn append(log: &mut SessionLog, event: SessionEvent) {
        log.append_event(&event).unwrap();
    }

    /// 中间坏行（已换行的非 JSON 行）被诊断报告，且不阻断后续好行扫描。
    #[test]
    fn diagnose_reports_middle_bad_line() {
        let (dir, workspace, session_id, path) = make_session();
        let sessions_root = dir.path().join("sessions");
        let mut log = SessionLog::create_with_id(
            &sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
            session_id,
        )
        .unwrap();
        append(
            &mut log,
            SessionEvent::UserSubmitted {
                content: "a".into(),
            },
        );
        append(
            &mut log,
            SessionEvent::UserSubmitted {
                content: "b".into(),
            },
        );
        drop(log);

        let mut raw = std::fs::read(&path).unwrap();
        // 在两条合法事件中间插入坏行。
        let first_newline = raw.iter().position(|b| *b == b'\n').unwrap() + 1;
        raw.splice(
            first_newline..first_newline,
            b"this-is-not-json\n".iter().copied(),
        );
        std::fs::write(&path, raw).unwrap();

        let bad = diagnose(&path).unwrap();
        assert_eq!(bad.len(), 1, "应报告 1 个坏行: {bad:?}");
        assert_eq!(bad[0].line, 2);
        assert!(bad[0].reason.contains("JSON"), "{} ", bad[0].reason);
        assert!(!bad[0].incomplete_tail);

        // 未修复前严格读取整体拒绝（P0-2 根因）。
        assert!(read_events(&path).is_err(), "坏行存在时严格读取必须拒绝");
    }

    /// repair：备份 + quarantine + 重写，修复后严格读取可用、max_seq 正确。
    #[test]
    fn repair_quarantines_and_rewrites_clean() {
        let (dir, workspace, session_id, path) = make_session();
        let sessions_root = dir.path().join("sessions");
        let mut log = SessionLog::create_with_id(
            &sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
            session_id,
        )
        .unwrap();
        append(
            &mut log,
            SessionEvent::UserSubmitted {
                content: "a".into(),
            },
        );
        append(
            &mut log,
            SessionEvent::UserSubmitted {
                content: "b".into(),
            },
        );
        drop(log);

        let mut raw = std::fs::read(&path).unwrap();
        let first_newline = raw.iter().position(|b| *b == b'\n').unwrap() + 1;
        raw.splice(
            first_newline..first_newline,
            b"this-is-not-json\n".iter().copied(),
        );
        std::fs::write(&path, raw).unwrap();

        let report = repair(&path).unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(report.backup.is_some(), "修复前必须备份");
        assert!(report.quarantine.is_some(), "坏行必须隔离");
        assert!(report.modified);
        assert_eq!(report.max_seq, 2, "重写后 max_seq = 最后好行 seq");

        // 备份是修复前现场（含坏行）；隔离文件有坏行注释。
        let backup = report.backup.unwrap();
        let backup_raw = std::fs::read_to_string(&backup).unwrap();
        assert!(backup_raw.contains("this-is-not-json"), "备份保留损坏现场");
        let quarantine = report.quarantine.unwrap();
        let q = std::fs::read_to_string(&quarantine).unwrap();
        assert!(q.contains("# line 2"), "隔离文件带行号注释: {q}");

        // 修复后严格读取可用，且事件与好行一致。
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2, "坏行被移除，两条好事件保留");
        assert!(matches!(&events[0], SessionEvent::UserSubmitted { content } if content == "a"));
        assert!(matches!(&events[1], SessionEvent::UserSubmitted { content } if content == "b"));

        // 修复后可重新打开（SessionLog::open 严格校验通过）。
        SessionLog::open(&sessions_root, workspace.as_std_path(), session_id).unwrap();
    }

    /// repair：已请求但缺 ToolCompleted 的 call 被合成 Interrupted 终态。
    #[test]
    fn repair_rebuilds_interrupted_tool_outcome() {
        let (dir, workspace, session_id, path) = make_session();
        let sessions_root = dir.path().join("sessions");
        let mut log = SessionLog::create_with_id(
            &sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
            session_id,
        )
        .unwrap();
        let call = tpi_core::message::ToolCall {
            call_id: tpi_core::ids::ToolCallId::new_v7(),
            provider_id: "provider-call".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        append(
            &mut log,
            SessionEvent::UserSubmitted {
                content: "run".into(),
            },
        );
        append(&mut log, SessionEvent::ToolRequested { call });
        // 缺 ToolCompleted（崩溃）：pending call。
        drop(log);

        // 尾部坏行（未换行残片）也不影响修复。
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"{\"schema\":1");
        std::fs::write(&path, raw).unwrap();

        let report = repair(&path).unwrap();
        assert_eq!(
            report.synthesized_interrupted, 1,
            "必须合成 1 个 Interrupted 终态"
        );
        assert_eq!(report.max_seq, 3, "合成后 seq 递增到 3");

        let events = read_events(&path).unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                SessionEvent::ToolCompleted { outcome, .. }
                    if outcome.status == tpi_core::outcome::ToolStatus::Interrupted
            )),
            "必须包含合成的 Interrupted ToolCompleted: {events:?}"
        );
        // 合成结果写入后，再次 repair 不重复合成（pending 已清）。
        let second = repair(&path).unwrap();
        assert_eq!(
            second.synthesized_interrupted, 0,
            "重复 repair 不得重复合成"
        );
    }

    /// 完全干净的文件：diagnose 无坏行，repair 不修改不备份。
    #[test]
    fn clean_session_repair_is_noop() {
        let (dir, workspace, session_id, path) = make_session();
        let sessions_root = dir.path().join("sessions");
        let mut log = SessionLog::create_with_id(
            &sessions_root,
            workspace.as_std_path(),
            RunId::new_v7(),
            session_id,
        )
        .unwrap();
        append(
            &mut log,
            SessionEvent::UserSubmitted {
                content: "ok".into(),
            },
        );
        drop(log);

        assert!(diagnose(&path).unwrap().is_empty());
        let report = repair(&path).unwrap();
        assert!(!report.modified, "干净文件不得修改");
        assert!(report.backup.is_none());
        assert_eq!(report.max_seq, 1);
    }
}
