//! Fundamental types shared across the workspace transaction system.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

// ── Normalized Path ──

/// Platform-aware normalized path for identity comparison.
///
/// On Windows: case-insensitive, forward slashes normalized to backslashes.
/// On Unix: case-sensitive, canonical form.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct NormalizedPath(String);

impl NormalizedPath {
    pub fn new(path: &Path, root: &Path) -> Self {
        let relative = path.strip_prefix(root).unwrap_or(path);
        Self(normalize_path_string(&relative.to_string_lossy()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn join(&self, child: &str) -> NormalizedPath {
        let joined = if self.0.is_empty() {
            child.to_string()
        } else {
            format!("{}\\{}", self.0, child)
        };
        NormalizedPath(joined)
    }
}

impl fmt::Display for NormalizedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for NormalizedPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn normalize_path_string(path: &str) -> String {
    let normalized = path.replace('/', "\\");
    #[cfg(windows)]
    {
        normalized.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

// ── ID Types ──

/// Opaque identifier for content-addressed blobs (BLAKE3 hex).
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct BlobId(String);

impl BlobId {
    pub fn new(hash_hex: &str) -> Self {
        Self(hash_hex.to_string())
    }

    /// Two-char prefix for the directory bucket.
    pub fn bucket(&self) -> &str {
        &self.0[..2.min(self.0.len())]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for BlobId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Unique identifier for a workspace checkpoint.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointId(String);

impl CheckpointId {
    pub fn new(id: &str) -> Self {
        Self(id.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a workspace transaction.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransactionId(String);

impl TransactionId {
    pub fn new(id: &str) -> Self {
        Self(id.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── Entry Kind ──

/// Type of filesystem entry tracked by the workspace index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Symlink { target: PathBuf },
}

// ── Timestamp ──

/// Serializable timestamp (millisecond precision).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Timestamp {
    millis: u64,
}

impl Timestamp {
    pub fn now() -> Self {
        let duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            millis: duration.as_millis() as u64,
        }
    }

    pub fn millis(&self) -> u64 {
        self.millis
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_path_display() {
        let p = NormalizedPath::new(Path::new("src/main.rs"), Path::new("."));
        assert_eq!(p.as_str(), "src\\main.rs");
    }

    #[test]
    fn blob_id_bucket() {
        let id = BlobId::new("abcdef1234567890");
        assert_eq!(id.bucket(), "ab");
    }

    #[test]
    fn normalized_path_join() {
        let base = NormalizedPath::new(Path::new("src"), Path::new("."));
        let child = base.join("main.rs");
        assert_eq!(child.as_str(), "src\\main.rs");
    }
}
