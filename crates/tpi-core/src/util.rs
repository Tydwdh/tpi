//! 通用错误处理辅助（§错误处理纪律：生产代码不允许 unwrap/expect/panic）。
//!
//! 唯一的例外是 `unreachable` 语义的 match 分支——用显式分支消除，
//! 不保留 `unreachable!()` 宏。所有不可恢复的错误都记录日志并降级。

use std::sync::{Mutex, MutexGuard};

/// 一次有界逐行读取的结果。`bytes` 不包含行尾 LF；CRLF 的 CR 保留为
/// JSON/文本解析器可接受的尾部空白。`consumed_bytes` 包含实际消费的行尾。
pub struct BoundedLine {
    pub bytes: Vec<u8>,
    pub terminated: bool,
    pub consumed_bytes: u64,
}

/// 有界逐行读取状态。超长行会被完整丢弃到下一个行边界，调用者可安全继续，
/// 不会把同一物理行的剩余部分误当成下一行。
pub enum BoundedLineRead {
    Eof,
    Line(BoundedLine),
    TooLong,
}

/// 获取互斥锁；遇 poison（持锁线程 panic 后遗留）时记录告警并恢复，
/// 不 panic、不丢弃数据（`PoisonError::into_inner` 取回内部数据）。
///
/// `what` 用于日志标识（如 `"current_plan"`）。
pub fn lock_mutex<'a, T>(mutex: &'a Mutex<T>, what: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(what, "mutex poisoned; recovering guard");
            poisoned.into_inner()
        }
    }
}

/// P7 下沉：artifact 路径组件校验（原 `crate::tool::validate_artifact_component`）。
/// 纯函数，core 层提供；tool（files）与 session（artifact）共用。
pub fn validate_artifact_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// P7-02 拆 crate：`TPI_HOME` 解析（原 `crate::config::tpi_home`）。纯函数，
/// core 层提供；capabilities（mcp）、skills、config、app 共用。
pub fn tpi_home() -> std::path::PathBuf {
    std::env::var_os("TPI_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".tpi"))
                .unwrap_or_else(|| std::path::PathBuf::from(".tpi"))
        })
}

/// UTF-8 安全截断：把 `String` 截断到不超过 `max_bytes` 的最大字符边界。
///
/// `String::truncate` 在非 char boundary 处直接 panic；所有按字节预算截断
/// `String` 的代码必须经过这里（BUG-001/002 回归：read/web_fetch 大中文内容）。
/// `text.len() <= max_bytes` 时原样返回。
pub fn truncate_to_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// 读取不超过 `max_bytes` 的整个文件，并抵御 metadata 检查后的增长竞态。
pub fn read_file_bounded(path: &std::path::Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > max_bytes as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("文件超过 {max_bytes} 字节上限"),
        ));
    }
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("文件超过 {max_bytes} 字节上限"),
        ));
    }
    Ok(bytes)
}

/// UTF-8 文本文件的有界读取；大小和编码错误都在同一底层边界处理。
pub fn read_utf8_file_bounded(path: &std::path::Path, max_bytes: usize) -> std::io::Result<String> {
    String::from_utf8(read_file_bounded(path, max_bytes)?).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("文件不是有效 UTF-8: {error}"),
        )
    })
}

/// 从 `BufRead` 读取一个物理行，最多保留 `max_bytes` 字节。
///
/// 超限后继续消费到 LF/EOF，但不继续分配；这既限制内存，也保持后续行边界。
pub fn read_line_bounded<R: std::io::BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<BoundedLineRead> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    let mut consumed_bytes = 0u64;
    let mut saw_input = false;
    let mut too_long = false;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if !saw_input {
                Ok(BoundedLineRead::Eof)
            } else if too_long {
                Ok(BoundedLineRead::TooLong)
            } else {
                Ok(BoundedLineRead::Line(BoundedLine {
                    bytes,
                    terminated: false,
                    consumed_bytes,
                }))
            };
        }
        saw_input = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index.saturating_add(1));
        let payload = newline.map_or(consumed, |index| index);
        if !too_long {
            match bytes.len().checked_add(payload) {
                Some(total) if total <= max_bytes => bytes.extend_from_slice(&available[..payload]),
                _ => {
                    too_long = true;
                    bytes.clear();
                }
            }
        }
        consumed_bytes = consumed_bytes
            .checked_add(u64::try_from(consumed).unwrap_or(u64::MAX))
            .ok_or_else(|| std::io::Error::other("行偏移溢出"))?;
        reader.consume(consumed);
        if newline.is_some() {
            return if too_long {
                Ok(BoundedLineRead::TooLong)
            } else {
                Ok(BoundedLineRead::Line(BoundedLine {
                    bytes,
                    terminated: true,
                    consumed_bytes,
                }))
            };
        }
    }
}

/// 判断路径自身是否是符号链接或 Windows reparse point（junction 等）。
/// 对可能递归或执行破坏性操作的调用方，这比 `Path::is_dir/is_file` 的跟随式
/// 判断更安全。
#[cfg(windows)]
pub fn is_symlink_or_reparse(path: &std::path::Path) -> std::io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

#[cfg(not(windows))]
pub fn is_symlink_or_reparse(path: &std::path::Path) -> std::io::Result<bool> {
    Ok(std::fs::symlink_metadata(path)?.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_prefix_within_budget_and_valid_utf8() {
        let mut s = "你好世界abc".to_string();
        truncate_to_char_boundary(&mut s, 7); // "你好世" = 9 字节；7 落在 "世" 中间 → 截到 "你好"（6 字节）
        assert_eq!(s, "你好");
        assert!(s.is_char_boundary(s.len()));
        assert!(s.len() <= 7);
    }

    #[test]
    fn truncate_short_input_unchanged() {
        let mut s = "abc".to_string();
        truncate_to_char_boundary(&mut s, 100);
        assert_eq!(s, "abc");
    }

    #[test]
    fn truncate_emoji_zwj_never_splits() {
        let mut s = "👨‍💻x".to_string(); // 8 字节 + 1
        truncate_to_char_boundary(&mut s, 8);
        // 8 字节边界在 ZWJ 序列内 → 回退到 0（不产生半个 emoji/非法字节）。
        assert!(s.is_char_boundary(s.len()));
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());
        assert!(s.len() <= 8);
    }

    #[test]
    fn bounded_line_discards_whole_oversized_line_and_resumes_at_next_line() {
        let input = b"123456\nnext\n";
        let mut reader = std::io::BufReader::with_capacity(2, &input[..]);
        assert!(matches!(
            read_line_bounded(&mut reader, 3).unwrap(),
            BoundedLineRead::TooLong
        ));
        let BoundedLineRead::Line(line) = read_line_bounded(&mut reader, 4).unwrap() else {
            panic!("second line must remain readable");
        };
        assert_eq!(line.bytes, b"next");
        assert!(line.terminated);
        assert_eq!(line.consumed_bytes, 5);
    }

    #[test]
    fn bounded_file_rechecks_actual_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large");
        std::fs::write(&path, b"12345").unwrap();
        assert_eq!(read_file_bounded(&path, 5).unwrap(), b"12345");
        assert_eq!(
            read_file_bounded(&path, 4).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}
