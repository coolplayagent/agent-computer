use super::RuntimePermission;
use serde::{Deserialize, Serialize};

fn default_lifetime() -> u32 {
    900
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectRequest {
    pub requested_capabilities: Vec<RuntimePermission>,
    #[serde(default = "default_lifetime")]
    pub lifetime_seconds: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ConnectionState {
    Active,
    Expired,
    Revoked,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionActivity {
    Idle,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionVisibility {
    Hidden,
    Visible,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionHeartbeat {
    pub expected_revision: i64,
    pub activity: ConnectionActivity,
    pub visibility: ConnectionVisibility,
}

/// Current own-session metadata. Capabilities are an informational intersection
/// on the Computer only; they are not a lease or a grant on related resources.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectionSession {
    pub session_id: String,
    pub computer_id: String,
    pub principal_id: String,
    pub principal_kind: String,
    pub revision: i64,
    pub revocation_revision: i64,
    pub state: ConnectionState,
    pub requested_capabilities: Vec<RuntimePermission>,
    pub capabilities: Vec<RuntimePermission>,
    pub max_runtime_seconds: Option<u32>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub checked_at_ms: i64,
    pub last_seen_at_ms: i64,
    pub activity: ConnectionActivity,
    pub visibility: ConnectionVisibility,
}
