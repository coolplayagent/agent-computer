use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    /// The initial control revision is 1, independently of definition revisions.
    pub expected_revision: i64,
    pub expected_spec_revision: i64,
    pub max_runtime_seconds: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelQueuedStart {
    pub expected_revision: i64,
    pub request_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StartState {
    Queued,
    Preparing,
    Prepared,
    Sealing,
    Sealed,
    Cancelled,
    Stopped,
}

/// Immutable admission receipt. Obtain current state from computer_runtime().
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartReceipt {
    pub request_id: String,
    pub computer_id: String,
    pub control_revision: i64,
    pub spec_revision: i64,
    pub generation: i64,
    pub candidate_id: String,
    pub snapshot_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_manifest_digest: Option<String>,
    pub state: StartState,
    pub reason: String,
    pub cpu_millis: i64,
    pub memory_mib: i64,
    pub storage_bytes: i64,
    pub max_runtime_seconds: u32,
    pub queue_deadline_at_ms: i64,
    pub event_sequence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComputerRuntime {
    pub computer_id: String,
    pub revision: i64,
    pub generation: i64,
    pub active_request: Option<String>,
    pub start_state: Option<StartState>,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_receipt: Option<ComputerStopReceipt>,
}

/// Stop a verified file-only checkpoint or an undispatched prepared Candidate.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopPreparedComputer {
    pub expected_revision: i64,
    pub request_id: String,
}

/// Historical receipt, not a physical fencing certificate for dispatched work.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComputerStopReceipt {
    pub computer_id: String,
    pub request_id: String,
    pub generation: i64,
    pub candidate_id: String,
    pub control_revision: i64,
    pub input_revision: i64,
    pub input_manifest_digest: String,
    pub retained_storage_bytes: i64,
    pub proof: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<WorkspaceCheckpoint>,
    pub stopped_at_ms: i64,
    pub event_sequence: i64,
}

/// Complete only for Computers with no declared Apps or unfinished executions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCheckpoint {
    pub artifact_id: String,
    pub input_revision: i64,
    pub manifest_digest: String,
    pub snapshot_digest: String,
    pub computer_spec_digest: String,
    pub app_states: Vec<serde_json::Value>,
    pub unfinished_execution_ids: Vec<String>,
}
