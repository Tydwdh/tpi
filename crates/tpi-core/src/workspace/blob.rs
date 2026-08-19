//! Content-Addressed Blob Store.
//!
//! Stores file content indexed by BLAKE3 hash. Same content is stored only once.
//! On-disk layout: `<root>/objects/<2-char-bucket>/<remaining-hex>`

use super::types::BlobId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Content-addressed store for file blobs.
///
/// ```text
/// .tpi/workspace/objects/
///   ab/
///     cdef1234567890...
/// ```
#[derive(Debug)]
pub struct BlobStore {
    root: PathBuf,
}

/// Metadata persisted alongside each blob for fast existence checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobMeta {
    pub id: BlobId,
    pub size: u64,
    pub created_at: u64, // epoch millis
}

impl BlobStore {
    /// Open or create a blob store at the given root.
    /// `root` is typically `.tpi/workspace/objects/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store content and return its BLAKE3-based BlobId.
    /// If content with the same hash already exists, the existing entry is reused.
    pub fn put(&self, bytes: &[u8]) -> Result<BlobId, BlobError> {
        let hash = blake3::hash(bytes);
        let hex = hash.to_hex();
        let id = BlobId::new(&hex);

        let path = self.blob_path(&id);
        if path.exists() {
            return Ok(id);
        }

        // Ensure parent directory (2-char bucket) exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BlobError::Io {
                context: "create blob bucket".into(),
                source: e,
            })?;
        }

        // Write to temp, then rename for atomicity.
        let temp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&temp).map_err(|e| BlobError::Io {
                context: "create blob temp".into(),
                source: e,
            })?;
            file.write_all(bytes).map_err(|e| BlobError::Io {
                context: "write blob temp".into(),
                source: e,
            })?;
            let _ = file.sync_all(); // Best-effort fsync; rename provides ordering.
        }

        // Rename temp → final (atomic on same filesystem).
        std::fs::rename(&temp, &path).map_err(|e| BlobError::Io {
            context: "rename blob temp".to_string(),
            source: e,
        })?;

        Ok(id)
    }

    /// Retrieve blob content by id.
    pub fn get(&self, id: &BlobId) -> Result<Vec<u8>, BlobError> {
        let path = self.blob_path(id);
        std::fs::read(&path).map_err(|e| BlobError::Io {
            context: format!("read blob {}", id.as_str()),
            source: e,
        })
    }

    /// Check whether a blob exists (cheap metadata check).
    pub fn contains(&self, id: &BlobId) -> bool {
        self.blob_path(id).exists()
    }

    /// On-disk path for a blob.
    fn blob_path(&self, id: &BlobId) -> PathBuf {
        self.root.join(id.bucket()).join(id.as_str())
    }

    /// Total number of blobs stored.
    pub fn count(&self) -> Result<usize, BlobError> {
        let mut count = 0;
        let entries = std::fs::read_dir(&self.root).map_err(|e| BlobError::Io {
            context: "read blob store root".into(),
            source: e,
        })?;
        for bucket in entries.flatten() {
            if bucket.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                if let Ok(bucket_entries) = std::fs::read_dir(bucket.path()) {
                    count += bucket_entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
                        .count();
                }
            }
        }
        Ok(count)
    }

    /// Total bytes stored across all blobs.
    pub fn total_bytes(&self) -> Result<u64, BlobError> {
        let mut total = 0u64;
        let entries = std::fs::read_dir(&self.root).map_err(|e| BlobError::Io {
            context: "read blob store root".into(),
            source: e,
        })?;
        for bucket in entries.flatten() {
            if bucket.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                if let Ok(bucket_entries) = std::fs::read_dir(bucket.path()) {
                    for entry in bucket_entries.flatten() {
                        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                            if let Ok(meta) = entry.metadata() {
                                total += meta.len();
                            }
                        }
                    }
                }
            }
        }
        Ok(total)
    }
}

/// Errors from blob store operations.
#[derive(Debug)]
pub enum BlobError {
    Io {
        context: String,
        source: std::io::Error,
    },
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobError::Io { context, source } => {
                write!(f, "{context}: {source}")
            }
        }
    }
}

impl std::error::Error for BlobError {}

/// Compute the BLAKE3 hash of some bytes, returning the hex string.
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let content = b"hello world";
        let id = store.put(content).unwrap();

        assert!(store.contains(&id));
        let retrieved = store.get(&id).unwrap();
        assert_eq!(retrieved, content);
    }

    #[test]
    fn dedup_same_content() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let id1 = store.put(b"duplicate").unwrap();
        let id2 = store.put(b"duplicate").unwrap();
        assert_eq!(id1, id2);

        // Only one file on disk.
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn different_content_different_id() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let id1 = store.put(b"alpha").unwrap();
        let id2 = store.put(b"beta").unwrap();
        assert_ne!(id1, id2);
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn empty_content() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let id = store.put(b"").unwrap();
        assert!(store.contains(&id));
        assert_eq!(store.get(&id).unwrap(), b"");
    }

    #[test]
    fn large_content() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let content = vec![0xABu8; 1024 * 1024]; // 1 MiB
        let id = store.put(&content).unwrap();
        assert_eq!(store.get(&id).unwrap(), content);
    }

    #[test]
    fn total_bytes_calculation() {
        let dir = tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        store.put(b"aaa").unwrap();
        store.put(b"bbb").unwrap();
        store.put(b"aaa").unwrap(); // dedup

        let total = store.total_bytes().unwrap();
        assert_eq!(total, 6); // "aaa" + "bbb"
    }

    #[test]
    fn blake3_hex_deterministic() {
        let h1 = blake3_hex(b"test");
        let h2 = blake3_hex(b"test");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // BLAKE3 produces 32 bytes = 64 hex chars
    }
}
