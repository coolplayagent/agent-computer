use crate::{Error, Result};
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionKind {
    Declaration,
    Volume,
    Workspace,
    Sandbox,
    App,
    Agent,
    Computer,
    StorageClass,
    NetworkPolicy,
    BrowserProfile,
    Secret,
}
impl DefinitionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declaration => "declaration",
            Self::Volume => "volume",
            Self::Workspace => "workspace",
            Self::Sandbox => "sandbox",
            Self::App => "app",
            Self::Agent => "agent",
            Self::Computer => "computer",
            Self::StorageClass => "storage_class",
            Self::NetworkPolicy => "network_policy",
            Self::BrowserProfile => "browser_profile",
            Self::Secret => "secret",
        }
    }
    pub fn catalog(self) -> bool {
        matches!(
            self,
            Self::StorageClass | Self::NetworkPolicy | Self::BrowserProfile | Self::Secret
        )
    }
}
impl std::str::FromStr for DefinitionKind {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        [
            Self::Declaration,
            Self::Volume,
            Self::Workspace,
            Self::Sandbox,
            Self::App,
            Self::Agent,
            Self::Computer,
            Self::StorageClass,
            Self::NetworkPolicy,
            Self::BrowserProfile,
            Self::Secret,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == value)
        .ok_or(Error::InvalidStoredData)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefinitionPermission {
    Create,
    Manage,
    Reference,
}
impl DefinitionPermission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Manage => "manage",
            Self::Reference => "reference",
        }
    }
}

pub struct DefinitionGrant<'a> {
    pub organization: &'a OrganizationId,
    pub principal: &'a PrincipalId,
    pub kind: DefinitionKind,
    /// A literal name or '*'; create permission must use '*'.
    pub name: &'a str,
    pub permission: DefinitionPermission,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Dependency {
    pub kind: DefinitionKind,
    pub resource_id: String,
    pub name: String,
    pub revision: i64,
    pub digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Create,
    Update,
    Unchanged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedResource {
    pub kind: DefinitionKind,
    pub name: String,
    pub resource_id: String,
    pub expected_revision: i64,
    pub revision: i64,
    pub digest: String,
    pub change: Change,
    pub before: Option<Value>,
    pub before_digest: Option<String>,
    pub before_dependencies: Vec<Dependency>,
    pub after: Value,
    pub dependencies: Vec<Dependency>,
    pub requires_drain: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DefinitionPlan {
    pub plan_id: String,
    pub plan_digest: String,
    pub organization: String,
    pub principal: String,
    pub declaration_name: String,
    pub declaration_expected_revision: i64,
    pub definition_digest: String,
    pub expires_at_ms: i64,
    pub resources: Vec<PlannedResource>,
    pub deletes_data: bool,
    pub event_sequence: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DefinitionOperation {
    pub operation_id: String,
    pub plan_id: String,
    pub state: String,
    pub resources: Vec<Dependency>,
    pub event_sequence: i64,
    #[serde(default)]
    pub progress: Vec<crate::reconciliation::IntentProgress>,
    #[serde(default)]
    pub watermark: i64,
}

pub(super) fn random_id(prefix: &str) -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::EntropyUnavailable)?;
    Ok(format!(
        "{prefix}_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
pub(crate) fn digest(domain: &str, value: &impl Serialize) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update([0]);
    hash.update(
        serde_json::to_vec(&serde_json::to_value(value).map_err(|_| Error::InvalidStoredData)?)
            .map_err(|_| Error::InvalidStoredData)?,
    );
    Ok(format!("sha256:{:x}", hash.finalize()))
}
pub(super) fn expected(value: Option<u64>, actual: i64) -> Result<()> {
    match value {
        None if actual > 0 => Err(Error::PreconditionRequired),
        None => Ok(()),
        Some(value) if i64::try_from(value).ok() == Some(actual) => Ok(()),
        _ => Err(Error::RevisionConflict),
    }
}
