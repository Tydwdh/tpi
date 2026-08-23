//! Workspace Transaction System — persistent workspace state, CAS, mutations, undo/redo.
//!
//! Core principle: *Workspace is persistent state, not per-command snapshots.*
//!
//! # Module structure
//!
//! ```text
//!                 WorkspaceManager
//!                       │
//!       ┌───────────────┼────────────────┐
//!       │               │                │
//!  WorkspaceIndex    BlobStore    MutationCoordinator
//!       │               │                │
//!       └───────┬───────┘                │
//!               │                        │
//!           Checkpoint ◄─────────────────┘
//!               │
//!       WorkspaceTransaction
//!               │
//!       MutationJournal
//!               │
//!           Undo / Redo
//! ```

pub mod blob;
pub mod checkpoint;
pub mod coordinator;
pub mod index;
pub mod journal;
pub mod manager;
pub mod mutation;
pub mod policy;
pub mod recovery;
pub mod transaction;
pub mod types;
pub mod watcher;

// Re-export core types at module root for ergonomic imports.
pub use blob::BlobStore;
pub use checkpoint::Checkpoint;
pub use index::FileEntry;
pub use index::WorkspaceIndex;
pub use journal::{JournalEntry, JournalIntegrity, JournalState, MutationJournal};
pub use mutation::WorkspaceMutation;
pub use policy::{MutationSafetyPolicy, Reversibility, ReversibilityIssue, TrackingPolicy};
pub use transaction::{MutationCause, TransactionState, WorkspaceTransaction};
pub use types::{BlobId, CheckpointId, EntryKind, NormalizedPath, TransactionId};
