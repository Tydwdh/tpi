//! Session-scoped resource identity and lifecycle metadata.
//!
//! These types deliberately live in `tpi-core`: the agent graph, capability
//! layer, and persistence-facing adapters all need to agree on the same
//! ownership vocabulary without making the dependency graph cyclic.

use crate::ids::{AgentId, DelegationId, ToolCallId};

/// How long a resource is allowed to outlive the run that created it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLifetime {
    /// Ends when the owning agent reaches a terminal state.
    #[default]
    Agent,
    /// May outlive one agent run, but ends with the delegation tree node.
    Delegation,
    /// Session-scoped resource; ends when the session runtime shuts down.
    Session,
}

/// Workspace effect visibility of a long-lived resource.
///
/// `ExternallyMutable` is intentionally stronger than a scheduler access
/// class: a persistent shell/process can mutate workspace state between tool
/// calls, so the workspace must not be treated as a scheduler-only invariant.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccess {
    #[default]
    ReadOnly,
    Mutating,
    ExternallyMutable,
}

/// Stable owner of a managed resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ResourceOwner {
    pub agent_id: AgentId,
    pub delegation_id: Option<DelegationId>,
}

/// Metadata attached at resource creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ResourceMeta {
    pub owner: ResourceOwner,
    pub lifetime: ResourceLifetime,
    pub created_by: ToolCallId,
    pub workspace_access: WorkspaceAccess,
}

/// Caller identity used for resource authorization.
///
/// `managed_agent_ids` is a graph snapshot assembled by the agent runtime. It
/// lets a parent control resources owned by descendants without teaching the
/// capability layer about `AgentManager` internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub agent_id: AgentId,
    pub parent_agent_id: Option<AgentId>,
    pub delegation_id: Option<DelegationId>,
    pub managed_agent_ids: Vec<AgentId>,
}

impl AgentIdentity {
    pub fn owner(&self) -> ResourceOwner {
        ResourceOwner {
            agent_id: self.agent_id,
            delegation_id: self.delegation_id,
        }
    }

    pub fn can_manage(&self, owner: ResourceOwner, lifetime: ResourceLifetime) -> bool {
        matches!(lifetime, ResourceLifetime::Session)
            || owner.agent_id == self.agent_id
            || self.managed_agent_ids.contains(&owner.agent_id)
    }
}
