use crate::{Error, Result, auth::ServiceScope};
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Computer,
    Workspace,
    App,
    BrowserProfile,
}
impl RuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Computer => "computer",
            Self::Workspace => "workspace",
            Self::App => "app",
            Self::BrowserProfile => "browser_profile",
        }
    }
}
impl std::str::FromStr for RuntimeKind {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        [
            Self::Computer,
            Self::Workspace,
            Self::App,
            Self::BrowserProfile,
        ]
        .into_iter()
        .find(|k| k.as_str() == value)
        .ok_or(Error::InvalidRuntimeRequest)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePermission {
    Connect,
    Read,
    Observe,
    #[serde(rename = "app.use")]
    AppUse,
    Activate,
    Execute,
    Modify,
    Control,
    Publish,
    Manage,
    Delete,
}
impl RuntimePermission {
    pub const ALL: [Self; 11] = [
        Self::Connect,
        Self::Read,
        Self::Observe,
        Self::AppUse,
        Self::Activate,
        Self::Execute,
        Self::Modify,
        Self::Control,
        Self::Publish,
        Self::Manage,
        Self::Delete,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Read => "read",
            Self::Observe => "observe",
            Self::AppUse => "app.use",
            Self::Activate => "activate",
            Self::Execute => "execute",
            Self::Modify => "modify",
            Self::Control => "control",
            Self::Publish => "publish",
            Self::Manage => "manage",
            Self::Delete => "delete",
        }
    }
    pub fn scope(self) -> ServiceScope {
        match self {
            Self::Connect => ServiceScope::RuntimeConnect,
            Self::Read => ServiceScope::RuntimeRead,
            Self::Observe => ServiceScope::RuntimeObserve,
            Self::AppUse => ServiceScope::RuntimeAppUse,
            Self::Activate => ServiceScope::RuntimeActivate,
            Self::Execute => ServiceScope::RuntimeExecute,
            Self::Modify => ServiceScope::RuntimeModify,
            Self::Control => ServiceScope::RuntimeControl,
            Self::Publish => ServiceScope::RuntimePublish,
            Self::Manage => ServiceScope::RuntimeManage,
            Self::Delete => ServiceScope::RuntimeDelete,
        }
    }
    pub(crate) fn accepts(self, kind: RuntimeKind) -> bool {
        use RuntimeKind::*;
        match kind {
            Computer => true,
            Workspace => matches!(
                self,
                Self::Read | Self::Modify | Self::Publish | Self::Manage | Self::Delete
            ),
            App => matches!(
                self,
                Self::Read
                    | Self::Observe
                    | Self::AppUse
                    | Self::Activate
                    | Self::Control
                    | Self::Manage
                    | Self::Delete
            ),
            BrowserProfile => matches!(
                self,
                Self::Read | Self::AppUse | Self::Manage | Self::Delete
            ),
        }
    }
}
impl std::str::FromStr for RuntimePermission {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str() == value)
            .ok_or(Error::InvalidRuntimeRequest)
    }
}

pub struct RuntimeGrant<'a> {
    pub organization: &'a OrganizationId,
    pub principal: &'a PrincipalId,
    pub kind: RuntimeKind,
    pub resource_id: &'a str,
    pub permission: RuntimePermission,
    /// Required for activate grants, absent for other permissions and revocation.
    pub max_runtime_seconds: Option<u32>,
}

/// Every required resource/action must be checked in the effect admission transaction.
/// Calling a standalone check does not issue a reusable permit or modification lease.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequirement {
    pub kind: RuntimeKind,
    pub resource_id: String,
    pub permission: RuntimePermission,
    pub runtime_seconds: Option<u32>,
}
impl RuntimeRequirement {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_target(self.kind, &self.resource_id, self.permission)?;
        if (self.permission == RuntimePermission::Activate
            && !self
                .runtime_seconds
                .is_some_and(|n| (1..=86400).contains(&n)))
            || (self.permission != RuntimePermission::Activate && self.runtime_seconds.is_some())
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        Ok(())
    }
}
pub(crate) fn validate_target(
    kind: RuntimeKind,
    id: &str,
    permission: RuntimePermission,
) -> Result<()> {
    if agent_computer_core::identity::ComputerId::new(id).is_err() || !permission.accepts(kind) {
        return Err(Error::InvalidRuntimeRequest);
    }
    Ok(())
}

/// Current permissions intersected with this credential's scopes. This view is
/// informational and expires immediately as an authorization decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeAccess {
    pub kind: RuntimeKind,
    pub resource_id: String,
    pub permissions: Vec<RuntimePermission>,
    pub max_runtime_seconds: Option<u32>,
    pub checked_at_ms: i64,
}
