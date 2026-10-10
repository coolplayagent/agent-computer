use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmitApp {
    pub connection_session_id: String,
    pub computer_generation: i64,
    pub app_id: String,
    pub expected_app_revision: i64,
    /// Fixed admission-to-expiry interval, including time in the queue. No renewal.
    pub runtime_seconds: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelApp {
    pub app_generation: i64,
    pub expected_revision: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppState {
    Queued,
    Blocked,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInstance {
    pub instance_id: String,
    pub computer_id: String,
    pub computer_generation: i64,
    pub app_id: String,
    pub app_generation: i64,
    pub app_revision: i64,
    pub sandbox_id: String,
    pub profile_id: String,
    pub profile_owner_id: String,
    pub connection_session_id: String,
    pub binding_digest: String,
    pub revision: i64,
    pub state: AppState,
    /// This admission API neither launches an App nor establishes runtime readiness.
    pub ready: bool,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub checked_at_ms: i64,
}
