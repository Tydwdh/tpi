//! Revision-bound 可靠编辑（文档 §2.2、§10）。
//!
//! - revision 对原始字节计算 BLAKE3，协议传完整 256 bit digest：`b3:<64-hex>`（§10.1）。
//! - 内部使用 `logical_lf_text`（去 BOM、CRLF→LF）作为匹配空间，
//!   但写回时只替换命中的原始 byte ranges，未触及字节原样保留（§10.5）。
//! - 第一版严格拒绝 stale revision，不自动 fuzzy rebase（§10.3 第 12 条）。
//! - `write` 只创建新文件（§10.6）。
//! - M3：Windows 提交使用 `ReplaceFileW` + 同卷唯一 backup（§10.7）；
//!   成功校验 backup digest，失败/校验不符进入可诊断恢复。

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use serde::Deserialize;

pub use crate::tool::outcome::Effect;

/// edit 工具参数（§10.3）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct EditArgs {
    pub path: String,
    /// `read` 输出的 `[revision=...]` 或裸 `b3:<64-hex>`。
    pub revision: String,
    pub replacements: Vec<Replacement>,
}

/// revision 前缀（§10.1：协议传完整 256 bit digest）。
pub const REVISION_PREFIX: &str = "b3:";

/// 读文件的大小上限（超过视为不可合理快照，返回类型化结果）。
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// 计算文件原始字节的 revision（§10.1）。
pub fn revision_of(raw: &[u8]) -> String {
    let digest = blake3::hash(raw);
    format!("{REVISION_PREFIX}{}", digest.to_hex())
}

/// 校验 revision 格式（`b3:` + 64 hex）。
pub fn is_valid_revision(value: &str) -> bool {
    let Some(hex) = value.strip_prefix(REVISION_PREFIX) else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 模型可见的 revision 头（与 `read`/`edit`/`write` 共享，§2.2）。
pub fn format_revision_header(revision: &str) -> String {
    format!("[revision={revision}]")
}

/// 接受裸 revision token 或 `read` 输出的完整 header（§2.2：展示值可原样回传）。
pub fn parse_revision_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let candidate = if let Some(inner) = trimmed
        .strip_prefix("[revision=")
        .and_then(|rest| rest.strip_suffix(']'))
    {
        inner
    } else {
        trimmed
    };
    if is_valid_revision(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// 行尾策略（§10.5：按 anchor 附近行尾 / 文件多数行尾编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

/// 文件快照（§10.1）。
#[derive(Debug)]
pub struct FileSnapshot {
    pub path: Utf8PathBuf,
    pub raw: Arc<[u8]>,
    pub logical_lf_text: Arc<str>,
    pub revision: String,
    /// 是否有 UTF-8 BOM。
    pub has_bom: bool,
    /// 替换编码使用的行尾（文件多数行尾；mixed 文件按 §10.5 策略）。
    pub line_ending: LineEnding,
    /// 逻辑 byte 位置 → 原始 byte 位置的单调映射（len = logical len + 1）。
    logical_to_raw: Vec<usize>,
}

/// 解析文本文件为快照（§10.1：UTF-8 与 UTF-8 BOM；二进制返回类型化错误）。
pub fn snapshot_file(path: &Utf8PathBuf) -> Result<FileSnapshot, EditError> {
    let raw = match std::fs::read(path.as_std_path()) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(EditError::NotFound { path: path.clone() });
        }
        Err(error) => {
            return Err(EditError::Io {
                path: path.clone(),
                message: error.to_string(),
            });
        }
    };
    build_snapshot(path.clone(), raw)
}

pub fn build_snapshot(path: Utf8PathBuf, raw: Vec<u8>) -> Result<FileSnapshot, EditError> {
    if raw.len() > MAX_SNAPSHOT_BYTES {
        return Err(EditError::FileTooLarge {
            path,
            bytes: raw.len(),
        });
    }
    let (has_bom, body) = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        (true, &raw[3..])
    } else {
        (false, &raw[..])
    };
    // 严格 UTF-8 解码（§10.1：无法解码返回类型化结果，不做 lossy rewrite）。
    let text = std::str::from_utf8(body)
        .map_err(|_| EditError::UnsupportedEncoding { path: path.clone() })?;
    let (logical_lf_text, logical_to_raw, line_ending) = normalize_lf(text, has_bom);
    let revision = revision_of(&raw);
    Ok(FileSnapshot {
        path,
        raw: Arc::from(raw),
        logical_lf_text: Arc::from(logical_lf_text),
        revision,
        has_bom,
        line_ending,
        logical_to_raw,
    })
}

/// 把 UTF-8 文本归一化为 LF 逻辑文本，同时构建逻辑偏移 → 原始偏移映射。
///
/// BOM 已剥离；`\r\n` 的 `\r` 不进入逻辑文本（§10.1：`logical_lf_text` 去 BOM 并统一 `\n`）。
fn normalize_lf(text: &str, has_bom: bool) -> (String, Vec<usize>, LineEnding) {
    let raw_offset_base = if has_bom { 3 } else { 0 };
    let bytes = text.as_bytes();
    let mut logical = String::with_capacity(bytes.len());
    let mut map = Vec::with_capacity(bytes.len() + 1);
    let mut crlf_count = 0usize;
    let mut lf_count = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            crlf_count += 1;
            map.push(raw_offset_base + i);
            logical.push('\n');
            i += 2;
        } else {
            if b == b'\n' {
                lf_count += 1;
            }
            map.push(raw_offset_base + i);
            logical.push(b as char);
            i += 1;
        }
    }
    map.push(raw_offset_base + bytes.len());
    let line_ending = if crlf_count > lf_count {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    (logical, map, line_ending)
}

/// 编辑错误诊断（§10.3 第 12 条：机器可辨）。
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum EditError {
    #[error("path not found: {path}")]
    NotFound { path: Utf8PathBuf },
    #[error("unsupported encoding: {path} (only UTF-8 and UTF-8 BOM are supported)")]
    UnsupportedEncoding { path: Utf8PathBuf },
    #[error("file too large: {path} ({bytes} bytes)")]
    FileTooLarge { path: Utf8PathBuf, bytes: usize },
    #[error("stale revision: {path} current={current} expected={expected}")]
    StaleRevision {
        path: Utf8PathBuf,
        current: String,
        expected: String,
    },
    #[error("invalid revision: {value}")]
    InvalidRevision { value: String },
    #[error("old_text must not be empty (path={path})")]
    EmptyOldText { path: Utf8PathBuf },
    #[error("no match for replacements[{index}] in {path}")]
    NoMatch { path: Utf8PathBuf, index: usize },
    #[error("multiple matches for replacements[{index}] in {path} (must be unique)")]
    MultipleMatches { path: Utf8PathBuf, index: usize },
    #[error("replacements[{a}] and replacements[{b}] overlap in {path}")]
    Overlap {
        path: Utf8PathBuf,
        a: usize,
        b: usize,
    },
    #[error("replacement {index} is a no-op (old_text == new_text) in {path}")]
    NoOp { path: Utf8PathBuf, index: usize },
    #[error("non-canonical line endings in replacements[{index}] of {path} (\\r not allowed)")]
    NonCanonicalLineEndings { path: Utf8PathBuf, index: usize },
    #[error("target already exists: {path}")]
    AlreadyExists { path: Utf8PathBuf },
    #[error("commit failed: {path}: {message}")]
    CommitFailed { path: Utf8PathBuf, message: String },
    #[error("concurrent modification during commit: {path} (backup digest mismatch)")]
    ConcurrentModification { path: Utf8PathBuf },
    #[error("commit recovery failed: {path}: {message}")]
    CommitRecoveryFailed { path: Utf8PathBuf, message: String },
    #[error("io error on {path}: {message}")]
    Io { path: Utf8PathBuf, message: String },
}

impl EditError {
    /// 机器可辨诊断名（§10.3 第 12 条）。
    pub fn code(&self) -> &'static str {
        match self {
            EditError::NotFound { .. } => "not_found",
            EditError::UnsupportedEncoding { .. } => "unsupported_encoding",
            EditError::FileTooLarge { .. } => "file_too_large",
            EditError::StaleRevision { .. } => "stale_revision",
            EditError::InvalidRevision { .. } => "invalid_revision",
            EditError::EmptyOldText { .. } => "empty_old_text",
            EditError::NoMatch { .. } => "no_match",
            EditError::MultipleMatches { .. } => "multiple_matches",
            EditError::Overlap { .. } => "overlap",
            EditError::NoOp { .. } => "no_op",
            EditError::NonCanonicalLineEndings { .. } => "non_canonical_line_endings",
            EditError::AlreadyExists { .. } => "already_exists",
            EditError::CommitFailed { .. } => "commit_failed",
            EditError::ConcurrentModification { .. } => "concurrent_modification_during_commit",
            EditError::CommitRecoveryFailed { .. } => "commit_recovery_failed",
            EditError::Io { .. } => "io_error",
        }
    }
}

/// 一次 replacement（§10.3）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct Replacement {
    pub old_text: String,
    pub new_text: String,
}

/// 预检通过的 replacement（逻辑坐标已解析）。
#[derive(Debug, Clone)]
struct PreparedReplacement {
    index: usize,
    logical_start: usize,
    logical_end: usize,
    new_logical: String,
}

/// 应用 replacement 后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditResult {
    pub previous_revision: String,
    pub current_revision: String,
    pub applied: usize,
    /// 编辑前原始字节（unified diff 与诊断用）。
    pub previous_raw: Vec<u8>,
    pub new_raw: Vec<u8>,
}

/// 生成 unified diff（§10.3 第 10 条；仅用于展示与验证，不用于猜测式修改）。
pub fn unified_diff(result: &EditResult) -> String {
    use similar::TextDiff;
    let old_text = String::from_utf8_lossy(&result.previous_raw);
    let new_text = String::from_utf8_lossy(&result.new_raw);
    if old_text == new_text {
        return String::new();
    }
    let diff = TextDiff::from_lines(&old_text, &new_text);
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// 精确匹配次数（KMP；返回 0、1 或 2+ 即可）。
pub fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    let mut count = 0usize;
    let mut start = 0usize;
    while let Some(offset) = haystack[start..].find(needle) {
        count += 1;
        if count > 1 {
            return count;
        }
        start += offset + needle.len();
    }
    count
}

/// 执行 revision-bound exact edit（§10.3）。
///
/// 流程：读快照 → revision 校验 → 全部 replacements 预检（存在/唯一/不重叠/no-op）
/// → 一次性应用 → temp + sync → 原子替换。
pub fn apply_edit(
    path: &Utf8PathBuf,
    revision: &str,
    replacements: &[Replacement],
) -> Result<EditResult, EditError> {
    let Some(revision) = parse_revision_token(revision) else {
        return Err(EditError::InvalidRevision {
            value: revision.to_string(),
        });
    };
    let snapshot = snapshot_file(path)?;
    apply_edit_to_snapshot(snapshot, &revision, replacements)
}

fn apply_edit_to_snapshot(
    snapshot: FileSnapshot,
    expected_revision: &str,
    replacements: &[Replacement],
) -> Result<EditResult, EditError> {
    if replacements.is_empty() {
        return Err(EditError::EmptyOldText {
            path: snapshot.path.clone(),
        });
    }
    if snapshot.revision != expected_revision {
        return Err(EditError::StaleRevision {
            path: snapshot.path.clone(),
            current: snapshot.revision,
            expected: expected_revision.to_string(),
        });
    }

    // 预检（§10.3 第 1-5 条）：全部通过才应用。
    let mut prepared: Vec<PreparedReplacement> = Vec::with_capacity(replacements.len());
    for (index, replacement) in replacements.iter().enumerate() {
        if replacement.old_text.is_empty() {
            return Err(EditError::EmptyOldText {
                path: snapshot.path.clone(),
            });
        }
        if replacement.old_text.contains('\r') || replacement.new_text.contains('\r') {
            return Err(EditError::NonCanonicalLineEndings {
                path: snapshot.path.clone(),
                index,
            });
        }
        if replacement.old_text == replacement.new_text {
            return Err(EditError::NoOp {
                path: snapshot.path.clone(),
                index,
            });
        }
        let occurrences = count_occurrences(&snapshot.logical_lf_text, &replacement.old_text);
        if occurrences == 0 {
            return Err(EditError::NoMatch {
                path: snapshot.path.clone(),
                index,
            });
        }
        if occurrences > 1 {
            return Err(EditError::MultipleMatches {
                path: snapshot.path.clone(),
                index,
            });
        }
        let logical_start = snapshot
            .logical_lf_text
            .find(&replacement.old_text)
            .unwrap();
        prepared.push(PreparedReplacement {
            index,
            logical_start,
            logical_end: logical_start + replacement.old_text.len(),
            new_logical: replacement.new_text.clone(),
        });
    }

    // 不重叠（§10.3 第 4 条：按逻辑坐标排序检查）。
    let mut sorted = prepared.clone();
    sorted.sort_by_key(|r| r.logical_start);
    for pair in sorted.windows(2) {
        if pair[0].logical_end > pair[1].logical_start {
            return Err(EditError::Overlap {
                path: snapshot.path.clone(),
                a: pair[0].index,
                b: pair[1].index,
            });
        }
    }

    // 一次性应用：从后往前替换（保持前缀偏移有效）。
    let mut new_raw = snapshot.raw.to_vec();
    let has_bom = snapshot.has_bom;
    let line_ending = snapshot.line_ending;
    let logical_to_raw = &snapshot.logical_to_raw;
    for r in prepared.iter().rev() {
        let raw_start = logical_to_raw[r.logical_start];
        let raw_end = logical_to_raw[r.logical_end];
        let new_bytes =
            encode_replacement(&r.new_logical, raw_start, &new_raw, line_ending, has_bom);
        new_raw.splice(raw_start..raw_end, new_bytes);
    }

    let previous_revision = snapshot.revision.clone();
    let current_revision = revision_of(&new_raw);
    Ok(EditResult {
        previous_revision,
        current_revision,
        applied: prepared.len(),
        previous_raw: snapshot.raw.to_vec(),
        new_raw,
    })
}

/// 把 replacement 的 LF 编码为目标行尾（§10.5：anchor 附近行尾，M1 用文件多数行尾）。
fn encode_replacement(
    new_logical: &str,
    raw_start: usize,
    new_raw: &[u8],
    line_ending: LineEnding,
    has_bom: bool,
) -> Vec<u8> {
    // anchor 附近行尾：替换起点前最近的原始行尾序列。
    let mut anchor_ending: Option<LineEnding> = None;
    let body_offset = if has_bom { 3 } else { 0 };
    if raw_start > body_offset {
        let mut i = raw_start - 1;
        loop {
            let b = new_raw[i];
            if b == b'\n' {
                anchor_ending = if i > body_offset && new_raw[i - 1] == b'\r' {
                    Some(LineEnding::Crlf)
                } else {
                    Some(LineEnding::Lf)
                };
                break;
            }
            if i == body_offset {
                break;
            }
            i -= 1;
        }
    }
    let ending = anchor_ending.unwrap_or(line_ending);
    match ending {
        LineEnding::Lf => new_logical.as_bytes().to_vec(),
        LineEnding::Crlf => {
            let mut out = Vec::with_capacity(new_logical.len() + 8);
            for &b in new_logical.as_bytes() {
                if b == b'\n' {
                    out.push(b'\r');
                }
                out.push(b);
            }
            out
        }
    }
}

/// 同目录唯一临时文件路径（§10.3 第 9 条；不依赖外部 temp 库）。
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path(dir: &Path, target_name: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    dir.join(format!(".tpi-{target_name}-{pid}-{counter}-{nanos}.tmp"))
}

/// 提交计划（§10.7 第 1 条：副作用前确定 target/temp/backup 标识并持久化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPlan {
    /// 同目录唯一 temp（内容 = 新版本）。
    pub temp_path: std::path::PathBuf,
    /// 同目录唯一 backup（ReplaceFileW 备份旧 target）。
    pub backup_path: Option<std::path::PathBuf>,
}

/// 生成提交计划（只生成唯一路径，不创建文件；§10.7：标识先于副作用持久化）。
pub fn prepare_commit(path: &Utf8PathBuf) -> CommitPlan {
    let dir = path
        .parent()
        .map(|p| p.as_std_path().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    CommitPlan {
        temp_path: unique_temp_path(&dir, "edit"),
        backup_path: Some(unique_temp_path(&dir, "backup")),
    }
}

/// 原子替换：Windows 用 `ReplaceFileW(target, temp, backup)`（§10.7），Unix 用 `rename`。
pub fn replace_file(
    temp_path: &Path,
    target: &Path,
    backup_path: Option<&Path>,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
        let to_wide = |p: &Path| {
            let mut wide: Vec<u16> = p.as_os_str().encode_wide().collect();
            wide.push(0);
            wide
        };
        let replaced = to_wide(target);
        let replacement = to_wide(temp_path);
        let backup = backup_path.map(to_wide).unwrap_or_default();
        let result = unsafe {
            ReplaceFileW(
                replaced.as_ptr(),
                replacement.as_ptr(),
                if backup.is_empty() {
                    std::ptr::null()
                } else {
                    backup.as_ptr()
                },
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(temp_path, target)?;
        Ok(())
    }
}

/// no-clobber 安装：目标已存在时失败（§10.6 write）。
pub fn install_no_clobber(temp_path: &Path, target: &Path) -> Result<(), InstallError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let to_wide = |p: &Path| {
            let mut wide: Vec<u16> = p.as_os_str().encode_wide().collect();
            wide.push(0);
            wide
        };
        let from = to_wide(temp_path);
        let to = to_wide(target);
        let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
        if result == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(183) {
                // ERROR_ALREADY_EXISTS
                return Err(InstallError::AlreadyExists);
            }
            return Err(InstallError::Io(error.to_string()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(temp_path, target).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                InstallError::AlreadyExists
            } else {
                InstallError::Io(e.to_string())
            }
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("target already exists")]
    AlreadyExists,
    #[error("io: {0}")]
    Io(String),
}

/// 提交编辑结果到磁盘（§10.3 第 9-10 条、§10.7 恢复协议）。
///
/// 流程：temp 写入 + sync → 紧邻提交 final freshness validation → `ReplaceFileW`
/// → 校验 target/backup digest → 持久化 ToolCompleted 后由调用方删除 backup。
pub fn commit_edit(
    result: &EditResult,
    path: &Utf8PathBuf,
    plan: &CommitPlan,
) -> Result<(), EditError> {
    // §10.3 第 9 条：同目录 temp、sync_all 后关闭，紧邻提交。
    let temp_path = &plan.temp_path;
    write_temp_synced_from(temp_path, &result.new_raw).map_err(|e| EditError::Io {
        path: path.clone(),
        message: format!("temp write: {e}"),
    })?;

    // §10.3 第 6 条：临时文件准备完成后紧邻提交再校验一次 file identity 与完整 digest。
    let current = revision_of(
        &std::fs::read(path.as_std_path()).map_err(|e| EditError::Io {
            path: path.clone(),
            message: format!("final freshness read: {e}"),
        })?,
    );
    if current != result.previous_revision {
        let _ = std::fs::remove_file(temp_path);
        return Err(EditError::StaleRevision {
            path: path.clone(),
            current,
            expected: result.previous_revision.clone(),
        });
    }

    // §10.7 第 3 步：ReplaceFileW(target, temp, backup)。
    let backup_path = plan.backup_path.clone();
    if let Err(error) = replace_file(temp_path, path.as_std_path(), backup_path.as_deref()) {
        // §10.7 第 5 步：API 失败 → 根据 target/temp/backup 的存在性和 digest 恢复。
        return recover_after_failure(path, &result.new_raw, &backup_path, error);
    }

    // §10.7 第 4 步：成功后校验 target 为 candidate、backup 为 expected。
    verify_after_replace(
        path,
        &backup_path,
        &result.previous_revision,
        &revision_of(&result.new_raw),
    )?;
    // §10.7 第 6 步：temp 立即清理；backup 是崩溃恢复判定（effect=committed）的
    // 现场证据，必须保留到 ToolCompleted 持久化之后（由 agent 层清理，见 execute_batch）。
    let _ = std::fs::remove_file(temp_path);
    Ok(())
}

/// 提交后校验（§10.7 第 4 步 / §10.4 竞态窗口）。
///
/// - target 不是 candidate → 外部并发修改，恢复并返回并发诊断；
/// - backup digest != expected → backup 已包含未预期的外部变化，恢复并返回并发诊断。
///
/// 独立函数以便竞态测试直接覆盖（§20.2 场景 19）。
pub fn verify_after_replace(
    path: &Utf8PathBuf,
    backup_path: &Option<std::path::PathBuf>,
    expected_revision: &str,
    candidate_digest: &str,
) -> Result<(), EditError> {
    let target_now =
        revision_of(
            &std::fs::read(path.as_std_path()).map_err(|e| EditError::Io {
                path: path.clone(),
                message: format!("post-commit read: {e}"),
            })?,
        );
    if target_now != candidate_digest {
        // 提交后 target 不是 candidate：外部并发修改（§10.4 竞态窗口）。
        restore_target(path, backup_path, "target != candidate");
        return Err(EditError::ConcurrentModification { path: path.clone() });
    }
    if let Some(backup) = backup_path
        && backup.exists()
    {
        let backup_digest = revision_of(&std::fs::read(backup).map_err(|e| EditError::Io {
            path: path.clone(),
            message: format!("backup read: {e}"),
        })?);
        if backup_digest != expected_revision {
            // §10.4：backup 已包含未预期的外部变化 → 恢复并返回并发诊断。
            restore_target(path, backup_path, "backup digest mismatch");
            return Err(EditError::ConcurrentModification { path: path.clone() });
        }
    }
    Ok(())
}

/// 用 backup 恢复 target（§10.7 第 5 步：API 失败或校验不符时的恢复流程）。
fn restore_target(path: &Utf8PathBuf, backup_path: &Option<std::path::PathBuf>, reason: &str) {
    if let Some(backup) = backup_path
        && backup.exists()
        && std::fs::copy(backup, path.as_std_path()).is_ok()
    {
        tracing::warn!(%path, %reason, "edit commit restored target from backup");
        return;
    }
    tracing::warn!(%path, %reason, "edit commit: backup unavailable, target state uncertain");
}

/// §10.7 第 5 步：ReplaceFileW 失败后的确定性恢复。
fn recover_after_failure(
    path: &Utf8PathBuf,
    new_raw: &[u8],
    backup_path: &Option<std::path::PathBuf>,
    error: std::io::Error,
) -> Result<(), EditError> {
    let target_exists = path.as_std_path().exists();
    let target_digest = target_exists
        .then(|| std::fs::read(path.as_std_path()).ok())
        .flatten()
        .map(|raw| revision_of(&raw));
    let new_digest = revision_of(new_raw);
    let backup_digest = backup_path
        .as_ref()
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read(p).ok())
        .map(|raw| revision_of(&raw));

    match (target_digest, backup_digest) {
        // target 已包含新内容：提交实际成功（ReplaceFileW 返回失败但完成）。
        (Some(digest), _) if digest == new_digest => {
            if let Some(backup) = backup_path {
                let _ = std::fs::remove_file(backup);
            }
            tracing::warn!(%path, %error, "ReplaceFileW reported failure but target is candidate");
            Ok(())
        }
        // target 是旧内容且 backup 是旧内容：未提交，恢复完成。
        (Some(expected), Some(backup)) if expected == backup => {
            if let Some(backup) = backup_path {
                let _ = std::fs::remove_file(backup);
            }
            tracing::warn!(%path, %error, "ReplaceFileW failed; target unchanged (restored)");
            Ok(())
        }
        // target 不存在但 backup 存在：原文件已被移动，用 backup 恢复。
        (None, Some(_)) => {
            if let Some(backup) = backup_path
                && std::fs::copy(backup, path.as_std_path()).is_ok()
            {
                let _ = std::fs::remove_file(backup);
                tracing::warn!(%path, %error, "ReplaceFileW failed; restored from backup");
                return Ok(());
            }
            Err(EditError::CommitRecoveryFailed {
                path: path.clone(),
                message: format!("target missing, backup restore failed: {error}"),
            })
        }
        // 无法证明恢复完成：保留所有文件并返回 commit_recovery_failed（§10.7 第 5 条）。
        _ => Err(EditError::CommitRecoveryFailed {
            path: path.clone(),
            message: format!("cannot prove recovery: {error}"),
        }),
    }
}

/// 写临时文件并同步（§10.3 第 9 条：同目录 temp、sync_all、关闭）。
fn write_temp_synced_from(temp_path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    Ok(())
}

/// create-only 写入（§10.6）：temp + sync + no-clobber move。
pub fn write_new_file(
    path: &Utf8PathBuf,
    content: &[u8],
    plan: &CommitPlan,
) -> Result<String, EditError> {
    let dir = path
        .parent()
        .map(|p| p.as_std_path().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir).map_err(|e| EditError::Io {
        path: path.clone(),
        message: format!("create dirs: {e}"),
    })?;
    // §10.7：temp 路径来自提交计划（ToolStarted 前已持久化）。
    let temp_path = &plan.temp_path;
    write_temp_synced_from(temp_path, content).map_err(|e| EditError::Io {
        path: path.clone(),
        message: format!("temp write: {e}"),
    })?;
    match install_no_clobber(temp_path, path.as_std_path()) {
        Ok(()) => {
            let _ = std::fs::remove_file(temp_path);
            Ok(revision_of(content))
        }
        Err(InstallError::AlreadyExists) => {
            let _ = std::fs::remove_file(temp_path);
            Err(EditError::AlreadyExists { path: path.clone() })
        }
        Err(InstallError::Io(message)) => {
            let _ = std::fs::remove_file(temp_path);
            Err(EditError::CommitFailed {
                path: path.clone(),
                message,
            })
        }
    }
}

/// Session-local bounded SnapshotStore（§10.1）。
///
/// 每个 session 拥有自己的 store，不能是进程全局 singleton。
/// 默认保存最多 64 个文件、每个文件 8 个 revision；淘汰只影响 rebase/诊断，
/// 不影响磁盘文件。`read` 在合理文件大小下保存完整 snapshot。
#[derive(Debug)]
pub struct SnapshotStore {
    versions: std::collections::HashMap<Utf8PathBuf, std::collections::VecDeque<FileSnapshot>>,
    /// 插入顺序（淘汰最旧 path 用；HashMap 迭代序随机，不能当作 LRU）。
    order: std::collections::VecDeque<Utf8PathBuf>,
    max_paths: usize,
    max_versions_per_path: usize,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new(64, 8)
    }
}

impl SnapshotStore {
    pub fn new(max_paths: usize, max_versions_per_path: usize) -> Self {
        Self {
            versions: Default::default(),
            order: Default::default(),
            max_paths,
            max_versions_per_path,
        }
    }

    /// 记录一个快照（§10.1：read 保存完整 snapshot）。
    /// P1-7：同 path 更新时 move-to-back——活跃文件不能因为最早插入被淘汰
    /// （此前 order 只在首次插入时 push_back，不是真 LRU）。
    pub fn record(&mut self, snapshot: FileSnapshot) {
        let path = snapshot.path.clone();
        if let Some(pos) = self.order.iter().position(|p| *p == path) {
            self.order.remove(pos);
        }
        self.order.push_back(path.clone());
        let entry = self.versions.entry(path.clone()).or_default();
        entry.retain(|old| old.revision != snapshot.revision);
        entry.push_front(snapshot);
        while entry.len() > self.max_versions_per_path {
            entry.pop_back();
        }
        // 淘汰最旧 path（按插入顺序）。
        while self.versions.len() > self.max_paths {
            if let Some(oldest) = self.order.pop_front() {
                self.versions.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// 按 revision 取快照（digest 自校验，§10.1：损坏即视为不存在）。
    pub fn get(&self, path: &Utf8PathBuf, revision: &str) -> Option<&FileSnapshot> {
        let entry = self.versions.get(path)?;
        entry.iter().find(|snapshot| {
            snapshot.revision == revision && revision_of(&snapshot.raw) == snapshot.revision
        })
    }

    pub fn latest(&self, path: &Utf8PathBuf) -> Option<&FileSnapshot> {
        self.versions.get(path)?.front()
    }

    pub fn clear(&mut self) {
        self.versions.clear();
        self.order.clear();
    }
}

/// read 的窗口输出（§10.2）。
pub struct ReadWindow {
    pub path: Utf8PathBuf,
    pub revision: String,
    pub start_line: usize,
    pub returned_lines: usize,
    pub total_lines: usize,
    pub truncated: bool,
    pub text: String,
}

/// 读取文件窗口（§10.2：正文统一 LF；行号是展示信息）。
pub fn read_window(
    path: &Utf8PathBuf,
    start_line: usize,
    line_count: usize,
) -> Result<ReadWindow, EditError> {
    let snapshot = snapshot_file(path)?;
    let lines: Vec<&str> = snapshot.logical_lf_text.lines().collect();
    let total_lines = lines.len();
    let start = start_line.saturating_sub(1).min(total_lines);
    let end = start + line_count;
    let truncated = end < total_lines;
    let text = lines[start..end.min(total_lines)].join("\n");
    Ok(ReadWindow {
        path: path.clone(),
        revision: snapshot.revision,
        start_line: start + 1,
        returned_lines: end.min(total_lines) - start,
        total_lines,
        truncated,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_round_trip_header_and_bare() {
        let raw = b"fn main() {}\n";
        let revision = revision_of(raw);
        assert!(revision.starts_with("b3:"));
        assert_eq!(revision.len(), 3 + 64);
        let header = format_revision_header(&revision);
        assert_eq!(
            parse_revision_token(&header).as_deref(),
            Some(revision.as_str())
        );
        assert_eq!(
            parse_revision_token(&revision).as_deref(),
            Some(revision.as_str())
        );
        assert_eq!(parse_revision_token("bogus"), None);
    }

    #[test]
    fn snapshot_normalizes_crlf_and_bom() {
        let path = Utf8PathBuf::from("test");
        let raw = b"\xEF\xBB\xBFfn main() {\r\n    work();\r\n}\r\n";
        let snapshot = build_snapshot(path, raw.to_vec()).unwrap();
        assert!(snapshot.has_bom);
        assert_eq!(&*snapshot.logical_lf_text, "fn main() {\n    work();\n}\n");
        assert_eq!(snapshot.line_ending, LineEnding::Crlf);
        // 逻辑偏移 0 → 原始 3（BOM 后）。
        assert_eq!(snapshot.logical_to_raw[0], 3);
    }

    #[test]
    fn edit_rejects_stale_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        std::fs::write(path.as_std_path(), "let x = 1;\n").unwrap();
        let error = apply_edit(
            &path,
            "b3:0000000000000000000000000000000000000000000000000000000000000000",
            &[Replacement {
                old_text: "1".into(),
                new_text: "2".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code(), "stale_revision");
    }

    #[test]
    fn edit_applies_exact_unique_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        let original = "fn old_name() {\n    work();\n}\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let revision = revision_of(original.as_bytes());
        let result = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "fn old_name() {".into(),
                new_text: "fn new_name() {".into(),
            }],
        )
        .unwrap();
        assert_eq!(result.applied, 1);
        commit_edit(&result, &path, &prepare_commit(&path)).unwrap();
        let now = std::fs::read_to_string(path.as_std_path()).unwrap();
        assert_eq!(now, "fn new_name() {\n    work();\n}\n");
    }

    #[test]
    fn edit_rejects_multiple_matches_and_overlap_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        let original = "let a = 1;\nlet b = 1;\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let revision = revision_of(original.as_bytes());
        // 歧义：整批零变化。
        let error = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "= 1".into(),
                new_text: "= 2".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code(), "multiple_matches");
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            original
        );
        // 重叠。
        let error = apply_edit(
            &path,
            &revision,
            &[
                Replacement {
                    old_text: "let a = 1".into(),
                    new_text: "let a = 9".into(),
                },
                Replacement {
                    old_text: "a = 1;".into(),
                    new_text: "a = 8;".into(),
                },
            ],
        )
        .unwrap_err();
        assert_eq!(error.code(), "overlap");
    }

    #[test]
    fn crlf_file_keeps_untouched_bytes_and_encodes_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        let original = "fn main() {\r\n    work();\r\n}\r\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let revision = revision_of(original.as_bytes());
        let result = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "work();".into(),
                new_text: "work();\n    more();".into(),
            }],
        )
        .unwrap();
        commit_edit(&result, &path, &prepare_commit(&path)).unwrap();
        let now = std::fs::read(path.as_std_path()).unwrap();
        assert_eq!(
            now,
            b"fn main() {\r\n    work();\r\n    more();\r\n}\r\n".to_vec()
        );
    }

    #[test]
    fn write_new_file_no_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("new.txt")).unwrap();
        let revision = write_new_file(&path, b"hello\n", &prepare_commit(&path)).unwrap();
        assert!(revision.starts_with("b3:"));
        let error = write_new_file(&path, b"again\n", &prepare_commit(&path)).unwrap_err();
        assert_eq!(error.code(), "already_exists");
        assert_eq!(
            std::fs::read_to_string(path.as_std_path()).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn read_window_reports_lines_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        std::fs::write(path.as_std_path(), "1\n2\n3\n4\n5\n").unwrap();
        let window = read_window(&path, 2, 2).unwrap();
        assert_eq!(window.start_line, 2);
        assert_eq!(window.returned_lines, 2);
        assert_eq!(window.total_lines, 5);
        assert!(window.truncated);
        assert_eq!(window.text, "2\n3");
    }
}

#[cfg(test)]
mod p1_lru_tests {
    use super::*;

    fn snapshot(path: &str, tag: &str) -> (FileSnapshot, String) {
        let logical_lf_text: Arc<str> = Arc::from(format!("content-{tag}"));
        let raw: Arc<[u8]> = Arc::from(logical_lf_text.as_bytes().to_vec());
        let logical_len = logical_lf_text.len();
        let revision = revision_of(&raw);
        (
            FileSnapshot {
                path: Utf8PathBuf::from(path),
                raw,
                logical_lf_text,
                revision: revision.clone(),
                has_bom: false,
                line_ending: LineEnding::Lf,
                // 单调映射：0..=logical_len → 0..=logical_len（LF 文本无转换）。
                logical_to_raw: (0..=logical_len).collect(),
            },
            revision,
        )
    }

    /// P1-7：SnapshotStore 必须是真 LRU——同 path 更新刷新淘汰顺序。
    /// 此前 order 只在首次插入时 push_back，活跃文件会被最早插入者挤掉。
    #[test]
    fn record_refreshes_lru_order_on_path_update() {
        let mut store = SnapshotStore::new(2, 4);
        let (a1, _rev_a1) = snapshot("a.txt", "r1");
        let (b1, rev_b1) = snapshot("b.txt", "r1");
        let (a2, rev_a2) = snapshot("a.txt", "r2");
        let (c1, rev_c1) = snapshot("c.txt", "r1");
        store.record(a1);
        store.record(b1);
        // a.txt 更新（最近使用）。
        store.record(a2);
        // 新 path c.txt：淘汰最久未使用——修复前淘汰 a（插入序），
        // 修复后淘汰 b（LRU：a 最近被刷新）。
        store.record(c1);
        assert!(
            store.get(&Utf8PathBuf::from("a.txt"), &rev_a2).is_some(),
            "最近使用的 a.txt 必须保留（真 LRU）"
        );
        assert!(
            store.get(&Utf8PathBuf::from("b.txt"), &rev_b1).is_none(),
            "最久未使用的 b.txt 应被淘汰"
        );
        assert!(store.get(&Utf8PathBuf::from("c.txt"), &rev_c1).is_some());
    }
}
