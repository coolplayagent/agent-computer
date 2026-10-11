use super::*;
use agent_computer_browser::{Action, Actor, Operation};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcquireControl {
    pub connection_session_id: String,
    pub app_generation: i64,
    pub expected_epoch: i64,
    pub lifetime_ms: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseControl {
    pub connection_session_id: String,
    pub control_epoch: i64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserCommand {
    Pages,
    Observe { page_id: String },
    Act { action: Action },
}
impl BrowserCommand {
    pub(super) fn permission(&self) -> RuntimePermission {
        match self {
            Self::Act { .. } => RuntimePermission::Control,
            _ => RuntimePermission::Observe,
        }
    }
    pub(super) fn normalize(self) -> Result<Self> {
        match self {
            Self::Observe { ref page_id } if !agent_computer_browser::identifier(page_id) => {
                Err(Error::InvalidRuntimeRequest)
            }
            Self::Act { action } => Ok(Self::Act {
                action: action
                    .normalize()
                    .map_err(|_| Error::InvalidRuntimeRequest)?,
            }),
            _ => Ok(self),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitBrowserCommand {
    pub connection_session_id: String,
    pub app_generation: i64,
    pub control_epoch: i64,
    pub command: BrowserCommand,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct BrowserControl {
    pub instance_id: String,
    pub control_epoch: i64,
    pub state: String,
    pub owner_principal: String,
    pub owner_connection: String,
    pub expires_at_ms: i64,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct BrowserCommandView {
    pub request_id: String,
    pub instance_id: String,
    pub control_epoch: i64,
    pub state: String,
    pub result: Option<Value>,
    pub result_valid_until_ms: Option<i64>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct Caller {
    pub principal: String,
    pub credential: String,
    pub session: String,
}
impl Caller {
    pub fn actor(&self) -> Actor {
        Actor {
            principal_id: self.principal.clone(),
            connection_session_id: self.session.clone(),
        }
    }
}
/// Original committed dispatch ownership. A database read cannot recreate it.
pub struct BrowserWork {
    pub(super) id: String,
    pub(super) instance: String,
    pub(super) nonce: String,
    pub(super) caller: Caller,
    pub(super) epoch: i64,
    pub(super) command: BrowserCommand,
    pub(super) started: Instant,
}
impl BrowserWork {
    pub fn request_id(&self) -> &str {
        &self.id
    }
    pub fn operation(&self) -> Operation {
        let actor = self.caller.actor();
        match &self.command {
            BrowserCommand::Pages => Operation::Pages { actor },
            BrowserCommand::Observe { page_id } => Operation::Observe {
                actor,
                page_id: page_id.clone(),
            },
            BrowserCommand::Act { action } => Operation::Act {
                actor,
                action_id: self.id.clone(),
                action: action.clone(),
            },
        }
    }
}
/// Original drain claim, consumed only after a correlated private driver barrier.
pub struct BrowserDrain {
    pub(super) epoch: i64,
    pub(super) instance: String,
    pub(super) nonce: String,
}
impl BrowserDrain {
    pub fn control_epoch(&self) -> u64 {
        self.epoch as u64
    }
}
