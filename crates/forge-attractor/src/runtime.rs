use crate::storage::AttractorArtifactWriter;
use crate::{AttractorError, Graph, Node, RuntimeContext, handlers};
use async_trait::async_trait;
use forge_cxdb_runtime::{CxdbFsSnapshotPolicy, CxdbTurnId as TurnId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Notify;

/// Cooperative pipeline control handle.
///
/// Shared between the caller (web server, CLI) and the pipeline runner.
/// The runner checks these signals at safe points between node executions.
#[derive(Debug)]
pub struct PipelineControl {
    cancelled: AtomicBool,
    paused: AtomicBool,
    resume_notify: Notify,
}

impl PipelineControl {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            resume_notify: Notify::new(),
        }
    }

    /// Request cancellation. The runner will exit before executing the next node.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // Also wake any paused waiter so it can see the cancel
        self.resume_notify.notify_one();
    }

    /// Returns true if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Pause the pipeline. The runner will block before the next node execution.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// Resume a paused pipeline.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.resume_notify.notify_one();
    }

    /// Returns true if currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Wait until resumed or cancelled. Returns true if cancelled.
    pub async fn wait_for_resume(&self) -> bool {
        loop {
            if self.is_cancelled() {
                return true;
            }
            if !self.is_paused() {
                return false;
            }
            self.resume_notify.notified().await;
        }
    }
}

impl Default for PipelineControl {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeStatus {
    #[default]
    Success,
    PartialSuccess,
    Retry,
    Fail,
    Skipped,
}

impl NodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::PartialSuccess => "partial_success",
            Self::Retry => "retry",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }

    pub fn is_success_like(self) -> bool {
        matches!(self, Self::Success | Self::PartialSuccess)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeOutcome {
    pub status: NodeStatus,
    pub notes: Option<String>,
    pub failure_reason: Option<String>,
    pub context_updates: RuntimeContext,
    pub preferred_label: Option<String>,
    pub suggested_next_ids: Vec<String>,
}

impl NodeOutcome {
    pub fn success() -> Self {
        Self {
            status: NodeStatus::Success,
            ..Default::default()
        }
    }

    pub fn failure(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            status: NodeStatus::Fail,
            notes: Some(reason.clone()),
            failure_reason: Some(reason),
            ..Default::default()
        }
    }
}

#[async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute(
        &self,
        node: &Node,
        context: &RuntimeContext,
        graph: &Graph,
    ) -> Result<NodeOutcome, AttractorError>;
}

#[derive(Debug, Default)]
pub struct NoopNodeExecutor;

#[async_trait]
impl NodeExecutor for NoopNodeExecutor {
    async fn execute(
        &self,
        _node: &Node,
        _context: &RuntimeContext,
        _graph: &Graph,
    ) -> Result<NodeOutcome, AttractorError> {
        Ok(NodeOutcome::success())
    }
}

pub struct RunConfig {
    pub run_id: Option<String>,
    pub base_turn_id: Option<TurnId>,
    pub storage: Option<crate::storage::SharedAttractorStorageWriter>,
    pub artifacts: Option<Arc<dyn AttractorArtifactWriter>>,
    pub cxdb_persistence: CxdbPersistenceMode,
    pub fs_snapshot_policy: Option<CxdbFsSnapshotPolicy>,
    pub events: crate::RuntimeEventSink,
    pub executor: Arc<dyn NodeExecutor>,
    pub retry_backoff: crate::RetryBackoffConfig,
    pub logs_root: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub resume_from_checkpoint: Option<PathBuf>,
    pub max_loop_restarts: u32,
    pub control: Option<Arc<PipelineControl>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CxdbPersistenceMode {
    Off,
    Required,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            run_id: None,
            base_turn_id: None,
            storage: None,
            artifacts: None,
            cxdb_persistence: CxdbPersistenceMode::Off,
            fs_snapshot_policy: None,
            events: crate::RuntimeEventSink::default(),
            executor: Arc::new(handlers::registry::RegistryNodeExecutor::new(
                handlers::core_registry(),
            )),
            retry_backoff: crate::RetryBackoffConfig::default(),
            logs_root: None,
            workspace_root: None,
            resume_from_checkpoint: None,
            max_loop_restarts: 16,
            control: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineStatus {
    Success,
    Fail,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PipelineRunResult {
    pub run_id: String,
    pub status: PipelineStatus,
    pub failure_reason: Option<String>,
    pub completed_nodes: Vec<String>,
    pub node_outcomes: BTreeMap<String, NodeOutcome>,
    pub context: RuntimeContext,
}
