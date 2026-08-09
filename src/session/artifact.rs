//! 有界 artifact 存储。
//!
//! Artifact record：opaque ID、MIME、byte length、digest、创建工具、保留策略和内部路径。
//! UI 可直接展开文本 artifact；模型通过 `read(@artifact/...)` 有界读取（§8.4）。
//! session 删除时 artifact 才随之清理，不运行后台"智能整理"。

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
}

impl ArtifactWriter {
    /// 在 artifacts 目录创建新 artifact。
    pub fn create(
        artifacts_root: &std::path::Path,
        session_id: &str,
        tool: &str,
        mime: &str,
    ) -> std::io::Result<Self> {
        let dir = artifacts_root.join(session_id);
        std::fs::create_dir_all(&dir)?;
        let id = crate::ids::EventId::new_v7().to_string();
        let internal_path = dir.join(format!("{id}.out"));
        let file = std::fs::File::create(&internal_path)?;
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
        })
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    /// 追加一段输出。
    pub fn write(&mut self, _stream: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// 完成并返回 record（flush + 计算 digest）。
    pub fn finish(mut self) -> std::io::Result<ArtifactRecord> {
        self.file.flush()?;
        self.record.byte_length = self.written;
        let mut digest = blake3::Hasher::new();
        let mut file = std::fs::File::open(&self.record.internal_path)?;
        std::io::copy(&mut file, &mut digest)?;
        self.record.digest = format!("b3:{}", digest.finalize().to_hex());
        Ok(self.record)
    }
}

/// 读取 artifact 内容（有界，§8.4）。
pub fn read_bounded(record: &ArtifactRecord, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(&record.internal_path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut bytes = Vec::with_capacity(max_bytes.min(record.byte_length as usize));
    reader
        .by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

use std::io::Read;

/// 按 opaque id 查找 artifact（`@artifact/<session>/<id>`，§8.4）。
pub fn find(
    artifacts_root: &std::path::Path,
    session_id: &str,
    id: &str,
) -> Option<ArtifactRecord> {
    if !crate::tool::validate_artifact_component(session_id)
        || !crate::tool::validate_artifact_component(id)
    {
        return None;
    }
    let path = artifacts_root.join(session_id).join(format!("{id}.out"));
    if !path.exists() {
        return None;
    }
    let meta = path.metadata().ok()?;
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
