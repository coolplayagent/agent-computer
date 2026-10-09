use crate::{
    Error, Result,
    plans::{DefinitionKind, Dependency},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerId(String);
impl WorkerId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !identifier(&value) {
            return Err(Error::InvalidReconcileResult);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
pub(super) fn identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub(super) fn duration_ms(value: Duration, maximum: u64) -> Result<i64> {
    if !(1..=maximum).contains(&value.as_secs()) || value.subsec_nanos() != 0 {
        return Err(Error::InvalidReconcileResult);
    }
    Ok(value.as_secs() as i64 * 1000)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IntentState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileReason {
    AuthorizationRevoked,
    ReferenceUnavailable,
    BackendUnavailable,
    BackendTransient,
    BackendRejected,
    RuntimeUnknown,
    ResourceConflict,
    OperatorAbandoned,
}
impl ReconcileReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizationRevoked => "authorization_revoked",
            Self::ReferenceUnavailable => "reference_unavailable",
            Self::BackendUnavailable => "backend_unavailable",
            Self::BackendTransient => "backend_transient",
            Self::BackendRejected => "backend_rejected",
            Self::RuntimeUnknown => "runtime_unknown",
            Self::ResourceConflict => "resource_conflict",
            Self::OperatorAbandoned => "operator_abandoned",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IntentProgress {
    pub step_id: String,
    pub resource_id: String,
    pub revision: i64,
    pub state: IntentState,
    pub attempts: i64,
    pub dispatch_started: bool,
    pub reason: Option<ReconcileReason>,
    pub available_at_ms: i64,
    pub event_sequence: i64,
}

#[derive(Debug, Serialize)]
pub struct ReconciliationStatus {
    pub operation_id: String,
    pub state: String,
    pub watermark: i64,
    pub progress: Vec<IntentProgress>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ClaimMode {
    Execute,
    /// A previous lease may have dispatched. Inspect the original effect; never
    /// repeat creation/action merely because its acknowledgement was lost.
    Observe,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReconcileTask {
    pub organization: String,
    pub operation_id: String,
    /// Stable across retries, worker restarts and lease epochs.
    pub step_id: String,
    pub resource_id: String,
    pub revision: i64,
    pub kind: DefinitionKind,
    pub spec_digest: String,
    pub spec: Value,
    pub dependencies: Vec<Dependency>,
    pub requires_drain: bool,
}

/// An in-process handle issued only by a successful claim transaction. Its epoch
/// fences database writes, not processes/nodes. Never accept handles over HTTP.
#[derive(Clone, Debug)]
pub struct ReconcileLease {
    pub(super) task: ReconcileTask,
    pub(super) owner: WorkerId,
    pub(super) epoch: i64,
    pub(super) until_ms: i64,
    pub(super) mode: ClaimMode,
}
impl ReconcileLease {
    pub fn task(&self) -> &ReconcileTask {
        &self.task
    }
    pub fn owner(&self) -> &WorkerId {
        &self.owner
    }
    pub fn epoch(&self) -> i64 {
        self.epoch
    }
    pub fn expires_at_ms(&self) -> i64 {
        self.until_ms
    }
    pub fn mode(&self) -> ClaimMode {
        self.mode
    }
}

#[derive(Debug)]
pub enum ClaimOutcome {
    Idle,
    Claimed(Box<ReconcileLease>),
    Blocked(IntentProgress),
}

/// Evidence of one durable dispatch admission, not proof of external execution.
/// It is deliberately neither Clone nor Deserialize. Backends still need online
/// authority/instance fencing and must use task.step_id as the effect identity.
#[derive(Debug)]
pub struct DispatchPermit {
    pub(super) task: ReconcileTask,
    pub(super) epoch: i64,
}
impl DispatchPermit {
    pub fn task(&self) -> &ReconcileTask {
        &self.task
    }
    pub fn epoch(&self) -> i64 {
        self.epoch
    }
}

/// Supplied by a trusted adapter after verifying the backend's actual object UID,
/// stable effect labels and pinned spec. This envelope alone proves no runtime fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EffectReceipt {
    pub step_id: String,
    pub resource_id: String,
    pub revision: i64,
    pub spec_digest: String,
    pub backend: String,
    pub object_uid: String,
    pub evidence_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ReconcileOutcome {
    Applied {
        receipt: EffectReceipt,
    },
    Retry {
        reason: ReconcileReason,
        delay_seconds: u32,
    },
    Blocked {
        reason: ReconcileReason,
    },
    /// A terminal failure is legal only before dispatch. Unknown effects remain
    /// blocked/observable and continue to exclude newer work on this resource.
    Failed {
        reason: ReconcileReason,
    },
}
