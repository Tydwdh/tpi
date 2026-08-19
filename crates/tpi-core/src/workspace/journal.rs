//! Mutation Journal — durable append-only log of workspace transactions.

use super::mutation::WorkspaceMutation;
use super::policy::Reversibility;
use super::transaction::MutationCause;
use super::types::{CheckpointId, Timestamp, TransactionId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A single journal entry representing one committed transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub transaction_id: TransactionId,
    pub cause: MutationCause,
    pub checkpoint_id: CheckpointId,
    pub mutations: Vec<WorkspaceMutation>,
    pub reversibility: Reversibility,
    pub timestamp: Timestamp,
}

/// Integrity status of the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalIntegrity {
    /// All entries are parseable.
    Clean,
    /// Some entries could not be parsed — undo/redo blocked unless forced.
    Tainted { corrupt_lines: usize },
}

/// Loaded journal state.
#[derive(Debug)]
pub struct JournalState {
    pub entries: Vec<JournalEntry>,
    pub integrity: JournalIntegrity,
}

/// Persistent append-only journal stored as JSONL.
#[derive(Debug)]
pub struct MutationJournal {
    path: PathBuf,
}

impl MutationJournal {
    /// Create a journal handle for a given session.
    pub fn new(artifacts_root: &Path, session_id: &str) -> Self {
        let path = artifacts_root
            .join(session_id)
            .join("workspace-journal.jsonl");
        Self { path }
    }

    /// Direct path constructor.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append a single entry (one JSON line).
    pub fn append(&self, entry: &JournalEntry) -> Result<(), JournalError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| JournalError::Io {
                context: "create journal dir".into(),
                source: e,
            })?;
        }

        let line =
            serde_json::to_string(entry).map_err(|e| JournalError::Serialize(e.to_string()))?;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| JournalError::Io {
                context: format!("open journal {}", self.path.display()),
                source: e,
            })?;

        use std::io::Write;
        writeln!(file, "{line}").map_err(|e| JournalError::Io {
            context: "append journal entry".into(),
            source: e,
        })?;

        file.sync_all().map_err(|e| JournalError::Io {
            context: "fsync journal".into(),
            source: e,
        })?;

        Ok(())
    }

    /// Load the entire journal from disk, counting corrupt lines.
    pub fn load(&self) -> Result<JournalState, JournalError> {
        if !self.path.exists() {
            return Ok(JournalState {
                entries: Vec::new(),
                integrity: JournalIntegrity::Clean,
            });
        }

        let content = std::fs::read_to_string(&self.path).map_err(|e| JournalError::Io {
            context: format!("read journal {}", self.path.display()),
            source: e,
        })?;

        let mut entries = Vec::new();
        let mut corrupt = 0usize;

        for (line_no, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalEntry>(trimmed) {
                Ok(entry) => entries.push(entry),
                Err(_) => {
                    corrupt += 1;
                    tracing::warn!(line_no = line_no + 1, "journal: corrupt line skipped");
                }
            }
        }

        let integrity = if corrupt == 0 {
            JournalIntegrity::Clean
        } else {
            JournalIntegrity::Tainted {
                corrupt_lines: corrupt,
            }
        };

        Ok(JournalState { entries, integrity })
    }

    /// Find a specific transaction by id.
    pub fn find(&self, tx_id: &TransactionId) -> Result<Option<JournalEntry>, JournalError> {
        let state = self.load()?;
        Ok(state
            .entries
            .into_iter()
            .find(|e| &e.transaction_id == tx_id))
    }

    /// Whether the journal file exists.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Path to the journal file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Errors from journal operations.
#[derive(Debug)]
pub enum JournalError {
    Io {
        context: String,
        source: std::io::Error,
    },
    Serialize(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Io { context, source } => write!(f, "{context}: {source}"),
            JournalError::Serialize(msg) => write!(f, "journal serialize: {msg}"),
        }
    }
}

impl std::error::Error for JournalError {}

#[cfg(test)]
mod tests {
    use super::super::types::{BlobId, NormalizedPath};
    use super::*;
    use tempfile::tempdir;

    fn make_entry(tx_id: &str) -> JournalEntry {
        JournalEntry {
            transaction_id: TransactionId::new(tx_id),
            cause: MutationCause::External,
            checkpoint_id: CheckpointId::new("cp-0"),
            mutations: vec![WorkspaceMutation::Create {
                path: NormalizedPath::new(
                    std::path::Path::new("new.txt"),
                    std::path::Path::new("."),
                ),
                content: BlobId::new("aaa"),
            }],
            reversibility: Reversibility::Exact,
            timestamp: Timestamp::now(),
        }
    }

    #[test]
    fn append_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let journal = MutationJournal::new(dir.path(), "test-session");

        journal.append(&make_entry("tx-1")).unwrap();
        journal.append(&make_entry("tx-2")).unwrap();

        let state = journal.load().unwrap();
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.integrity, JournalIntegrity::Clean);
    }

    #[test]
    fn find_by_id() {
        let dir = tempdir().unwrap();
        let journal = MutationJournal::new(dir.path(), "test-session");

        journal.append(&make_entry("tx-1")).unwrap();
        journal.append(&make_entry("tx-2")).unwrap();

        let found = journal.find(&TransactionId::new("tx-2")).unwrap().unwrap();
        assert_eq!(found.transaction_id, TransactionId::new("tx-2"));
    }

    #[test]
    fn corrupt_line_marks_tainted() {
        let dir = tempdir().unwrap();
        let journal = MutationJournal::new(dir.path(), "test-session");

        // Write a valid line then a corrupt one.
        let path = journal.path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let valid_line = serde_json::to_string(&make_entry("tx-ok")).unwrap();
        std::fs::write(path, format!("{valid_line}\nthis is not valid json\n")).unwrap();

        let state = journal.load().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(
            state.integrity,
            JournalIntegrity::Tainted { corrupt_lines: 1 }
        );
    }

    #[test]
    fn empty_journal_is_clean() {
        let dir = tempdir().unwrap();
        let journal = MutationJournal::new(dir.path(), "test-session");
        let state = journal.load().unwrap();
        assert!(state.entries.is_empty());
        assert_eq!(state.integrity, JournalIntegrity::Clean);
    }
}
