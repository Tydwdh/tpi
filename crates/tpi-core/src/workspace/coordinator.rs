//! Multi-agent mutation coordinator.
//!
//! Ensures concurrent agents don't produce conflicting uncoordinated
//! workspace writes. Provides write serialization and access control.
//!
//! ```text
//! Root Agent ──┐
//! Agent A ─────┤
//! Agent B ─────┼── WorkspaceMutationCoordinator
//! Background ──┤
//! PTY ─────────┘
//! ```

use super::types::TransactionId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Access mode for a workspace transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceAccess {
    /// Read-only access (no mutations expected).
    SharedReadOnly,
    /// Read-write access with coordination (serialized writes).
    SharedExclusive,
    /// Fully isolated workspace (e.g., worktree, overlay).
    Isolated,
}

/// Information about an active transaction.
#[derive(Debug, Clone)]
pub struct TransactionInfo {
    pub transaction_id: TransactionId,
    pub agent_id: String,
    pub access: WorkspaceAccess,
    pub started_at: u64, // epoch millis
}

/// Coordinates workspace access across multiple agents.
///
/// Core invariant: at most one SharedExclusive writer at a time.
/// SharedReadOnly callers can proceed in parallel.
pub struct MutationCoordinator {
    /// Currently active transactions.
    active: HashMap<TransactionId, TransactionInfo>,
    /// Number of active readers.
    reader_count: usize,
}

impl MutationCoordinator {
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
            reader_count: 0,
        }
    }

    /// Request access to the workspace. Returns true if granted.
    pub fn acquire(
        &mut self,
        tx_id: TransactionId,
        agent_id: String,
        access: WorkspaceAccess,
    ) -> Result<(), AcquireError> {
        let tx_id_for_insert = tx_id.clone();
        match &access {
            WorkspaceAccess::SharedReadOnly => {
                // Read-only: always granted as long as no exclusive writer
                // is active (to prevent reads during write).
                if self.has_active_writer() {
                    let blocked_by = self
                        .active
                        .values()
                        .find(|t| t.access == WorkspaceAccess::SharedExclusive)
                        .map(|t| t.transaction_id.clone())
                        .unwrap_or_else(|| TransactionId::new("unknown"));
                    return Err(AcquireError::WriteConflict { blocked_by });
                }
                self.reader_count += 1;
                self.active.insert(
                    tx_id_for_insert.clone(),
                    TransactionInfo {
                        transaction_id: tx_id,
                        agent_id,
                        access,
                        started_at: timestamp_millis(),
                    },
                );
                Ok(())
            }
            WorkspaceAccess::SharedExclusive => {
                // Exclusive write: only one at a time, and no readers.
                if self.has_active_writer() {
                    let blocked_by = self
                        .active
                        .values()
                        .find(|t| t.access == WorkspaceAccess::SharedExclusive)
                        .map(|t| t.transaction_id.clone())
                        .unwrap_or_else(|| TransactionId::new("unknown"));
                    return Err(AcquireError::WriteConflict { blocked_by });
                }
                if self.reader_count > 0 {
                    return Err(AcquireError::ReadersActive {
                        count: self.reader_count,
                    });
                }
                self.active.insert(
                    tx_id_for_insert.clone(),
                    TransactionInfo {
                        transaction_id: tx_id,
                        agent_id,
                        access,
                        started_at: timestamp_millis(),
                    },
                );
                Ok(())
            }
            WorkspaceAccess::Isolated => {
                // Isolated: always granted (separate workspace).
                self.active.insert(
                    tx_id_for_insert.clone(),
                    TransactionInfo {
                        transaction_id: tx_id,
                        agent_id,
                        access,
                        started_at: timestamp_millis(),
                    },
                );
                Ok(())
            }
        }
    }

    /// Release access after transaction completes.
    pub fn release(&mut self, tx_id: &TransactionId) {
        if let Some(info) = self.active.remove(tx_id) {
            if info.access == WorkspaceAccess::SharedReadOnly {
                self.reader_count = self.reader_count.saturating_sub(1);
            }
        }
    }

    /// Check if there's an active exclusive writer.
    fn has_active_writer(&self) -> bool {
        self.active
            .values()
            .any(|t| t.access == WorkspaceAccess::SharedExclusive)
    }

    /// List active transactions.
    pub fn active_transactions(&self) -> Vec<&TransactionInfo> {
        self.active.values().collect()
    }

    /// Number of active transactions.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

impl Default for MutationCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from access acquisition.
#[derive(Debug)]
pub enum AcquireError {
    /// Another agent holds an exclusive write lock.
    WriteConflict { blocked_by: TransactionId },
    /// Readers are active (exclusive write blocked).
    ReadersActive { count: usize },
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::WriteConflict { blocked_by } => {
                write!(f, "write conflict: blocked by transaction {blocked_by}")
            }
            AcquireError::ReadersActive { count } => {
                write!(f, "{count} reader(s) active, exclusive write blocked")
            }
        }
    }
}

impl std::error::Error for AcquireError {}

fn timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::types::TransactionId;

    fn tx(id: &str) -> TransactionId {
        TransactionId::new(id)
    }

    #[test]
    fn multiple_readers_allowed() {
        let mut coord = MutationCoordinator::new();
        coord
            .acquire(tx("r1"), "agent-a".into(), WorkspaceAccess::SharedReadOnly)
            .unwrap();
        coord
            .acquire(tx("r2"), "agent-b".into(), WorkspaceAccess::SharedReadOnly)
            .unwrap();
        assert_eq!(coord.active_count(), 2);
    }

    #[test]
    fn writer_blocks_other_writers() {
        let mut coord = MutationCoordinator::new();
        coord
            .acquire(tx("w1"), "agent-a".into(), WorkspaceAccess::SharedExclusive)
            .unwrap();
        let result = coord.acquire(tx("w2"), "agent-b".into(), WorkspaceAccess::SharedExclusive);
        assert!(result.is_err());
    }

    #[test]
    fn writer_blocks_readers() {
        let mut coord = MutationCoordinator::new();
        coord
            .acquire(tx("w1"), "agent-a".into(), WorkspaceAccess::SharedExclusive)
            .unwrap();
        let result = coord.acquire(tx("r1"), "agent-b".into(), WorkspaceAccess::SharedReadOnly);
        assert!(result.is_err());
    }

    #[test]
    fn reader_blocks_writer() {
        let mut coord = MutationCoordinator::new();
        coord
            .acquire(tx("r1"), "agent-a".into(), WorkspaceAccess::SharedReadOnly)
            .unwrap();
        let result = coord.acquire(tx("w1"), "agent-b".into(), WorkspaceAccess::SharedExclusive);
        assert!(result.is_err());
    }

    #[test]
    fn isolated_always_succeeds() {
        let mut coord = MutationCoordinator::new();
        coord
            .acquire(tx("w1"), "agent-a".into(), WorkspaceAccess::SharedExclusive)
            .unwrap();
        coord
            .acquire(tx("i1"), "agent-b".into(), WorkspaceAccess::Isolated)
            .unwrap();
    }

    #[test]
    fn release_allows_new_writer() {
        let mut coord = MutationCoordinator::new();
        coord
            .acquire(tx("w1"), "agent-a".into(), WorkspaceAccess::SharedExclusive)
            .unwrap();
        coord.release(&tx("w1"));
        coord
            .acquire(tx("w2"), "agent-b".into(), WorkspaceAccess::SharedExclusive)
            .unwrap();
    }
}
