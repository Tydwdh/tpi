//! Workspace transaction — the state machine governing every mutation.

use super::types::{CheckpointId, Timestamp, TransactionId};
use serde::{Deserialize, Serialize};

/// Who produced this mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationProvenance {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MutationCause {
    Edit {
        tool_call_id: String,
    },
    Write {
        tool_call_id: String,
    },
    Command {
        command_id: String,
    },
    Terminal {
        terminal_id: String,
    },
    Job {
        process_id: String,
    },
    Agent {
        agent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegation_id: Option<String>,
        operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    Undo {
        transaction_id: TransactionId,
    },
    Redo {
        transaction_id: TransactionId,
    },
    External,
}

impl std::fmt::Display for MutationCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MutationCause::Edit { tool_call_id } => write!(f, "edit({tool_call_id})"),
            MutationCause::Write { tool_call_id } => write!(f, "write({tool_call_id})"),
            MutationCause::Command { command_id } => write!(f, "command({command_id})"),
            MutationCause::Terminal { terminal_id } => write!(f, "terminal({terminal_id})"),
            MutationCause::Job { process_id } => write!(f, "job({process_id})"),
            MutationCause::Agent {
                agent_id,
                operation,
                ..
            } => write!(f, "agent({agent_id}, {operation})"),
            MutationCause::Undo { transaction_id } => write!(f, "undo({transaction_id})"),
            MutationCause::Redo { transaction_id } => write!(f, "redo({transaction_id})"),
            MutationCause::External => write!(f, "external"),
        }
    }
}

impl MutationCause {
    /// Preserve the original operation while attaching graph provenance to the
    /// single global workspace journal entry.
    pub fn with_provenance(
        self,
        provenance: MutationProvenance,
        tool_call_id: Option<String>,
    ) -> Self {
        let operation = self.to_string();
        Self::Agent {
            agent_id: provenance.agent_id,
            parent_agent_id: provenance.parent_agent_id,
            delegation_id: provenance.delegation_id,
            operation,
            tool_call_id,
        }
    }
}

/// Transaction lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionState {
    /// Transaction created, before any mutations recorded.
    Preparing,
    /// Mutations are being applied / reconciled.
    Running,
    /// Reconciling filesystem state after execution.
    Reconciling,
    /// Successfully committed to journal.
    Committed,
    /// Transaction failed (reason recorded).
    Failed { reason: String },
    /// Transaction was aborted (e.g. user cancel).
    Aborted,
    /// Transaction was undone by a subsequent undo operation.
    Undone,
}

/// A workspace transaction tracks a single logical mutation operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceTransaction {
    pub id: TransactionId,
    pub cause: MutationCause,
    pub checkpoint_before: CheckpointId,
    pub checkpoint_after: Option<CheckpointId>,
    pub state: TransactionState,
    pub created_at: Timestamp,
    pub committed_at: Option<Timestamp>,
}

impl WorkspaceTransaction {
    pub fn new(cause: MutationCause, checkpoint_before: CheckpointId) -> Self {
        Self {
            id: TransactionId::new(&uuid::Uuid::now_v7().to_string()),
            cause,
            checkpoint_before,
            checkpoint_after: None,
            state: TransactionState::Preparing,
            created_at: Timestamp::now(),
            committed_at: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            TransactionState::Committed
                | TransactionState::Failed { .. }
                | TransactionState::Aborted
                | TransactionState::Undone
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::CheckpointId;
    use super::*;

    #[test]
    fn new_transaction_is_preparing() {
        let tx = WorkspaceTransaction::new(
            MutationCause::Edit {
                tool_call_id: "tc-1".into(),
            },
            CheckpointId::new("cp-0"),
        );
        assert_eq!(tx.state, TransactionState::Preparing);
        assert!(!tx.is_terminal());
    }

    #[test]
    fn committed_is_terminal() {
        let mut tx = WorkspaceTransaction::new(
            MutationCause::Command {
                command_id: "cmd-1".into(),
            },
            CheckpointId::new("cp-0"),
        );
        tx.state = TransactionState::Committed;
        assert!(tx.is_terminal());
    }

    #[test]
    fn cause_display() {
        let cause = MutationCause::External;
        assert_eq!(format!("{cause}"), "external");
    }
}
