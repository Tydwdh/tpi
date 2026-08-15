//! 有界 artifact 存储。
//!
//! Artifact record：opaque ID、MIME、byte length、digest、创建工具、保留策略和内部路径。
//! UI 可直接展开文本 artifact；模型通过 `read(@artifact/...)` 有界读取（§8.4）。
//! session 删除时 artifact 才随之清理，不运行后台"智能整理"。

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 单个 artifact 的落盘上限。模型读取另有更窄窗口；这里防止长时间刷屏命令
/// 无限占用磁盘。超限写入失败，调用方的 Job Object 会终止进程树，Drop 删除残片。
pub const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// artifact 记录（§14.3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: String,
    pub mime: String,
    pub byte_length: u64,
    pub digest: String,
    pub created_by: String,
    /// 保留策略：session 生命周期。
    pub retention: String,
    /// 内部路径（不暴露给模型；模型只使用 opaque id）。
    pub internal_path: PathBuf,
}

/// 追加式 artifact 写入器（完整工具输出落盘，§8.4）。
pub struct ArtifactWriter {
    record: ArtifactRecord,
    file: std::fs::File,
    written: u64,
    finished: bool,
}

impl ArtifactWriter {
    /// 在 artifacts 目录创建新 artifact。
    pub fn create(
        artifacts_root: &std::path::Path,
        session_id: &str,
        tool: &str,
        mime: &str,
    ) -> std::io::Result<Self> {
        if !tpi_core::util::validate_artifact_component(session_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid artifact session id",
            ));
        }
        let dir = artifacts_root.join(session_id);
        std::fs::create_dir_all(&dir)?;
        let dir_meta = std::fs::symlink_metadata(&dir)?;
        if !dir_meta.is_dir() || tpi_core::util::is_symlink_or_reparse(&dir)? {
            return Err(std::io::Error::other(
                "artifact session path is not a regular directory",
            ));
        }
        let id = tpi_core::ids::EventId::new_v7().to_string();
        let internal_path = dir.join(format!("{id}.out"));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&internal_path)?;
        Ok(Self {
            record: ArtifactRecord {
                id,
                mime: mime.to_string(),
                byte_length: 0,
                digest: String::new(),
                created_by: tool.to_string(),
                retention: "session".into(),
                internal_path,
            },
            file,
            written: 0,
            finished: false,
        })
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    /// 追加一段输出。
    pub fn write(&mut self, _stream: &str, bytes: &[u8]) -> std::io::Result<()> {
        let next = checked_artifact_len(self.written, bytes.len(), MAX_ARTIFACT_BYTES)?;
        self.file.write_all(bytes)?;
        self.written = next;
        Ok(())
    }

    /// 完成并返回 record（flush + 计算 digest）。
    pub fn finish(mut self) -> std::io::Result<ArtifactRecord> {
        self.file.flush()?;
        self.file.sync_data()?;
        self.record.byte_length = self.written;
        let mut digest = blake3::Hasher::new();
        let mut file = std::fs::File::open(&self.record.internal_path)?;
        std::io::copy(&mut file, &mut digest)?;
        self.record.digest = format!("b3:{}", digest.finalize().to_hex());
        self.finished = true;
        Ok(self.record.clone())
    }
}

fn checked_artifact_len(current: u64, additional: usize, limit: u64) -> std::io::Result<u64> {
    let additional =
        u64::try_from(additional).map_err(|_| std::io::Error::other("artifact 长度溢出"))?;
    let next = current
        .checked_add(additional)
        .ok_or_else(|| std::io::Error::other("artifact 长度溢出"))?;
    if next > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("artifact 超过 {limit} 字节上限"),
        ));
    }
    Ok(next)
}

impl Drop for ArtifactWriter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.record.internal_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.record.internal_path.display(),
                %error,
                "failed to remove incomplete artifact"
            );
        }
    }
}

/// 读取 artifact 内容（有界，§8.4）。
pub fn read_bounded(record: &ArtifactRecord, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(&record.internal_path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut bytes = Vec::with_capacity(max_bytes.min(record.byte_length as usize));
    reader
        .by_ref()
        .take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// 按 opaque id 查找 artifact（`@artifact/<session>/<id>`，§8.4）。
pub fn find(
    artifacts_root: &std::path::Path,
    session_id: &str,
    id: &str,
) -> Option<ArtifactRecord> {
    if !tpi_core::util::validate_artifact_component(session_id)
        || !tpi_core::util::validate_artifact_component(id)
    {
        return None;
    }
    let path = artifacts_root.join(session_id).join(format!("{id}.out"));
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || tpi_core::util::is_symlink_or_reparse(&path).ok()?
    {
        return None;
    }
    Some(ArtifactRecord {
        id: id.to_string(),
        mime: "text/plain".into(),
        byte_length: meta.len(),
        digest: String::new(),
        created_by: String::new(),
        retention: "session".into(),
        internal_path: path,
    })
}

pub struct ArtifactWindow {
    pub bytes: Vec<u8>,
    pub returned_lines: usize,
    pub truncated: bool,
}

/// 按行流式定位 artifact 窗口；内存只保存请求窗口，超长单行也受 `max_bytes` 限制。
pub fn read_line_window(
    record: &ArtifactRecord,
    start_line: usize,
    max_lines: usize,
    max_bytes: usize,
) -> std::io::Result<ArtifactWindow> {
    let mut file = std::io::BufReader::new(std::fs::File::open(&record.internal_path)?);
    let requested_start = start_line.max(1);
    let max_lines = max_lines.max(1);
    let mut current_line = 1usize;
    let mut returned_lines = 0usize;
    let mut consumed = 0u64;
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    let mut chunk = [0u8; 8192];

    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            if bytes.last().is_some_and(|byte| *byte != b'\n') && returned_lines < max_lines {
                returned_lines = returned_lines.saturating_add(1);
            }
            return Ok(ArtifactWindow {
                bytes,
                returned_lines,
                truncated: false,
            });
        }
        for byte in &chunk[..read] {
            consumed = consumed.saturating_add(1);
            if current_line >= requested_start && returned_lines < max_lines {
                if bytes.len() >= max_bytes {
                    return Ok(ArtifactWindow {
                        bytes,
                        returned_lines: returned_lines.max(1),
                        truncated: true,
                    });
                }
                bytes.push(*byte);
            }
            if *byte == b'\n' {
                if current_line >= requested_start {
                    returned_lines = returned_lines.saturating_add(1);
                    if returned_lines >= max_lines {
                        return Ok(ArtifactWindow {
                            bytes,
                            returned_lines,
                            truncated: consumed < record.byte_length,
                        });
                    }
                }
                current_line = current_line.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_finish_is_readable_and_bounded_with_sentinel_byte() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            ArtifactWriter::create(dir.path(), "session", "bash", "text/plain").unwrap();
        writer.write("stdout", b"abcdef").unwrap();
        let record = writer.finish().unwrap();
        assert_eq!(record.byte_length, 6);
        assert_eq!(read_bounded(&record, 3).unwrap(), b"abcd");
        assert!(record.digest.starts_with("b3:"));
    }

    #[test]
    fn unfinished_writer_removes_partial_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let mut writer =
                ArtifactWriter::create(dir.path(), "session", "bash", "text/plain").unwrap();
            writer.write("stdout", b"partial").unwrap();
            writer.record.internal_path.clone()
        };
        assert!(!path.exists(), "未完成 artifact 不得遗留孤儿文件");
    }

    #[test]
    fn artifact_length_limit_is_checked_before_writing() {
        assert_eq!(checked_artifact_len(3, 2, 5).unwrap(), 5);
        assert_eq!(
            checked_artifact_len(5, 1, 5).unwrap_err().kind(),
            std::io::ErrorKind::FileTooLarge
        );
        assert!(checked_artifact_len(u64::MAX, 1, u64::MAX).is_err());
    }

    #[test]
    fn line_window_can_page_beyond_the_initial_byte_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            ArtifactWriter::create(dir.path(), "session", "bash", "text/plain").unwrap();
        writer
            .write("stdout", "first\nsecond\nthird\n".as_bytes())
            .unwrap();
        let record = writer.finish().unwrap();
        let window = read_line_window(&record, 3, 1, 64).unwrap();
        assert_eq!(window.bytes, b"third\n");
        assert_eq!(window.returned_lines, 1);
        assert!(!window.truncated);

        let bounded = read_line_window(&record, 1, 3, 3).unwrap();
        assert_eq!(bounded.bytes, b"fir");
        assert!(bounded.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn find_rejects_symlink_artifacts() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session");
        std::fs::create_dir_all(&session).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"secret").unwrap();
        symlink(&outside, session.join("id.out")).unwrap();
        assert!(find(dir.path(), "session", "id").is_none());
    }
}
