//! Revision-bound 可靠编辑与原子提交。
//!
//! - V3 唯一编辑协议：`replacements: [{old_text, new_text}]`——模型只负责
//!   “把这段现有代码变成这段新代码”，不提供行号。old_text 同时承担定位、
//!   precondition、上下文与结构边界（§edit v3）。
//! - 匹配链（每层 0→下一层 / 1→成功 / >1→MultipleMatches 拒绝）：
//!   `Exact → NormalizeEOL → IgnoreTrailingWhitespace → EquivalentIndentation
//!   (UniformOuterIndent) → Fail`。不做 tab↔space 逐字符换算（保守：只允许
//!   整块共同前缀平移；Makefile 等缩进敏感语言禁用）。
//! - CRLF/LF 完全内部化：文件读取时记录原始 EOL，内部统一 LF 匹配，写回时
//!   恢复原 EOL。模型永远用 LF（old_text/new_text 带 `\r` 拒绝，防歧义）。
//! - revision 对原始字节计算 BLAKE3（内部保留）；模型协议使用不透明 `r{id}` 标识符。
//! - stale revision 处理（§修复）：先对**当前文件**做唯一匹配预检——若所有
//!   非 no-op replacement 仍唯一匹配（fmt 等外部空白改动后常见），允许宽松应用
//!   （免重新 read）；否则仍 `stale_revision` 拒绝，并回填当前文件相关区域
//!   内容，模型免 read 即可纠正。
//! - no-op（old_text == new_text）项跳过不整批拒绝（§修复）；全部 no-op 才报错。
//! - `write` 是 revision-bound 整文件写入：新建或提供匹配 revision 的整体重写（§10.6）。
//! - M3：Windows 提交使用 `ReplaceFileW` + 同卷唯一 backup（§10.7）；
//!   成功校验 backup digest，失败/校验不符进入可诊断恢复。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use serde::Deserialize;

pub use tpi_core::outcome::Effect;

/// edit 工具参数（V3：唯一编辑协议 = `old_text → new_text` replacements）。
///
/// 模型不提供行号——`old_text` 同时承担定位、precondition、上下文与结构边界。
/// 插入 = 空 new_text 的 old_text；删除 = 空 new_text；替换 = 两者都非空。
/// batch 语义：全部 replacement 先 resolve 到 base revision 坐标，任一个
/// 失败/重叠则整批拒绝，文件不变（§10.3）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct EditArgs {
    /// 目标文件路径（workspace 内相对路径或绝对路径）。
    pub path: String,
    /// `read`/`search` 输出的 `[revision=r42]` 或裸 `r42` / `b3:<64-hex>`
    /// ——目标快照 identity（所有 old_text 坐标属于此 revision）。
    pub revision: String,
    /// 编辑列表：`old_text → new_text`。每个 old_text 在文件中必须唯一匹配
    /// （经规范化匹配链：Exact → NormalizeEOL → IgnoreTrailingWhitespace →
    /// EquivalentIndentation；>1 匹配拒绝）。
    pub replacements: Vec<Replacement>,
}

/// revision 前缀（§10.1：协议传完整 256 bit digest）。
pub const REVISION_PREFIX: &str = "b3:";

/// 读文件的大小上限（超过视为不可合理快照，返回类型化结果）。
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_REPLACEMENTS: usize = 256;

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

/// 匹配宽容策略（P1/P2：按文件类型决定最高 relaxation level）。
///
/// 原则：relaxation 只发生在 **where**（定位），不传播到 **what**（重建）——
/// 定位用归一化视图，替换仍作用于原始字节区间，未触及字节原样保留。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WhitespacePolicy {
    /// 不允许任何 whitespace tolerance（全部精确匹配）。
    Exact,
    /// 允许 trailing whitespace 归一化定位（行尾空白无语义，安全）。
    TrailingInsensitive,
    /// 允许 uniform outer-indent reconciliation（默认；块整体平移）。
    /// 不做 tab==spaces 换算：只接受**每行共同前缀**的整体偏移。
    UniformOuterIndent,
}

/// 一次 replacement 实际命中的匹配层级（诊断/测试用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchTier {
    Exact,
    TrailingInsensitive,
    UniformOuterIndent,
}

/// 行尾摘要（read 元信息，P0a）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEndingsSummary {
    Lf,
    Crlf,
    Mixed,
}

/// 主导缩进类型（read 元信息，P0a）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentationSummary {
    Tabs,
    Spaces,
    Mixed,
    None_,
}

/// read 输出头的空白元信息（P0a：模型据此构造 old_text，闭合 read→edit 协议）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhitespaceInfo {
    pub line_endings: LineEndingsSummary,
    pub indentation: IndentationSummary,
    /// 展示用 tab 宽度（非规范；仅提示）。
    pub tab_width_display: u8,
    /// 文件是否存在行尾空白。
    pub trailing_whitespace: bool,
}

/// 差异类型（no_match 结构化诊断，P0b）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchKind {
    /// 缩进差异（lstrip 后内容相同）。
    Indentation,
    /// 文本差异（内容本身不同）。
    Textual,
    /// 无法定位（无相似候选）。
    None,
}

/// no_match 结构化诊断（P0b：模型免 read 即可知道差异在哪）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoMatchDiagnostic {
    /// 最相似候选的行范围（1-based）。
    pub lines: (usize, usize),
    /// 相似度（万分比 0..=10000；10000 = 完全相等）。
    pub similarity_bp: u16,
    pub kind: MismatchKind,
    /// 首个差异行（相对候选起点，0-based）。
    pub line: Option<usize>,
    /// 文件实际缩进（old_text 应匹配的目标；转义显示）。
    pub expected_indent: Option<String>,
    /// old_text 提供的缩进（转义显示）。
    pub provided_indent: Option<String>,
    /// 该行 lstrip 后内容是否相等（区分缩进/文本差异）。
    pub content_equal_after_lstrip: Option<bool>,
    /// 文本差异：首个不同行 (old_text 行, 实际行)。
    pub first_difference: Option<(String, String)>,
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
    /// 逻辑文本中由 CRLF 归一化而来的 `\n` 字节偏移。用稀疏偏移表代替
    /// 每字节 `usize` 映射，避免大文件产生数百 MiB 的映射内存。
    crlf_logical_offsets: Vec<usize>,
}

impl FileSnapshot {
    fn raw_offset(&self, logical_offset: usize) -> usize {
        let bom = usize::from(self.has_bom) * 3;
        let removed_crs = self
            .crlf_logical_offsets
            .partition_point(|offset| *offset < logical_offset);
        bom.saturating_add(logical_offset)
            .saturating_add(removed_crs)
    }
}

/// 解析文本文件为快照（§10.1：UTF-8 与 UTF-8 BOM；二进制返回类型化错误）。
pub fn snapshot_file(path: &Utf8PathBuf) -> Result<FileSnapshot, EditError> {
    let raw = read_raw_file(path)?;
    build_snapshot(path.clone(), raw)
}

/// 有界读取写/编辑目标。外部进程可在 freshness 校验前把文件替换为巨型文件，
/// 因此每一次校验读取都必须独立执行同一上限，不能只依赖最初的 metadata。
pub fn read_raw_file(path: &Utf8PathBuf) -> Result<Vec<u8>, EditError> {
    read_raw_path(path.as_std_path(), path)
}

fn read_raw_path(path: &Path, error_path: &Utf8PathBuf) -> Result<Vec<u8>, EditError> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(EditError::NotFound {
                path: error_path.clone(),
            });
        }
        Err(error) => {
            return Err(EditError::Io {
                path: error_path.clone(),
                message: error.to_string(),
            });
        }
    };
    let metadata = file.metadata().map_err(|error| EditError::Io {
        path: error_path.clone(),
        message: error.to_string(),
    })?;
    if metadata.len() > MAX_SNAPSHOT_BYTES as u64 {
        return Err(EditError::FileTooLarge {
            path: error_path.clone(),
            bytes: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
        });
    }
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take((MAX_SNAPSHOT_BYTES as u64).saturating_add(1))
        .read_to_end(&mut raw)
        .map_err(|error| EditError::Io {
            path: error_path.clone(),
            message: error.to_string(),
        })?;
    if raw.len() > MAX_SNAPSHOT_BYTES {
        return Err(EditError::FileTooLarge {
            path: error_path.clone(),
            bytes: raw.len(),
        });
    }
    Ok(raw)
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
    let (logical_lf_text, crlf_logical_offsets, line_ending) = normalize_lf(text);
    let revision = revision_of(&raw);
    Ok(FileSnapshot {
        path,
        raw: Arc::from(raw),
        logical_lf_text: Arc::from(logical_lf_text),
        revision,
        has_bom,
        line_ending,
        crlf_logical_offsets,
    })
}

/// 把 UTF-8 文本归一化为 LF 逻辑文本，同时构建逻辑偏移 → 原始偏移映射。
///
/// BOM 已剥离；`\r\n` 的 `\r` 不进入逻辑文本（§10.1：`logical_lf_text` 去 BOM 并统一 `\n`）。
fn normalize_lf(text: &str) -> (String, Vec<usize>, LineEnding) {
    let mut logical = String::with_capacity(text.len());
    let mut crlf_logical_offsets = Vec::new();
    let mut crlf_count = 0usize;
    let mut lf_count = 0usize;
    // 按字符迭代（UTF-8 安全）：此前按字节 `push(b as char)` 会把多字节
    // 字符拆成 Latin-1 乱码（中文/emoji 文件 read/edit 全部损坏）。
    let mut chars = text.char_indices().peekable();
    while let Some((_i, ch)) = chars.next() {
        if ch == '\r'
            && let Some(&(_, '\n')) = chars.peek()
        {
            chars.next(); // 消费 \n
            crlf_count += 1;
            crlf_logical_offsets.push(logical.len());
            logical.push('\n');
            continue;
        }
        if ch == '\n' {
            lf_count += 1;
        }
        logical.push(ch);
    }
    let line_ending = if crlf_count > lf_count {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    (logical, crlf_logical_offsets, line_ending)
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
        /// stale 且 replacement 不再唯一匹配时，当前文件相关区域上下文
        /// （模型免 read 自纠；§修复）。
        context: Option<String>,
    },
    #[error("invalid revision: {value}")]
    InvalidRevision { value: String },
    #[error(
        "invalid line range in {path}: {start_line}..={end_line} (must be 1-indexed, end >= start)"
    )]
    InvalidRange {
        path: Utf8PathBuf,
        start_line: usize,
        end_line: usize,
    },
    #[error("old_text must not be empty (path={path})")]
    EmptyOldText { path: Utf8PathBuf },
    #[error("too many replacements in {path}: {count} (max {MAX_REPLACEMENTS})")]
    TooManyReplacements { path: Utf8PathBuf, count: usize },
    #[error("both operations and legacy replacements provided for {path} (choose one)")]
    BothOperationsAndReplacements { path: Utf8PathBuf },
    #[error("no match for replacements[{index}] in {path}")]
    NoMatch {
        path: Utf8PathBuf,
        index: usize,
        /// 当前文件相关区域上下文（模型免 read 自纠；§修复）。
        context: Option<String>,
        /// 结构化差异诊断（P0b；None = 无相似候选）。
        /// Box：诊断仅在错误路径构造，避免撑大 NoMatch variant（result_large_err）。
        diagnostic: Option<Box<NoMatchDiagnostic>>,
    },
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
    #[error("all replacements are no-ops (old_text == new_text) in {path}")]
    AllNoOps { path: Utf8PathBuf },
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
            EditError::InvalidRange { .. } => "invalid_range",
            EditError::EmptyOldText { .. } => "empty_old_text",
            EditError::TooManyReplacements { .. } => "too_many_replacements",
            EditError::BothOperationsAndReplacements { .. } => "both_operations_and_replacements",
            EditError::NoMatch { .. } => "no_match",
            EditError::MultipleMatches { .. } => "multiple_matches",
            EditError::Overlap { .. } => "overlap",
            EditError::NoOp { .. } | EditError::AllNoOps { .. } => "no_op",
            EditError::NonCanonicalLineEndings { .. } => "non_canonical_line_endings",
            EditError::AlreadyExists { .. } => "already_exists",
            EditError::CommitFailed { .. } => "commit_failed",
            EditError::ConcurrentModification { .. } => "concurrent_modification_during_commit",
            EditError::CommitRecoveryFailed { .. } => "commit_recovery_failed",
            EditError::Io { .. } => "io_error",
        }
    }
}

/// 一次 replacement（V3 唯一编辑协议：old_text → new_text）。
/// old_text 同时承担定位、precondition、上下文与结构边界（§edit v3）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct Replacement {
    pub old_text: String,
    pub new_text: String,
}

/// 解析后的编辑片段（内部统一 primitive；全部坐标是 base snapshot 的
/// **逻辑字节**偏移，`apply_edit_to_snapshot` 消费）。
pub struct ResolvedSplice {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// 预检通过的 replacement（逻辑坐标已解析）。
#[derive(Debug, Clone)]
struct PreparedReplacement {
    index: usize,
    logical_start: usize,
    logical_end: usize,
    new_logical: String,
    /// 命中的匹配层级（P1/P2 宽容定位时非 Exact）。
    tier: MatchTier,
}

/// 应用 replacement 后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditResult {
    pub previous_revision: String,
    pub current_revision: String,
    /// 实际应用的 replacement 数（不含跳过的 no-op）。
    pub applied: usize,
    /// 因 old_text == new_text 而跳过的 no-op 条目数（§修复）。
    pub skipped_noops: usize,
    /// 本批所有 replacement 命中的最低匹配层级（P1/P2；全部 exact 为 Exact）。
    pub tier: MatchTier,
    /// 编辑前原始字节（unified diff 与诊断用）。
    pub previous_raw: Vec<u8>,
    pub new_raw: Vec<u8>,
}

/// 生成 unified diff（§10.3 第 10 条；仅用于展示与验证，不用于猜测式修改）。
pub fn unified_diff(result: &EditResult) -> String {
    const MAX_DIFF_INPUT_BYTES: usize = 4 * 1024 * 1024;
    const MAX_DIFF_OUTPUT_BYTES: usize = 256 * 1024;
    if result
        .previous_raw
        .len()
        .saturating_add(result.new_raw.len())
        > MAX_DIFF_INPUT_BYTES
    {
        return format!(
            "[diff omitted: input exceeds {} bytes; revisions {} -> {}]",
            MAX_DIFF_INPUT_BYTES, result.previous_revision, result.current_revision
        );
    }
    use similar::TextDiff;
    let old_text = String::from_utf8_lossy(&result.previous_raw);
    let new_text = String::from_utf8_lossy(&result.new_raw);
    if old_text == new_text {
        return String::new();
    }
    let diff = TextDiff::from_lines(&old_text, &new_text);
    let mut out = diff
        .unified_diff()
        .context_radius(3)
        .header("before", "after")
        .to_string();
    if out.len() > MAX_DIFF_OUTPUT_BYTES {
        tpi_core::util::truncate_to_char_boundary(&mut out, MAX_DIFF_OUTPUT_BYTES);
        out.push_str("\n[diff truncated]\n");
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
        let match_start = start + offset;
        let advance = haystack[match_start..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        start = match_start.saturating_add(advance);
    }
    count
}

/// 文件类型 → 最高宽容层级（P2b）。Makefile 的 `\t` 是语法：即使视觉
/// 列数相同，tab 与空格也不等价 → 禁用 uniform-indent（只允许 trailing）。
pub fn whitespace_policy_for(path: &Utf8PathBuf) -> WhitespacePolicy {
    let name = path
        .file_name()
        .map(|n| n.to_lowercase())
        .unwrap_or_default();
    let is_makefile = matches!(name.as_str(), "makefile" | "gnumakefile") || name.ends_with(".mk");
    if is_makefile {
        WhitespacePolicy::TrailingInsensitive
    } else {
        WhitespacePolicy::UniformOuterIndent
    }
}

/// 行边界：[start, end)，end 含换行符（`\n`）位置；不含则到文本末尾。
/// 末尾 `\n` 后的空段不产生行（与 `str::lines` 一致）。
fn line_bounds(text: &str) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            bounds.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < text.len() {
        bounds.push((start, text.len()));
    }
    bounds
}

/// 去掉行尾空白与换行符（比较基准：行内容本身）。
/// 注意：old_text 末行可能无换行，而对应实际行有换行——比较必须统一去掉。
fn trim_trailing_ws(line: &str) -> &str {
    line.trim_end_matches([' ', '\t', '\n'])
}

/// Tier 2：trailing-whitespace 归一化定位。两侧每行去行尾空白后逐行相等；
/// 返回命中的**原始** logical 区间（含真实 trailing——未触及字节保留）。
/// 唯一命中才返回；歧义（多个位置）返回 None（安全失败，不自动替换）。
fn locate_trailing_insensitive(haystack: &str, old_text: &str) -> Option<(usize, usize)> {
    let old_lines = line_bounds(old_text);
    if old_lines.is_empty() {
        return None;
    }
    let old_norm: Vec<&str> = old_lines
        .iter()
        .map(|&(s, e)| trim_trailing_ws(&old_text[s..e]))
        .collect();
    let hay_lines = line_bounds(haystack);
    if hay_lines.len() < old_lines.len() {
        return None;
    }
    // ISSUE-003：全窗口 O(N×L) 扫描在百万行文件上可达数十秒；宽容定位只需
    // 在邻近区域找到唯一命中，限定窗口上限（找不到时安全返回 None，由
    // diagnose_no_match 给出结构化诊断）。
    const MAX_LOCATE_WINDOWS: usize = 20_000;
    let mut hits = Vec::new();
    for win in 0..=(hay_lines.len() - old_lines.len()).min(MAX_LOCATE_WINDOWS) {
        let mut ok = true;
        for i in 0..old_lines.len() {
            let (hs, he) = hay_lines[win + i];
            let actual = trim_trailing_ws(&haystack[hs..he]);
            if actual != old_norm[i] {
                ok = false;
                break;
            }
        }
        if ok {
            let last = win + old_lines.len() - 1;
            hits.push((hay_lines[win].0, hay_lines[last].1));
        }
    }
    if hits.len() == 1 { Some(hits[0]) } else { None }
}

/// Tier 3：uniform outer-indent reconciliation。要求每行 lstrip 后内容相等，
/// 且实际行缩进 = 统一前缀 P + old_text 行缩进（P 对全部**非空行**相同；
/// 字符级 strip_suffix，不做 tab==space 换算）。返回 (命中区间, P)。
///
/// 安全边界：只接受“块整体平移”（坐标系偏移），relative indentation 变化
/// （如 Python 嵌套层级）直接失败；Makefile 等由 policy 禁用本层。
fn locate_uniform_indent(haystack: &str, old_text: &str) -> Option<(usize, usize, String)> {
    let old_lines = line_bounds(old_text);
    if old_lines.is_empty() {
        return None;
    }
    let mut old_rows = Vec::with_capacity(old_lines.len());
    for &(s, e) in &old_lines {
        let line = &old_text[s..e];
        let content_start = line.len() - line.trim_start_matches([' ', '\t']).len();
        let (indent, content) = line.split_at(content_start);
        old_rows.push((indent, content));
    }
    let hay_lines = line_bounds(haystack);
    if hay_lines.len() < old_lines.len() {
        return None;
    }
    // ISSUE-003：全窗口 O(N×L) 扫描在百万行文件上可达数十秒；宽容定位只需
    // 在邻近区域找到唯一命中，限定窗口上限（找不到时安全返回 None）。
    const MAX_LOCATE_WINDOWS: usize = 20_000;
    let mut hits = Vec::new();
    'win: for win in 0..=(hay_lines.len() - old_lines.len()).min(MAX_LOCATE_WINDOWS) {
        let mut prefix: Option<String> = None;
        for i in 0..old_lines.len() {
            let (hs, he) = hay_lines[win + i];
            let hay_line = &haystack[hs..he];
            let h_content_start = hay_line.len() - hay_line.trim_start_matches([' ', '\t']).len();
            let (h_indent, h_content) = hay_line.split_at(h_content_start);
            let (o_indent, o_content) = old_rows[i];
            // 内容（lstrip + 去行尾空白/换行）必须逐行相等。
            let h_core = h_content.trim_end_matches([' ', '\t', '\n']);
            let o_core = o_content.trim_end_matches([' ', '\t', '\n']);
            if h_core != o_core {
                continue 'win;
            }
            // 纯空白行不参与前缀推导/校验（indent 无语义）。
            if o_core.trim().is_empty() {
                continue;
            }
            match h_indent.strip_suffix(o_indent) {
                Some(p) => match &prefix {
                    None => prefix = Some(p.to_string()),
                    Some(existing) if existing == p => {}
                    Some(_) => continue 'win,
                },
                None => continue 'win,
            }
        }
        if let Some(p) = prefix {
            let last = win + old_lines.len() - 1;
            hits.push((hay_lines[win].0, hay_lines[last].1, p));
        }
    }
    if hits.len() == 1 {
        Some(hits.swap_remove(0))
    } else {
        None
    }
}

/// Tier 3：把 new_text 每行（非空行）加上统一前缀 P（重建缩进坐标系）。
/// 空行/纯空白行不加，避免在空白行制造无意义空白。
fn reindent_new_text(new_text: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return new_text.to_string();
    }
    let mut out = String::with_capacity(new_text.len() + prefix.len());
    for line in new_text.split_inclusive('\n') {
        if line.trim().is_empty() {
            out.push_str(line);
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
    }
    out
}

/// 行内容 core：去掉行尾空白与换行（保留行首 indent；用于“未变化行”判定）。
fn line_core(line: &str) -> &str {
    line.trim_end_matches([' ', '\t', '\n'])
}

/// new_text 与 old_text 的缩进相对结构一致性（Tier3 前置校验）：
/// 对所有非空行，new_indent_i 与 old_indent_i 存在**恒定整体偏移**（任一方向：
/// new = Q + old 或 old = Q + new，Q 对全部行相同）。防止模型在 new_text 里
/// 改变 relative indentation（如 Python 嵌套层级写平）后被静默重建。
fn new_text_relative_consistent(old_text: &str, new_text: &str) -> bool {
    let old_lines = line_bounds(old_text);
    let new_lines = line_bounds(new_text);
    if old_lines.len() != new_lines.len() {
        // 行数不同：无法逐行对齐，交由 reconcile 的 fallback（整体重建）。
        return true;
    }
    let mut expect: Option<(String, bool)> = None; // (前缀, true=new=Q+old)
    for i in 0..old_lines.len() {
        let o = &old_text[old_lines[i].0..old_lines[i].1];
        let n = &new_text[new_lines[i].0..new_lines[i].1];
        let o_core = line_core(o);
        if o_core.trim().is_empty() {
            continue;
        }
        let o_indent = &o[..o.len() - o.trim_start_matches([' ', '\t']).len()];
        let n_core = line_core(n);
        if n_core.trim().is_empty() {
            continue;
        }
        let n_indent = &n[..n.len() - n.trim_start_matches([' ', '\t']).len()];
        // new = Q + old，或 old = Q + new（Q 恒定）。
        let forward = n_indent
            .strip_suffix(o_indent)
            .map(|q| (q.to_string(), true));
        let backward = o_indent
            .strip_suffix(n_indent)
            .map(|q| (q.to_string(), false));
        let this = forward.or(backward);
        match (&expect, this) {
            (None, Some(x)) => expect = Some(x),
            (Some((eq, ed)), Some((q, d))) if *eq == q && *ed == d => {}
            _ => return false,
        }
    }
    true
}

/// 合并未变化行（P1 核心原则：match normalized view, mutate original view）。
///
/// new_text 中与 old_text 内容（core）相同的行——即 patch 的 context 行——
/// 保持**实际文件字节**（含真实 trailing whitespace；Tier3 时含统一前缀 P），
/// 只替换真正变化的行。避免宽容定位把 context 行的真实空白写丢（Codex
/// issue #30505 类问题）。行数不一致时无法逐行对齐 → 整体用 new_text。
fn reconcile_lines(old_text: &str, new_text: &str, actual: &str, prefix: &str) -> String {
    let old_lines = line_bounds(old_text);
    let new_lines = line_bounds(new_text);
    let act_lines = line_bounds(actual);
    if old_lines.len() != new_lines.len() || new_lines.len() != act_lines.len() {
        return reindent_new_text(new_text, prefix);
    }
    let mut out = String::with_capacity(actual.len() + new_text.len());
    for i in 0..old_lines.len() {
        let o = &old_text[old_lines[i].0..old_lines[i].1];
        let n = &new_text[new_lines[i].0..new_lines[i].1];
        if line_core(o) == line_core(n) {
            // 未变化行：保持实际字节（含真实 trailing / Tier3 前缀）。
            out.push_str(&actual[act_lines[i].0..act_lines[i].1]);
        } else {
            out.push_str(prefix);
            out.push_str(n);
            // 末行：new_text 无换行但实际行有换行 → 补（模型未写 \n 不意味
            // 着要删除该行的换行）。
            if i == old_lines.len() - 1
                && !n.ends_with('\n')
                && actual[act_lines[i].0..act_lines[i].1].ends_with('\n')
            {
                out.push('\n');
            }
        }
    }
    out
}

/// 宽容定位（Tier 2/3 分发）。返回 (logical_start, logical_end, new_logical, tier)。
/// 只改变“命中哪几行”；new_logical 仅 Tier3 按真实前缀重建，Tier2 原样。
fn locate_lenient(
    text: &str,
    replacement: &Replacement,
    policy: WhitespacePolicy,
) -> Option<(usize, usize, String, MatchTier)> {
    if policy >= WhitespacePolicy::TrailingInsensitive
        && let Some((start, end)) = locate_trailing_insensitive(text, &replacement.old_text)
    {
        let actual = &text[start..end];
        let merged = reconcile_lines(&replacement.old_text, &replacement.new_text, actual, "");
        return Some((start, end, merged, MatchTier::TrailingInsensitive));
    }
    if policy >= WhitespacePolicy::UniformOuterIndent
        && let Some((start, end, prefix)) = locate_uniform_indent(text, &replacement.old_text)
    {
        // new_text 内部 relative indentation 变化 → 拒绝（防静默写错层级）。
        if !new_text_relative_consistent(&replacement.old_text, &replacement.new_text) {
            return None;
        }
        let actual = &text[start..end];
        let merged = reconcile_lines(
            &replacement.old_text,
            &replacement.new_text,
            actual,
            &prefix,
        );
        return Some((start, end, merged, MatchTier::UniformOuterIndent));
    }
    None
}

/// 分析文件的空白概况（P0a：read 输出头元信息，模型据此构造 old_text）。
pub fn analyze_whitespace(snapshot: &FileSnapshot) -> WhitespaceInfo {
    // 行尾：扫描原始字节统计 CRLF 与孤立 LF（raw 保留真实行尾）。
    let raw = &snapshot.raw;
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'\n' {
            if i > 0 && raw[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
        i += 1;
    }
    let line_endings = match (crlf, lf) {
        (0, _) => LineEndingsSummary::Lf,
        (_, 0) => LineEndingsSummary::Crlf,
        _ => LineEndingsSummary::Mixed,
    };
    // 缩进与 trailing：基于 logical 文本逐行。
    let mut tabs = 0usize;
    let mut spaces = 0usize;
    let mut mixed = 0usize;
    let mut trailing = false;
    for line in snapshot.logical_lf_text.split('\n') {
        if line.ends_with(' ') || line.ends_with('\t') {
            trailing = true;
        }
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.len() == line.len() {
            continue;
        }
        let indent = &line[..line.len() - trimmed.len()];
        let has_tab = indent.contains('\t');
        let has_space = indent.contains(' ');
        match (has_tab, has_space) {
            (true, false) => tabs += 1,
            (false, true) => spaces += 1,
            _ => mixed += 1,
        }
    }
    let indentation = if tabs > 0 && spaces == 0 && mixed == 0 {
        IndentationSummary::Tabs
    } else if spaces > 0 && tabs == 0 && mixed == 0 {
        IndentationSummary::Spaces
    } else if tabs > 0 || spaces > 0 || mixed > 0 {
        IndentationSummary::Mixed
    } else {
        IndentationSummary::None_
    };
    WhitespaceInfo {
        line_endings,
        indentation,
        tab_width_display: 4,
        trailing_whitespace: trailing,
    }
}

/// 缩进转义显示（tab → `\t`，可 round-trip 复制）。
fn escape_ws(s: &str) -> String {
    format!("{s:?}")
}

/// no_match 结构化诊断（P0b）：定位最相似候选窗口，标注首个差异（缩进/文本）。
pub fn diagnose_no_match(text: &str, old_text: &str) -> Option<NoMatchDiagnostic> {
    let old_lines = line_bounds(old_text);
    let hay_lines = line_bounds(text);
    if old_lines.is_empty() || hay_lines.is_empty() {
        return None;
    }
    // ISSUE-003：扫描复杂度是 O(窗口数 × old 行数)，每次比较又是 O(行长)。
    // 大文件（≤64MiB / 百万行）上可能数秒到数十秒。诊断只需近似定位——
    // 限定参与扫描的行数（窗口与 old 行都设上限），超出后仍返回近似结果。
    const MAX_DIAGNOSE_WINDOWS: usize = 20_000;
    const MAX_DIAGNOSE_OLD_LINES: usize = 200;
    let old_lines = &old_lines[..old_lines.len().min(MAX_DIAGNOSE_OLD_LINES)];
    let old_rows: Vec<(&str, &str, &str)> = old_lines
        .iter()
        .map(|&(s, e)| {
            let line = &old_text[s..e];
            let content_start = line.len() - line.trim_start_matches([' ', '\t']).len();
            let (indent, content) = line.split_at(content_start);
            (indent, content.trim(), line)
        })
        .collect();
    let max_windows =
        (hay_lines.len().saturating_sub(old_lines.len()) + 1).min(MAX_DIAGNOSE_WINDOWS);
    let mut best: Option<(usize, f64, usize)> = None; // (win, similarity, first_diff)
    for win in 0..max_windows {
        let mut same = 0usize;
        let mut first_diff = None;
        for i in 0..old_lines.len() {
            let (hs, he) = hay_lines[win + i];
            let hay_line = &text[hs..he];
            let h_content_start = hay_line.len() - hay_line.trim_start_matches([' ', '\t']).len();
            let (h_indent, h_content) = hay_line.split_at(h_content_start);
            let (o_indent, o_content, _) = old_rows[i];
            // 相似度按“内容（lstrip+去尾空白/换行）相等”计，缩进差异不降分
            //（整体 outer-indent 偏移是常见场景，应给出高相似度诊断），
            // 但缩进不同的行记入 first_diff 供标注。
            let h_core = h_content.trim_end_matches([' ', '\t', '\n']);
            let o_core = o_content.trim_end_matches([' ', '\t', '\n']);
            if h_core == o_core {
                same += 1;
                if h_indent != o_indent && first_diff.is_none() {
                    first_diff = Some(i);
                }
            } else if first_diff.is_none() {
                first_diff = Some(i);
            }
        }
        let sim = same as f64 / old_lines.len() as f64;
        if best
            .as_ref()
            .map(|(_, bsim, _)| sim > *bsim)
            .unwrap_or(true)
        {
            best = Some((win, sim, first_diff.unwrap_or(0)));
        }
    }
    let (win, similarity, first_diff) = best?;
    let mut kind = MismatchKind::None;
    let mut expected_indent = None;
    let mut provided_indent = None;
    let mut content_equal = None;
    let mut first_difference = None;
    if let Some((o_indent, o_content, _)) = old_rows.get(first_diff) {
        let (hs, he) = hay_lines[win + first_diff];
        let hay_line = &text[hs..he];
        let h_content_start = hay_line.len() - hay_line.trim_start_matches([' ', '\t']).len();
        let (h_indent, h_content) = hay_line.split_at(h_content_start);
        let h_core = h_content.trim_end_matches([' ', '\t', '\n']);
        let o_core = o_content.trim_end_matches([' ', '\t', '\n']);
        if h_core == o_core {
            kind = MismatchKind::Indentation;
            // expected = 文件实际（old_text 应匹配的目标）；provided = old_text 提供。
            expected_indent = Some(escape_ws(h_indent));
            provided_indent = Some(escape_ws(o_indent));
            content_equal = Some(true);
        } else {
            kind = MismatchKind::Textual;
            let (os, oe) = old_lines[first_diff];
            // ISSUE-003：单行可达 64MiB，差异行只存有界片段（char 边界安全）。
            const DIFF_LINE_BUDGET: usize = 200;
            let provided: String = old_text[os..oe].chars().take(DIFF_LINE_BUDGET).collect();
            let actual: String = hay_line.chars().take(DIFF_LINE_BUDGET).collect();
            first_difference = Some((provided, actual));
        }
    }
    Some(NoMatchDiagnostic {
        lines: (win + 1, win + old_lines.len()),
        similarity_bp: ((similarity * 10000.0).round() as u16).min(10000),
        kind,
        line: Some(first_diff),
        expected_indent,
        provided_indent,
        content_equal_after_lstrip: content_equal,
        first_difference,
    })
}

/// 流程：读快照 → revision 校验 → 全部 replacements 预检（存在/唯一/不重叠/no-op）
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

/// 纯内存版 apply（R2：远端 edit 复用同一 semantic contract，§41/§42）。
///
/// 基于原始字节构建 snapshot（`build_snapshot`）后应用 replacement，
/// 不触碰本地磁盘；revision 校验、原子批、diff 语义与本地 `apply_edit`
/// 完全一致。远端提交由 SFTP temp+rename 完成。
#[allow(dead_code)] // 远端工具接线后移除
pub fn apply_edit_bytes(
    path: &Utf8PathBuf,
    raw: Vec<u8>,
    revision: &str,
    replacements: &[Replacement],
) -> Result<EditResult, EditError> {
    let snapshot = build_snapshot(path.clone(), raw)?;
    apply_edit_to_snapshot(snapshot, revision, replacements)
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
    if replacements.len() > MAX_REPLACEMENTS {
        return Err(EditError::TooManyReplacements {
            path: snapshot.path.clone(),
            count: replacements.len(),
        });
    }

    // 预检（§10.3 第 1-5 条）：基于当前文件内容，不依赖 revision。
    // §修复 #4：no-op（old_text == new_text）项跳过不整批拒绝；其余逐条校验，
    // 记录第一个失败的定位诊断（原子性：任一条失败即整体拒绝）。
    let mut prepared: Vec<PreparedReplacement> = Vec::with_capacity(replacements.len());
    let mut skipped_noops = 0usize;
    let mut precheck_error: Option<EditError> = None;
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
        // §修复 #4：no-op 跳过（不整批拒绝）。
        if replacement.old_text == replacement.new_text {
            skipped_noops += 1;
            continue;
        }
        // Tier 1：exact。歧义（>1）直接拒绝，不降级——安全失败优先于
        // 模糊命中的“危险成功”（P1 原则：relaxation 只发生在 where）。
        let occurrences = count_occurrences(&snapshot.logical_lf_text, &replacement.old_text);
        if occurrences > 1 {
            precheck_error = Some(EditError::MultipleMatches {
                path: snapshot.path.clone(),
                index,
            });
            break;
        }
        // 定位结果：(start, end, new_logical, tier)。宽容定位只改变“命中
        // 哪几行”，new_logical 仅在 Tier3 按实际前缀重建（统一前缀平移）。
        let (logical_start, logical_end, new_logical, tier) = if occurrences == 1 {
            let logical_start = match snapshot.logical_lf_text.find(&replacement.old_text) {
                Some(start) => start,
                None => {
                    tracing::error!(
                        path = %snapshot.path,
                        index,
                        "edit: count_occurrences 与 find 不一致（内部不变量破坏）",
                    );
                    precheck_error = Some(EditError::NoMatch {
                        path: snapshot.path.clone(),
                        index,
                        context: None,
                        diagnostic: None,
                    });
                    break;
                }
            };
            (
                logical_start,
                logical_start + replacement.old_text.len(),
                replacement.new_text.clone(),
                MatchTier::Exact,
            )
        } else {
            // Tier 2/3：宽容定位（按文件类型 policy；只定位不重建）。
            match locate_lenient(
                &snapshot.logical_lf_text,
                replacement,
                whitespace_policy_for(&snapshot.path),
            ) {
                Some(found) => found,
                None => {
                    precheck_error = Some(EditError::NoMatch {
                        path: snapshot.path.clone(),
                        index,
                        // §修复 #3：定位失败处当前文件内容，模型免 read 自纠。
                        context: locate_context(&snapshot.logical_lf_text, &replacement.old_text),
                        // P0b：结构化诊断（缩进/文本差异逐项标注）。
                        diagnostic: diagnose_no_match(
                            &snapshot.logical_lf_text,
                            &replacement.old_text,
                        )
                        .map(Box::new),
                    });
                    break;
                }
            }
        };
        prepared.push(PreparedReplacement {
            index,
            logical_start,
            logical_end,
            new_logical,
            tier,
        });
    }

    // revision 判定：
    // - 预检失败（NoMatch/MultipleMatches）优先报告；stale 时归并为
    //   StaleRevision（§修复 #2：回填当前区域上下文），非 stale 报原错误。
    // - stale 且所有非 no-op 项仍唯一匹配（§修复 #1：fmt 等外部空白改动后
    //   常见）→ 宽松应用（基于当前内容，免重新 read）。
    if let Some(error) = precheck_error {
        if snapshot.revision != expected_revision {
            let context = match &error {
                EditError::NoMatch { context, .. } => context.clone(),
                _ => None,
            };
            return Err(EditError::StaleRevision {
                path: snapshot.path.clone(),
                current: snapshot.revision,
                expected: expected_revision.to_string(),
                context,
            });
        }
        return Err(error);
    }
    if snapshot.revision != expected_revision {
        tracing::info!(
            path = %snapshot.path,
            "edit: revision stale but all replacements match current content; applying (lenient unique match)",
        );
    }
    // §修复 #4：无预检错误但没有任何可应用项（全部是 no-op）→ 明确拒绝。
    if prepared.is_empty() {
        return Err(EditError::AllNoOps {
            path: snapshot.path.clone(),
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

    // 在分配/拼接新缓冲区前计算精确原始字节大小（含 CRLF 扩张）。
    let mut projected_size = snapshot.raw.len();
    for replacement in &sorted {
        let raw_start = snapshot.raw_offset(replacement.logical_start);
        let raw_end = snapshot.raw_offset(replacement.logical_end);
        let encoded_len = encode_replacement(
            &replacement.new_logical,
            raw_start,
            &snapshot.raw,
            snapshot.line_ending,
            snapshot.has_bom,
        )
        .len();
        projected_size = projected_size
            .checked_sub(raw_end - raw_start)
            .and_then(|size| size.checked_add(encoded_len))
            .ok_or_else(|| EditError::FileTooLarge {
                path: snapshot.path.clone(),
                bytes: usize::MAX,
            })?;
    }
    if projected_size > MAX_SNAPSHOT_BYTES {
        return Err(EditError::FileTooLarge {
            path: snapshot.path.clone(),
            bytes: projected_size,
        });
    }

    // 一次性应用：从后往前替换（保持前缀偏移有效）。
    let mut new_raw = snapshot.raw.to_vec();
    let has_bom = snapshot.has_bom;
    let line_ending = snapshot.line_ending;
    for r in sorted.iter().rev() {
        let raw_start = snapshot.raw_offset(r.logical_start);
        let raw_end = snapshot.raw_offset(r.logical_end);
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
        skipped_noops,
        // 本批命中层级 = 最低（最大）tier；全部 exact 为 Exact。
        tier: prepared
            .iter()
            .map(|p| p.tier)
            .max()
            .unwrap_or(MatchTier::Exact),
        previous_raw: snapshot.raw.to_vec(),
        new_raw,
    })
}

/// 在文本中定位 `old_text` 最接近出现处的上下文（§修复 #2/#3：失败时回填
/// 当前文件相关区域，模型免 read 即可自纠）。
///
/// 策略：精确匹配优先；否则按行找与 old_text 首行最相似的行，取该行前后
/// 各 2 行、带行号、有界（≤400 字符）；找不到则返回文件头几行。
fn locate_context(text: &str, old_text: &str) -> Option<String> {
    const RADIUS_LINES: usize = 2;
    const MAX_CTX_CHARS: usize = 400;

    let first_line = old_text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return None;
    }
    // 精确匹配 old_text 的起始行号。
    let exact_line = text.lines().position(|line| line.contains(old_text));
    // 找不到精确匹配时，用首行做行内子串定位（容缩进/上下文差异）。
    let target_line =
        exact_line.or_else(|| text.lines().position(|line| line.contains(first_line)));
    let line_no = match target_line {
        Some(i) => i + 1,
        None => 1,
    };
    // 0-based 起始行（覆盖前 RADIUS 行；saturating 防 line_no=1 时下溢）。
    let start_line_0 = line_no.saturating_sub(RADIUS_LINES + 1);
    let mut snippet = String::new();
    for (offset, line) in text
        .lines()
        .skip(start_line_0)
        .take(RADIUS_LINES * 2 + 1)
        .enumerate()
    {
        let no = start_line_0 + offset + 1;
        // ISSUE-003：单行可达 MAX_SNAPSHOT_BYTES（64MiB），整行 append 会把
        // 诊断输出撑到数 MiB 进模型上下文；先按行预算截断（char 边界安全）。
        const LINE_BUDGET: usize = 120;
        let display: String = line.chars().take(LINE_BUDGET).collect();
        if no == line_no {
            snippet.push_str(&format!(">> {no}: {display}\n"));
        } else {
            snippet.push_str(&format!("   {no}: {display}\n"));
        }
        if snippet.len() > MAX_CTX_CHARS {
            snippet.push_str("...[truncated]\n");
            break;
        }
    }
    // 找不到任何定位（空文件/全空行）：回退文件头几行。
    if snippet.is_empty() {
        for (offset, line) in text.lines().take(RADIUS_LINES * 2 + 1).enumerate() {
            const LINE_BUDGET: usize = 120;
            let display: String = line.chars().take(LINE_BUDGET).collect();
            snippet.push_str(&format!("   {}: {display}\n", offset + 1));
            if snippet.len() > MAX_CTX_CHARS {
                break;
            }
        }
    }
    Some(snippet)
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
        // SAFETY: Every non-null pointer references a live, NUL-terminated UTF-16
        // buffer for the duration of the call. The two reserved arguments are null.
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
        if let Some(backup) = backup_path {
            std::fs::hard_link(target, backup)?;
        }
        if let Err(error) = std::fs::rename(temp_path, target) {
            if let Some(backup) = backup_path {
                let _ = std::fs::remove_file(backup);
            }
            return Err(error);
        }
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
        // SAFETY: Both pointers reference live, NUL-terminated UTF-16 buffers
        // for the duration of this synchronous call.
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
        match std::fs::hard_link(temp_path, target) {
            Ok(()) => {
                if let Err(error) = std::fs::remove_file(temp_path) {
                    tracing::warn!(%error, path = %temp_path.display(), "failed to clean installed temp link");
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(InstallError::AlreadyExists)
            }
            Err(error) => Err(InstallError::Io(error.to_string())),
        }
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
    let current = revision_of(&read_raw_file(path)?);
    if current != result.previous_revision {
        let _ = std::fs::remove_file(temp_path);
        return Err(EditError::StaleRevision {
            path: path.clone(),
            current,
            expected: result.previous_revision.clone(),
            // commit 阶段的竞态校验：无 replacement 可定位，回填 None。
            context: None,
        });
    }

    // §10.7 第 3 步：ReplaceFileW(target, temp, backup)。
    let backup_path = plan.backup_path.clone();
    if let Err(error) = replace_file(temp_path, path.as_std_path(), backup_path.as_deref()) {
        // §10.7 第 5 步：API 失败 → 根据 target/temp/backup 的存在性和 digest 恢复。
        let recovered = recover_after_failure(
            path,
            &result.new_raw,
            &backup_path,
            &result.previous_revision,
            error,
        );
        // ISSUE-013：`recover_after_failure` 返回 Ok 意味着 target 已是新内容
        // （提交实际成功），temp 已无价值必须清理；CommitFailed 同理。
        // CommitRecoveryFailed 保留 temp 是刻意证据设计（无法证明恢复完成）。
        if recovered.is_ok() || matches!(recovered, Err(EditError::CommitFailed { .. })) {
            let _ = std::fs::remove_file(temp_path);
        }
        return recovered;
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
    let target_now = revision_of(&read_raw_file(path)?);
    if target_now != candidate_digest {
        // 提交后 target 不是 candidate：外部并发修改（§10.4 竞态窗口）。
        restore_target(path, backup_path, "target != candidate")?;
        return Err(EditError::ConcurrentModification { path: path.clone() });
    }
    if let Some(backup) = backup_path {
        if !backup.exists() {
            return Err(EditError::CommitRecoveryFailed {
                path: path.clone(),
                message: "replacement succeeded but backup is missing".into(),
            });
        }
        let backup_digest = revision_of(&read_raw_path(backup, path)?);
        if backup_digest != expected_revision {
            // §10.4：backup 已包含未预期的外部变化 → 恢复并返回并发诊断。
            restore_target(path, backup_path, "backup digest mismatch")?;
            return Err(EditError::ConcurrentModification { path: path.clone() });
        }
    }
    Ok(())
}

/// 用 backup 恢复 target（§10.7 第 5 步：API 失败或校验不符时的恢复流程）。
fn restore_target(
    path: &Utf8PathBuf,
    backup_path: &Option<std::path::PathBuf>,
    reason: &str,
) -> Result<(), EditError> {
    let Some(backup) = backup_path.as_ref().filter(|backup| backup.exists()) else {
        return Err(EditError::CommitRecoveryFailed {
            path: path.clone(),
            message: format!("backup unavailable while restoring after {reason}"),
        });
    };
    std::fs::copy(backup, path.as_std_path()).map_err(|error| EditError::CommitRecoveryFailed {
        path: path.clone(),
        message: format!("restore after {reason}: {error}"),
    })?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.as_std_path())
        .and_then(|file| file.sync_all())
        .map_err(|error| EditError::CommitRecoveryFailed {
            path: path.clone(),
            message: format!("sync restored target after {reason}: {error}"),
        })?;
    tracing::warn!(%path, %reason, "edit commit restored target from backup");
    Ok(())
}

/// §10.7 第 5 步：ReplaceFileW 失败后的确定性恢复。
fn recover_after_failure(
    path: &Utf8PathBuf,
    new_raw: &[u8],
    backup_path: &Option<std::path::PathBuf>,
    expected_revision: &str,
    error: std::io::Error,
) -> Result<(), EditError> {
    let read_revision = |source: &Path| -> Result<Option<String>, EditError> {
        match read_raw_path(source, path) {
            Ok(raw) => Ok(Some(revision_of(&raw))),
            Err(EditError::NotFound { .. }) => Ok(None),
            Err(read_error) => Err(read_error),
        }
    };
    let target_digest = read_revision(path.as_std_path())?;
    let new_digest = revision_of(new_raw);
    let backup_digest = match backup_path {
        Some(backup) => read_revision(backup)?,
        None => None,
    };

    match (target_digest, backup_digest) {
        // target 已包含新内容：提交实际成功（ReplaceFileW 返回失败但完成）。
        (Some(digest), _) if digest == new_digest => {
            verify_after_replace(path, backup_path, expected_revision, &new_digest)?;
            tracing::warn!(%path, %error, "ReplaceFileW reported failure but target is candidate");
            Ok(())
        }
        // target 是旧内容：未提交。清理可证明属于旧版本的 backup/temp，明确报失败。
        (Some(target), backup)
            if target == expected_revision
                && backup
                    .as_deref()
                    .is_none_or(|value| value == expected_revision) =>
        {
            if let Some(backup) = backup_path {
                let _ = std::fs::remove_file(backup);
            }
            tracing::warn!(%path, %error, "ReplaceFileW failed; target unchanged (restored)");
            Err(EditError::CommitFailed {
                path: path.clone(),
                message: format!("replacement failed and target is unchanged: {error}"),
            })
        }
        // target 不存在但 backup 存在：原文件已被移动，用 backup 恢复。
        (None, Some(backup)) if backup == expected_revision => {
            restore_target(path, backup_path, "target missing after replace failure")?;
            if let Some(backup_path) = backup_path {
                let _ = std::fs::remove_file(backup_path);
            }
            tracing::warn!(%path, %error, "ReplaceFileW failed; restored from backup");
            Err(EditError::CommitFailed {
                path: path.clone(),
                message: format!("replacement failed; old target restored: {error}"),
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

/// write 的新建路径（§10.6）：temp + sync + no-clobber move；
/// 仅当目标不存在时走此路径（已存在文件走 revision 校验后的整体重写）。
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
///
/// 模型协议使用不透明 `r{id}` 标识符（§模型协议）：完整 BLAKE3 hash
/// 仅在内部保留用于一致性校验，不暴露给 LLM。
#[derive(Debug)]
pub struct SnapshotStore {
    versions: std::collections::HashMap<Utf8PathBuf, std::collections::VecDeque<FileSnapshot>>,
    /// 插入顺序（淘汰最旧 path 用；HashMap 迭代序随机，不能当作 LRU）。
    order: std::collections::VecDeque<Utf8PathBuf>,
    max_paths: usize,
    max_versions_per_path: usize,
    stored_bytes: usize,
    max_total_bytes: usize,
    /// 模型可见的不透明 revision ID 映射（b3:hash → r{id}）。
    hash_to_id: std::collections::HashMap<String, u64>,
    id_to_hash: std::collections::HashMap<u64, String>,
    next_id: u64,
}

const MAX_STORED_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_STORE_BYTES: usize = 128 * 1024 * 1024;

fn snapshot_memory_bytes(snapshot: &FileSnapshot) -> usize {
    snapshot
        .raw
        .len()
        .saturating_add(snapshot.logical_lf_text.len())
        .saturating_add(
            snapshot
                .crlf_logical_offsets
                .len()
                .saturating_mul(std::mem::size_of::<usize>()),
        )
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
            stored_bytes: 0,
            max_total_bytes: MAX_SNAPSHOT_STORE_BYTES,
            hash_to_id: Default::default(),
            id_to_hash: Default::default(),
            next_id: 1,
        }
    }

    /// 记录一个快照（§10.1：read 保存完整 snapshot）。
    /// P1-7：同 path 更新时 move-to-back——活跃文件不能因为最早插入被淘汰
    /// （此前 order 只在首次插入时 push_back，不是真 LRU）。
    pub fn record(&mut self, snapshot: FileSnapshot) {
        let snapshot_bytes = snapshot_memory_bytes(&snapshot);
        if snapshot_bytes > MAX_STORED_SNAPSHOT_BYTES || self.max_versions_per_path == 0 {
            return;
        }
        let path = snapshot.path.clone();
        // §B4：identity key（Windows 大小写折叠）——`Foo.rs`/`foo.rs` 同一物理
        // 文件必须共享 snapshot 条目，否则 read 用大写 edit 用小写会查不到。
        let key = crate::tool::path_identity(path.as_str());
        if let Some(pos) = self.order.iter().position(|p| p.as_str() == key) {
            self.order.remove(pos);
        }
        self.order.push_back(Utf8PathBuf::from(&key));
        let entry = self.versions.entry(Utf8PathBuf::from(&key)).or_default();
        if let Some(position) = entry
            .iter()
            .position(|old| old.revision == snapshot.revision)
            && let Some(old) = entry.remove(position)
        {
            self.stored_bytes = self
                .stored_bytes
                .saturating_sub(snapshot_memory_bytes(&old));
        }
        // 分配不透明 revision ID：同一 hash 复用已有 ID（§模型协议）。
        let hash = snapshot.revision.clone();
        self.hash_to_id.entry(hash.clone()).or_insert_with(|| {
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            self.id_to_hash.insert(id, hash);
            id
        });
        entry.push_front(snapshot);
        self.stored_bytes = self.stored_bytes.saturating_add(snapshot_bytes);
        while entry.len() > self.max_versions_per_path {
            if let Some(old) = entry.pop_back() {
                self.stored_bytes = self
                    .stored_bytes
                    .saturating_sub(snapshot_memory_bytes(&old));
            }
        }
        // 淘汰最旧 path（按插入顺序）。
        while self.versions.len() > self.max_paths || self.stored_bytes > self.max_total_bytes {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(versions) = self.versions.remove(&oldest) {
                    for snapshot in versions {
                        self.stored_bytes = self
                            .stored_bytes
                            .saturating_sub(snapshot_memory_bytes(&snapshot));
                    }
                }
            } else {
                break;
            }
        }
    }

    /// 按 revision 取快照（digest 自校验，§10.1：损坏即视为不存在）。
    /// §B4：用 identity key 查找（大小写折叠）。
    pub fn get(&self, path: &Utf8PathBuf, revision: &str) -> Option<&FileSnapshot> {
        let key = Utf8PathBuf::from(crate::tool::path_identity(path.as_str()));
        let entry = self.versions.get(&key)?;
        entry.iter().find(|snapshot| {
            snapshot.revision == revision && revision_of(&snapshot.raw) == snapshot.revision
        })
    }

    pub fn latest(&self, path: &Utf8PathBuf) -> Option<&FileSnapshot> {
        let key = Utf8PathBuf::from(crate::tool::path_identity(path.as_str()));
        self.versions.get(&key)?.front()
    }

    pub fn clear(&mut self) {
        self.versions.clear();
        self.order.clear();
        self.stored_bytes = 0;
        self.hash_to_id.clear();
        self.id_to_hash.clear();
        self.next_id = 1;
    }

    /// 将模型传回的 revision token（`r42` 或 `b3:...`）解析为内部完整 BLAKE3 hash。
    pub fn resolve_token(&self, token: &str) -> Option<String> {
        let trimmed = token.trim();
        let inner = if let Some(inner) = trimmed
            .strip_prefix("[revision=")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            inner
        } else {
            trimmed
        };
        // r{id} 格式
        if let Some(id_str) = inner.strip_prefix('r') {
            if let Ok(id) = id_str.parse::<u64>() {
                return self.id_to_hash.get(&id).cloned();
            }
        }
        // b3:hash 格式（向后兼容）
        if is_valid_revision(inner) {
            return Some(inner.to_string());
        }
        None
    }

    /// 将内部 BLAKE3 hash 转换为模型可见的 `r{id}` 格式。
    pub fn display_revision(&self, hash: &str) -> String {
        if let Some(&id) = self.hash_to_id.get(hash) {
            format!("r{id}")
        } else {
            // 未注册的 hash（理论上不应发生）：回退到完整 hash。
            hash.to_string()
        }
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
    Ok(read_window_from_snapshot(&snapshot, start_line, line_count))
}

pub fn read_window_from_snapshot(
    snapshot: &FileSnapshot,
    start_line: usize,
    line_count: usize,
) -> ReadWindow {
    let lines: Vec<&str> = snapshot.logical_lf_text.lines().collect();
    let total_lines = lines.len();
    let start = start_line.saturating_sub(1).min(total_lines);
    let end = start + line_count;
    let truncated = end < total_lines;
    let text = lines[start..end.min(total_lines)].join("\n");
    ReadWindow {
        path: snapshot.path.clone(),
        revision: snapshot.revision.clone(),
        start_line: start + 1,
        returned_lines: end.min(total_lines) - start,
        total_lines,
        truncated,
        text,
    }
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
    fn snapshot_store_revision_id_mapping() {
        // §模型协议：同一 hash 复用 ID，不同 hash 分配新 ID。
        let mut store = SnapshotStore::new(64, 8);
        let raw_a = b"fn main() {}\n";
        let raw_b = b"fn test() {}\n";
        let hash_a = revision_of(raw_a);
        let hash_b = revision_of(raw_b);

        // record snapshot A
        let snap_a = build_snapshot(Utf8PathBuf::from("a.rs"), raw_a.to_vec()).unwrap();
        store.record(snap_a);
        assert_eq!(store.display_revision(&hash_a), "r1");

        // record snapshot B → 新 ID
        let snap_b = build_snapshot(Utf8PathBuf::from("b.rs"), raw_b.to_vec()).unwrap();
        store.record(snap_b);
        assert_eq!(store.display_revision(&hash_b), "r2");

        // record same hash A again → 复用 ID
        let snap_a2 = build_snapshot(Utf8PathBuf::from("a.rs"), raw_a.to_vec()).unwrap();
        store.record(snap_a2);
        assert_eq!(store.display_revision(&hash_a), "r1");

        // resolve_token: r1 → b3:hash
        assert_eq!(store.resolve_token("r1"), Some(hash_a.clone()));
        assert_eq!(store.resolve_token("r2"), Some(hash_b.clone()));
        assert_eq!(store.resolve_token("r999"), None);

        // resolve_token: b3:hash 向后兼容
        assert_eq!(store.resolve_token(&hash_a), Some(hash_a.clone()));

        // resolve_token: [revision=r1] 带 header 前缀
        assert_eq!(store.resolve_token("[revision=r1]"), Some(hash_a.clone()));

        // resolve_token: [revision=b3:...] 带 header 前缀
        let header = format_revision_header(&hash_b);
        assert_eq!(store.resolve_token(&header), Some(hash_b));

        // 未注册 hash → display_revision 回退
        assert_eq!(store.display_revision("b3:unknown"), "b3:unknown");

        // clear 重置 ID
        store.clear();
        let snap_c = build_snapshot(Utf8PathBuf::from("c.rs"), raw_a.to_vec()).unwrap();
        store.record(snap_c);
        assert_eq!(store.display_revision(&hash_a), "r1"); // 重新从 1 开始
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
        assert_eq!(snapshot.raw_offset(0), 3);
    }

    /// 回归：normalize_lf 此前按字节 push(b as char)，中文/emoji 被拆成
    /// Latin-1 乱码（read 输出乱码、edit 匹配用乱码文本）。
    #[test]
    fn snapshot_keeps_multibyte_chars_intact() {
        let path = Utf8PathBuf::from("test");
        let raw = "// 目标：让 `cargo build` 通过。\nfn 主() {}\n".as_bytes();
        let snapshot = build_snapshot(path, raw.to_vec()).unwrap();
        assert_eq!(
            &*snapshot.logical_lf_text,
            "// 目标：让 `cargo build` 通过。\nfn 主() {}\n"
        );
        // 字符边界映射：'目' 的原始字节偏移 = 3（"// " 之后）。
        assert_eq!(snapshot.raw_offset(3), 3);
        // 行数正确（logical 按字符换行）。
        assert_eq!(snapshot.logical_lf_text.lines().count(), 2);
    }

    /// 回归：中文内容 + CRLF + BOM 的组合快照与写回。
    #[test]
    fn snapshot_keeps_multibyte_crlf_and_bom() {
        let path = Utf8PathBuf::from("test");
        let raw_bytes = b"\xEF\xBB\xBF// \xe6\xa0\x87\xe9\xa2\x98\r\n\xe6\xad\xa3\xe6\x96\x87\xe5\x86\x85\xe5\xae\xb9\r\n";
        let raw = String::from_utf8(raw_bytes.to_vec()).unwrap();
        let snapshot = build_snapshot(path, raw.as_bytes().to_vec()).unwrap();
        assert_eq!(&*snapshot.logical_lf_text, "// 标题\n正文内容\n");
        assert_eq!(snapshot.line_ending, LineEnding::Crlf);
        // '正' 的 logical 字节偏移 = 3("// ") + 6("标题") + 1(\n) = 10；
        // raw 偏移 = 3(BOM) + 3 + 6 + 2(\r\n) = 14。
        assert_eq!(snapshot.raw_offset(10), 14);
        // 下一个字符边界保持 UTF-8 字节距离。
        assert_eq!(snapshot.raw_offset(13), 17);
    }

    /// 回归：中文文件 read_window 返回正确文本（此前乱码）。
    #[test]
    fn read_window_returns_utf8_chinese() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("zh.rs")).unwrap();
        std::fs::write(path.as_std_path(), "// 第一行\n// 第二行\nfn main() {}\n").unwrap();
        let window = read_window(&path, 1, 3).unwrap();
        assert_eq!(window.text, "// 第一行\n// 第二行\nfn main() {}");
        assert_eq!(window.total_lines, 3);
    }

    /// 回归：中文替换 round-trip（old_text/new_text 均为中文，写回字节正确）。
    #[test]
    fn edit_chinese_round_trip_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("zh.rs")).unwrap();
        let original = "// 标题：旧内容\nfn main() {}\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let snapshot = snapshot_file(&path).unwrap();
        let result = apply_edit(
            &path,
            &snapshot.revision,
            &[Replacement {
                old_text: "旧内容".into(),
                new_text: "新内容".into(),
            }],
        )
        .unwrap();
        assert_eq!(result.new_raw, b"// \xe6\xa0\x87\xe9\xa2\x98\xef\xbc\x9a\xe6\x96\xb0\xe5\x86\x85\xe5\xae\xb9\nfn main() {}\n");
        // 写回后可重新快照且 round-trip 稳定。
        std::fs::write(path.as_std_path(), &result.new_raw).unwrap();
        let again = snapshot_file(&path).unwrap();
        assert_eq!(&*again.logical_lf_text, "// 标题：新内容\nfn main() {}\n");
    }

    #[test]
    fn edit_rejects_stale_revision_when_old_text_no_longer_matches() {
        // §修复 #1：stale 但 old_text 在当前文件不存在/不再唯一 → 仍拒绝。
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        std::fs::write(path.as_std_path(), "let x = 1;\n").unwrap();
        // 外部把文件改成不包含 old_text 的内容（等价 fmt/编辑后 old_text 失效）。
        std::fs::write(path.as_std_path(), "let y = 9;\n").unwrap();
        let error = apply_edit(
            &path,
            "b3:0000000000000000000000000000000000000000000000000000000000000000",
            &[Replacement {
                old_text: "x = 1".into(),
                new_text: "x = 2".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code(), "stale_revision");
        assert!(
            matches!(
                error,
                EditError::StaleRevision {
                    context: Some(_),
                    ..
                }
            ),
            "stale 拒绝必须回填当前区域上下文: {error:?}"
        );
    }

    #[test]
    fn edit_applies_when_stale_but_old_text_still_unique() {
        // §修复 #1：revision 过期（如 fmt 改动其他区域）但 old_text 在当前文件
        // 仍唯一匹配 → 宽松应用，免重新 read。
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        std::fs::write(path.as_std_path(), "let x = 1;\n").unwrap();
        let old_revision = revision_of(b"let x = 1;\n");
        // 外部 fmt 只改动其他区域（缩进/换行），目标 old_text 仍唯一存在。
        std::fs::write(path.as_std_path(), "let x = 1;\n\nfn main() {}\n").unwrap();
        let result = apply_edit(
            &path,
            &old_revision,
            &[Replacement {
                old_text: "let x = 1".into(),
                new_text: "let x = 2".into(),
            }],
        )
        .expect("stale 但唯一匹配应宽松应用");
        assert_eq!(result.applied, 1);
        let text = String::from_utf8(result.new_raw).unwrap();
        assert!(text.contains("let x = 2"), "宽松应用必须生效: {text}");
    }

    #[test]
    fn edit_skips_noop_replacements_in_batch() {
        // §修复 #4：no-op（old == new）项跳过，其余正常应用；结果带 skipped_noops。
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        let original = "let a = 1;\nlet b = 2;\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let revision = revision_of(original.as_bytes());
        let result = apply_edit(
            &path,
            &revision,
            &[
                Replacement {
                    old_text: "a = 1".into(),
                    new_text: "a = 10".into(),
                },
                // no-op：跳过，不整批拒绝。
                Replacement {
                    old_text: "b = 2".into(),
                    new_text: "b = 2".into(),
                },
            ],
        )
        .expect("含 no-op 的批量应跳过 no-op 并应用其余");
        assert_eq!(result.applied, 1);
        assert_eq!(result.skipped_noops, 1);
        let text = String::from_utf8(result.new_raw).unwrap();
        assert!(text.contains("a = 10") && text.contains("b = 2"), "{text}");
    }

    #[test]
    fn edit_all_noops_is_rejected() {
        // §修复 #4：全部都是 no-op → 明确拒绝（机器码 no_op），不静默。
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        std::fs::write(path.as_std_path(), "let a = 1;\n").unwrap();
        let revision = revision_of(b"let a = 1;\n");
        let error = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "a = 1".into(),
                new_text: "a = 1".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code(), "no_op");
        assert!(matches!(error, EditError::AllNoOps { .. }));
    }

    #[test]
    fn no_match_error_carries_current_context() {
        // §修复 #3：no_match 附当前文件相关区域（定位失败处，模型免 read 自纠）。
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        let original = "fn main() {\n    work();\n}\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let revision = revision_of(original.as_bytes());
        let error = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "helper()".into(),
                new_text: "helper2()".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code(), "no_match");
        match error {
            EditError::NoMatch { context, .. } => {
                let ctx = context.unwrap_or_default();
                assert!(
                    ctx.contains("work()") || ctx.contains("fn main"),
                    "context 必须包含当前文件内容: {ctx}"
                );
            }
            other => panic!("期望 NoMatch: {other:?}"),
        }
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

    /// §10.3：相邻 replacement（end == start，不重叠）必须正确应用——
    /// 从后往前替换时不得因前面 replacement 已改内容而错位（回归：edit 损坏文件）。
    #[test]
    fn adjacent_replacements_apply_without_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("lib.rs")).unwrap();
        let original = "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\npub fn abs(x: i32) -> i32 {\n    if x < 0 { -x } else { x }\n}\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let revision = revision_of(original.as_bytes());
        // 两个相邻 replacement（前一个替换 add 主体，后一个替换 add 尾部 + abs 开头）。
        let result = apply_edit(
            &path,
            &revision,
            &[
                Replacement {
                    old_text: "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}".into(),
                    new_text: "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}".into(),
                },
                Replacement {
                    old_text: "pub fn abs(x: i32)".into(),
                    new_text: "pub fn abs(x: i64)".into(),
                },
            ],
        )
        .expect("相邻 replacement 应成功");
        // apply_edit 只计算新内容（写盘在 commit_edit）；从 result 验证。
        let content = String::from_utf8_lossy(&result.new_raw).into_owned();
        assert!(content.contains("a + b"), "add 必须替换: {content:?}");
        assert!(
            content.contains("pub fn abs(x: i64)"),
            "abs 必须替换: {content:?}"
        );
        assert!(!content.contains("a - b"), "不应残留 a - b: {content:?}");
        // 不损坏：add 函数头只能出现一次完整定义。
        assert_eq!(
            content
                .matches("pub fn add(a: i32, b: i32) -> i32 {")
                .count(),
            1,
            "add 函数头不得重复: {content:?}"
        );
        // abs 替换后不出现 x: i32 的 abs 签名。
        assert!(
            !content.contains("pub fn abs(x: i32)"),
            "abs 旧签名不得残留: {content:?}"
        );
    }

    #[test]
    fn out_of_order_replacements_apply_by_file_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("order.txt")).unwrap();
        let original = "alpha beta gamma delta\n";
        std::fs::write(path.as_std_path(), original).unwrap();
        let revision = revision_of(original.as_bytes());
        let result = apply_edit(
            &path,
            &revision,
            &[
                Replacement {
                    old_text: "gamma".into(),
                    new_text: "G".into(),
                },
                Replacement {
                    old_text: "alpha".into(),
                    new_text: "a-much-longer-alpha".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(result.new_raw).unwrap(),
            "a-much-longer-alpha beta G delta\n"
        );
    }

    #[test]
    fn overlapping_occurrences_are_not_treated_as_unique() {
        assert_eq!(count_occurrences("aaa", "aa"), 2);
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
    fn failed_replace_with_unchanged_target_is_not_reported_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.txt")).unwrap();
        let backup = dir.path().join("backup.tmp");
        std::fs::write(path.as_std_path(), b"old").unwrap();
        std::fs::write(&backup, b"old").unwrap();
        let expected = revision_of(b"old");

        let error = recover_after_failure(
            &path,
            b"new",
            &Some(backup.clone()),
            &expected,
            std::io::Error::other("simulated replace failure"),
        )
        .unwrap_err();
        assert!(matches!(error, EditError::CommitFailed { .. }));
        assert_eq!(std::fs::read(path.as_std_path()).unwrap(), b"old");
        assert!(!backup.exists());
    }

    #[test]
    fn failed_replace_that_committed_candidate_keeps_backup_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.txt")).unwrap();
        let backup = dir.path().join("backup.tmp");
        std::fs::write(path.as_std_path(), b"new").unwrap();
        std::fs::write(&backup, b"old").unwrap();

        recover_after_failure(
            &path,
            b"new",
            &Some(backup.clone()),
            &revision_of(b"old"),
            std::io::Error::other("ambiguous OS result"),
        )
        .unwrap();
        assert!(backup.exists(), "ToolCompleted 前必须保留 backup 证据");
    }

    #[test]
    fn post_replace_verification_rejects_missing_planned_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.txt")).unwrap();
        std::fs::write(path.as_std_path(), b"new").unwrap();
        let missing = Some(dir.path().join("missing.backup"));
        let error =
            verify_after_replace(&path, &missing, &revision_of(b"old"), &revision_of(b"new"))
                .unwrap_err();
        assert!(matches!(error, EditError::CommitRecoveryFailed { .. }));
    }

    #[test]
    fn unified_diff_is_bounded_for_large_rewrites() {
        let previous_raw = vec![b'a'; 3 * 1024 * 1024];
        let new_raw = vec![b'b'; 3 * 1024 * 1024];
        let result = EditResult {
            previous_revision: revision_of(&previous_raw),
            current_revision: revision_of(&new_raw),
            applied: 1,
            skipped_noops: 0,
            tier: MatchTier::Exact,
            previous_raw,
            new_raw,
        };
        let diff = unified_diff(&result);
        assert!(diff.contains("diff omitted"));
        assert!(diff.len() < 1024);
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

    // ---- P1/P2 宽容匹配单元测试 ----

    #[test]
    fn policy_makefile_disables_uniform_indent_but_keeps_trailing() {
        let makefile = Utf8PathBuf::from("Makefile");
        let gnu = Utf8PathBuf::from("GNUmakefile");
        let mk = Utf8PathBuf::from("build.mk");
        let rs = Utf8PathBuf::from("src/main.rs");
        for p in [&makefile, &gnu, &mk] {
            assert_eq!(
                whitespace_policy_for(p),
                WhitespacePolicy::TrailingInsensitive,
                "{p}: Makefile 的 tab 是语法，禁用 uniform-indent"
            );
        }
        assert_eq!(
            whitespace_policy_for(&rs),
            WhitespacePolicy::UniformOuterIndent
        );
    }

    #[test]
    fn line_bounds_handles_newline_terminated_and_open_ended() {
        assert_eq!(line_bounds("a\nb\n"), vec![(0, 2), (2, 4)]);
        assert_eq!(line_bounds("a\nb"), vec![(0, 2), (2, 3)]);
        assert_eq!(line_bounds("a\n\nb"), vec![(0, 2), (2, 3), (3, 4)]);
        assert!(line_bounds("").is_empty());
    }

    #[test]
    fn trailing_insensitive_locate_unique_and_preserves_original_range() {
        let hay = "fn a() {   \n    work();\n}\n";
        // old_text 无 trailing：命中唯一；区间为完整行（含真实 trailing 与换行）。
        let (start, end) = locate_trailing_insensitive(hay, "fn a() {\n    work();").unwrap();
        assert_eq!(&hay[start..end], "fn a() {   \n    work();\n");
        // 歧义：两处 trailing 不同的相同行 → 拒绝（安全失败）。
        let dup = "x()   \nx()\n";
        assert!(locate_trailing_insensitive(dup, "x()").is_none());
        // old_text 自身带 trailing 也能匹配（双向归一化）。
        let (s2, e2) = locate_trailing_insensitive(hay, "fn a() {   \n    work();").unwrap();
        assert_eq!(&hay[s2..e2], "fn a() {   \n    work();\n");
        // old_text 末行带 \n：区间同样为完整行。
        let (s3, e3) = locate_trailing_insensitive(hay, "fn a() {\n    work();\n").unwrap();
        assert_eq!(&hay[s3..e3], "fn a() {   \n    work();\n");
    }

    #[test]
    fn uniform_indent_locate_accepts_outer_shift_rejects_relative_change() {
        // 整体少 8 空格（坐标系平移）：命中，P = 8 空格；区间为完整行。
        let hay = "fn main() {\n        if x {\n            foo();\n        }\n}\n";
        let old = "if x {\n    foo();\n}";
        let (start, end, prefix) = locate_uniform_indent(hay, old).unwrap();
        assert_eq!(
            &hay[start..end],
            "        if x {\n            foo();\n        }\n"
        );
        assert_eq!(prefix, "        ");
        // relative indentation 破坏（第二行少缩进）：拒绝。
        let bad_old = "if x {\nfoo();\n}";
        assert!(
            locate_uniform_indent(hay, bad_old).is_none(),
            "relative indent 变化必须拒绝"
        );
        // 内容不同：拒绝。
        assert!(locate_uniform_indent(hay, "if x {\n    bar();\n}").is_none());
        // 歧义（两处同构且内容相同的缩进块）：拒绝。
        let dup = "    if a {\n        x();\n    }\n    if a {\n        x();\n    }\n";
        assert!(locate_uniform_indent(dup, "if a {\n    x();\n}").is_none());
    }

    #[test]
    fn reindent_adds_prefix_to_non_blank_lines_only() {
        assert_eq!(
            reindent_new_text("if x {\n    foo();\n}\n", "    "),
            "    if x {\n        foo();\n    }\n"
        );
        // 空行不加前缀。
        assert_eq!(reindent_new_text("a\n\nb\n", "  "), "  a\n\n  b\n");
        assert_eq!(reindent_new_text("x", ""), "x");
    }

    #[test]
    fn analyze_whitespace_reports_tabs_crlf_and_trailing() {
        let path = Utf8PathBuf::from("t.rs");
        let raw = b"fn a() {\r\n\twork(); \r\n}\r\n";
        let snapshot = build_snapshot(path, raw.to_vec()).unwrap();
        let ws = analyze_whitespace(&snapshot);
        assert_eq!(ws.line_endings, LineEndingsSummary::Crlf);
        assert_eq!(ws.indentation, IndentationSummary::Tabs);
        assert!(ws.trailing_whitespace);
        // 空格缩进 + LF + 无 trailing。
        let path2 = Utf8PathBuf::from("s.rs");
        let raw2 = b"fn b() {\n    work();\n}\n";
        let ws2 = analyze_whitespace(&build_snapshot(path2, raw2.to_vec()).unwrap());
        assert_eq!(ws2.indentation, IndentationSummary::Spaces);
        assert_eq!(ws2.line_endings, LineEndingsSummary::Lf);
        assert!(!ws2.trailing_whitespace);
        // 混合缩进。
        let path3 = Utf8PathBuf::from("m.rs");
        let raw3 = b"a()\n\tb()\n  c()\n";
        let ws3 = analyze_whitespace(&build_snapshot(path3, raw3.to_vec()).unwrap());
        assert_eq!(ws3.indentation, IndentationSummary::Mixed);
    }

    #[test]
    fn diagnose_no_match_distinguishes_indentation_vs_textual() {
        // 缩进差异：lstrip 后内容相同。
        let text = "fn main() {\n        if x {\n            foo();\n        }\n}\n";
        let old = "if x {\n    foo();\n}";
        let d = diagnose_no_match(text, old).unwrap();
        assert_eq!(d.kind, MismatchKind::Indentation);
        assert_eq!(d.content_equal_after_lstrip, Some(true));
        assert!(d.similarity_bp >= 5000);
        // expected = 文件实际缩进（8 空格）；provided = old_text 首行缩进
        //（old 首行 `if x {` 无缩进 → 空）。
        assert_eq!(
            d.expected_indent.as_deref(),
            Some("\"        \""),
            "expected 必须是文件实际缩进"
        );
        assert_eq!(
            d.provided_indent.as_deref(),
            Some("\"\""),
            "provided 必须是 old_text 提供的缩进"
        );
        // 文本差异。
        let old2 = "if y {\n    foo();\n}";
        let d2 = diagnose_no_match(text, old2).unwrap();
        assert_eq!(d2.kind, MismatchKind::Textual);
        assert!(d2.first_difference.is_some());
    }

    /// ISSUE-003：超长单行/海量行文件的失败诊断必须**有界**——
    /// 此前 locate_context 先整行 append 再截断（64MiB 单行直接突破），
    /// diagnose_no_match 全窗口扫描 + 整行存储（百万行文件数十秒 + 数 MiB 输出）。
    #[test]
    fn issue_003_failure_diagnostics_are_bounded_on_huge_files() {
        // 1) locate_context：单行 1MiB，诊断输出必须 ≤ 400 字符预算。
        let huge_line = "x".repeat(1024 * 1024);
        let text = format!("before\n{huge_line}\nafter\n");
        let ctx = locate_context(&text, "nonexistent token");
        let ctx = ctx.expect("locate_context 必须返回兜底片段");
        assert!(
            ctx.len() <= 512,
            "单行超长时 locate_context 必须截断: {} 字节",
            ctx.len()
        );

        // 2) diagnose_no_match：百万行文件 + 多行 old_text，输出与耗时都有界。
        let mut hay = String::with_capacity(8 * 1024 * 1024);
        for i in 0..200_000 {
            hay.push_str(&format!("line {i}\n"));
        }
        // old_text 首行与 hay 相似但内容不同（必然 no match）。
        let old = "line 99999\nline 100000\nline 100001\nline CHANGED";
        let start = std::time::Instant::now();
        let d = diagnose_no_match(&hay, old);
        let elapsed = start.elapsed();
        assert!(d.is_some(), "应给出近似诊断");
        assert!(
            elapsed.as_secs() < 5,
            "百万行级别诊断必须快速返回（窗口上限）: {elapsed:?}"
        );
        if let Some(diag) = &d
            && let Some((provided, actual)) = &diag.first_difference
        {
            assert!(
                provided.len() <= 220,
                "差异行必须截断: {} 字节",
                provided.len()
            );
            assert!(actual.len() <= 220, "差异行必须截断: {} 字节", actual.len());
        }
    }

    /// P1：trailing 宽容定位应用后，未触及字节（真实 trailing）原样保留。
    #[test]
    fn edit_trailing_tolerance_preserves_untouched_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a.rs")).unwrap();
        let content = "fn a() {   \n    work();\n}\n";
        std::fs::write(path.as_std_path(), content).unwrap();
        let revision = revision_of(content.as_bytes());
        let result = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "fn a() {\n    work();".into(),
                new_text: "fn a() {\n    run();".into(),
            }],
        )
        .unwrap();
        assert_eq!(result.tier, MatchTier::TrailingInsensitive);
        commit_edit(&result, &path, &prepare_commit(&path)).unwrap();
        let after = std::fs::read_to_string(path.as_std_path()).unwrap();
        // 行尾 3 空格必须保留（未触及字节）。
        assert_eq!(after, "fn a() {   \n    run();\n}\n");
    }

    /// P2：uniform outer-indent 应用后，new_text 按实际前缀重建缩进。
    #[test]
    fn edit_uniform_indent_rebuilds_new_text_with_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("f.rs")).unwrap();
        let content = "fn main() {\n        if x {\n            foo();\n        }\n}\n";
        std::fs::write(path.as_std_path(), content).unwrap();
        let revision = revision_of(content.as_bytes());
        let result = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "if x {\n    foo();\n}".into(),
                new_text: "if x {\n    bar();\n}".into(),
            }],
        )
        .unwrap();
        assert_eq!(result.tier, MatchTier::UniformOuterIndent);
        commit_edit(&result, &path, &prepare_commit(&path)).unwrap();
        let after = std::fs::read_to_string(path.as_std_path()).unwrap();
        assert_eq!(
            after,
            "fn main() {\n        if x {\n            bar();\n        }\n}\n"
        );
    }

    /// P2：relative indentation 破坏 → 拒绝（不模糊替换）。
    #[test]
    fn edit_uniform_indent_rejects_relative_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("g.rs")).unwrap();
        let content = "fn main() {\n        if x {\n            foo();\n        }\n}\n";
        std::fs::write(path.as_std_path(), content).unwrap();
        let revision = revision_of(content.as_bytes());
        let err = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "if x {\nfoo();\n}".into(),
                new_text: "if x {\nbar();\n}".into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, EditError::NoMatch { .. }), "{err:?}");
    }

    /// P2b：Makefile 的 tab 缩进不宽容——空格版 old_text 对 tab 行仍 no_match。
    #[test]
    fn edit_makefile_tab_indent_is_not_lenient() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("Makefile")).unwrap();
        let content = "target:\n\tcommand\n";
        std::fs::write(path.as_std_path(), content).unwrap();
        let revision = revision_of(content.as_bytes());
        // 空格版（模型可能给 4 空格）：uniform-indent 被 policy 禁用 → NoMatch。
        let err = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "target:\n    command".into(),
                new_text: "target:\n    newcmd".into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, EditError::NoMatch { .. }), "{err:?}");
        // 精确 tab 版本正常。
        let ok = apply_edit(
            &path,
            &revision,
            &[Replacement {
                old_text: "target:\n\tcommand".into(),
                new_text: "target:\n\tnewcmd".into(),
            }],
        )
        .unwrap();
        assert_eq!(ok.tier, MatchTier::Exact);
    }
}

#[cfg(test)]
mod p1_lru_tests {
    use super::*;

    fn snapshot(path: &str, tag: &str) -> (FileSnapshot, String) {
        let logical_lf_text: Arc<str> = Arc::from(format!("content-{tag}"));
        let raw: Arc<[u8]> = Arc::from(logical_lf_text.as_bytes().to_vec());
        let revision = revision_of(&raw);
        (
            FileSnapshot {
                path: Utf8PathBuf::from(path),
                raw,
                logical_lf_text,
                revision: revision.clone(),
                has_bom: false,
                line_ending: LineEnding::Lf,
                crlf_logical_offsets: Vec::new(),
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
