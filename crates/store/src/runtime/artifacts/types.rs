use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitArtifact {
    pub request_id: String,
    pub expected_revision: i64,
    pub base_revision: i64,
    pub base_manifest: String,
    pub publish_current: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ArtifactState {
    Capturing,
    Committed,
    Conflict,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactCommit {
    pub commit_id: String,
    pub workspace_id: String,
    pub computer_id: String,
    pub candidate_id: String,
    pub generation: i64,
    pub creator: String,
    pub state: ArtifactState,
    pub base_revision: i64,
    pub base_manifest: String,
    pub publish_current: bool,
    pub input_revision: Option<i64>,
    pub manifest_digest: Option<String>,
    pub published_at_ms: Option<i64>,
}
/// Only a trusted claimed worker may capture/publish the sealed Candidate.
pub struct ArtifactLease {
    pub(super) org: String,
    pub(super) commit: String,
    pub(super) epoch: i64,
    pub(super) owner: String,
    pub(super) prepared: agent_computer_storage::Prepared,
    pub(super) target: super::super::preparation::PreparationTarget,
    pub(super) capture: Option<Bundle>,
}
impl ArtifactLease {
    pub fn organization(&self) -> &str {
        &self.org
    }
    pub fn commit_id(&self) -> &str {
        &self.commit
    }
    pub fn prepared(&self) -> &agent_computer_storage::Prepared {
        &self.prepared
    }
    pub fn target(&self) -> &super::super::preparation::PreparationTarget {
        &self.target
    }
    pub fn capture(&self) -> Option<&Bundle> {
        self.capture.as_ref()
    }
}
