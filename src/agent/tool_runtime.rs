//! Run-scoped state shared by every built-in tool invocation.
//!
//! Keeping construction here makes `ToolContext` an implementation detail of the
//! agent/tool boundary. New context fields have one initialization site instead
//! of one site for execution and another for post-execution observation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::ids::ToolCallId;
use crate::tool::edit::SnapshotStore;
use crate::tool::plan::Plan;
use crate::tool::search::ScanSnapshot;
use crate::tool::{ToolContext, ToolStreamEvent};

pub(super) struct ToolRuntime {
    config: RuntimeConfig,
    cancel: CancellationToken,
    session_id: String,
    interactive: bool,
    scan_snapshots: Arc<Mutex<HashMap<String, ScanSnapshot>>>,
    snapshot_store: Arc<Mutex<SnapshotStore>>,
    current_plan: Arc<Mutex<Option<Plan>>>,
}

#[derive(Clone)]
struct RuntimeConfig {
    workspace_root: camino::Utf8PathBuf,
    allow_outside_workspace: bool,
    artifacts_root: std::path::PathBuf,
    shell_path: Option<camino::Utf8PathBuf>,
}

impl ToolRuntime {
    pub(super) fn new(
        config: &Config,
        session_id: String,
        cancel: CancellationToken,
        interactive: bool,
    ) -> Self {
        Self {
            config: RuntimeConfig {
                workspace_root: config.workspace_root.clone(),
                allow_outside_workspace: config.allow_outside_workspace,
                artifacts_root: config.artifacts_root.clone(),
                shell_path: config.shell_path.clone(),
            },
            cancel,
            session_id,
            interactive,
            scan_snapshots: Default::default(),
            snapshot_store: Default::default(),
            current_plan: Default::default(),
        }
    }

    pub(super) fn plan_snapshot(&self) -> Option<Plan> {
        crate::util::lock_mutex(&self.current_plan, "current_plan").clone()
    }

    pub(super) fn context(
        &self,
        call_id: ToolCallId,
        output_tx: Option<mpsc::Sender<ToolStreamEvent>>,
    ) -> ToolContext {
        ToolContext {
            workspace_root: self.config.workspace_root.clone(),
            allow_outside_workspace: self.config.allow_outside_workspace,
            cancel: self.cancel.clone(),
            artifacts_root: self.config.artifacts_root.clone(),
            session_id: self.session_id.clone(),
            call_id,
            output_tx,
            scan_snapshots: self.scan_snapshots.clone(),
            shell_path: self.config.shell_path.clone(),
            snapshot_store: self.snapshot_store.clone(),
            current_plan: self.current_plan.clone(),
            interactive: self.interactive,
        }
    }
}
