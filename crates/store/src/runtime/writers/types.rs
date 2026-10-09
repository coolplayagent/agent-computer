use agent_computer_storage::Prepared;
use serde::{Deserialize, Serialize};

pub(super) fn duration() -> u32 {
    30
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriterScope {
    Modify,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquireWriterLease {
    pub scope: WriterScope,
    pub connection_session_id: String,
    pub candidate_id: String,
    pub generation: i64,
    #[serde(default = "duration")]
    pub duration_seconds: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterLeaseCommand {
    pub connection_session_id: String,
    pub generation: i64,
    pub epoch: i64,
    pub expected_revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewWriterLease {
    pub lease: WriterLeaseCommand,
    #[serde(default = "duration")]
    pub duration_seconds: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WriterLeaseState {
    Held,
    Draining,
    Released,
}

/// Current ownership metadata, not a filesystem capability or stopped-process proof.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WriterLease {
    pub lease_id: String,
    pub computer_id: String,
    pub candidate_id: String,
    pub connection_session_id: String,
    pub generation: i64,
    pub epoch: i64,
    pub revision: i64,
    pub state: WriterLeaseState,
    pub expires_at_ms: i64,
    pub checked_at_ms: i64,
    pub dispatch_recorded: bool,
    pub release_proof: Option<String>,
    pub file_edit: Option<agent_computer_storage::files::FileEditReport>,
}

/// Trusted integration input; never accepted by the HTTP lease routes.
pub struct WriterDispatch<'a> {
    pub dispatch_id: &'a str,
    pub input_digest: &'a str,
}

/// One-time admission returned only after committing the dispatch journal.
/// No Clone/Deserialize and no retry can reissue this permit. The bounded file
/// adapter enforces a conservative local deadline; general process supervision
/// remains separate because database time does not stop a process.
pub struct WriterDispatchPermit {
    pub(super) organization: String,
    pub(super) deadline: std::time::Instant,
    pub(super) lease: WriterLease,
    pub(super) dispatch_id: String,
    pub(super) input_digest: String,
    pub(super) prepared: Prepared,
}
impl WriterDispatchPermit {
    pub fn lease(&self) -> &WriterLease {
        &self.lease
    }
    pub fn dispatch_id(&self) -> &str {
        &self.dispatch_id
    }
    pub fn input_digest(&self) -> &str {
        &self.input_digest
    }
    pub fn prepared(&self) -> &Prepared {
        &self.prepared
    }
}
