use crate::{Actor, Error, Result, Scope, identifier};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    CreatePage {},
    ClosePage {
        page_id: String,
        observation_id: String,
    },
    Navigate {
        page_id: String,
        observation_id: String,
        url: String,
    },
    Click {
        page_id: String,
        observation_id: String,
        element_id: String,
    },
    ClickAt {
        page_id: String,
        observation_id: String,
        x: f64,
        y: f64,
    },
    Type {
        page_id: String,
        observation_id: String,
        element_id: String,
        text: String,
    },
    Select {
        page_id: String,
        observation_id: String,
        element_id: String,
        value: String,
    },
    Key {
        page_id: String,
        observation_id: String,
        key: String,
    },
    Scroll {
        page_id: String,
        observation_id: String,
        delta_x: f64,
        delta_y: f64,
    },
}
fn text(value: &str, limit: usize) -> bool {
    value.len() <= limit && !value.contains('\0')
}
fn number(value: f64, low: f64, high: f64) -> bool {
    value.is_finite() && (low..=high).contains(&value)
}
impl Action {
    pub fn normalize(mut self) -> Result<Self> {
        let (page, observation) = match &self {
            Self::CreatePage {} => return Ok(self),
            Self::ClosePage {
                page_id,
                observation_id,
            }
            | Self::Navigate {
                page_id,
                observation_id,
                ..
            }
            | Self::Click {
                page_id,
                observation_id,
                ..
            }
            | Self::ClickAt {
                page_id,
                observation_id,
                ..
            }
            | Self::Type {
                page_id,
                observation_id,
                ..
            }
            | Self::Select {
                page_id,
                observation_id,
                ..
            }
            | Self::Key {
                page_id,
                observation_id,
                ..
            }
            | Self::Scroll {
                page_id,
                observation_id,
                ..
            } => (page_id, observation_id),
        };
        if !identifier(page) || !identifier(observation) {
            return Err(Error::Protocol);
        }
        let valid = match &mut self {
            Self::Navigate { url, .. } => {
                if !text(url, 4096) {
                    return Err(Error::Protocol);
                }
                let parsed = url::Url::parse(url).map_err(|_| Error::Protocol)?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                {
                    return Err(Error::Protocol);
                }
                *url = parsed.to_string();
                true
            }
            Self::Click { element_id, .. } => identifier(element_id),
            Self::ClickAt { x, y, .. } => number(*x, 0.0, 1920.0) && number(*y, 0.0, 1080.0),
            Self::Type {
                element_id,
                text: value,
                ..
            } => identifier(element_id) && text(value, 16384),
            Self::Select {
                element_id, value, ..
            } => identifier(element_id) && text(value, 1024),
            Self::Key { key, .. } => matches!(
                key.as_str(),
                "Enter"
                    | "Tab"
                    | "Escape"
                    | "Backspace"
                    | "Delete"
                    | "ArrowLeft"
                    | "ArrowRight"
                    | "ArrowUp"
                    | "ArrowDown"
                    | "Home"
                    | "End"
                    | "PageUp"
                    | "PageDown"
                    | "Space"
            ),
            Self::Scroll {
                delta_x, delta_y, ..
            } => number(*delta_x, -2000.0, 2000.0) && number(*delta_y, -2000.0, 2000.0),
            Self::CreatePage {} | Self::ClosePage { .. } => true,
        };
        if !valid {
            return Err(Error::Protocol);
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Operation {
    Launch,
    Pages {
        actor: Actor,
    },
    Observe {
        actor: Actor,
        page_id: String,
    },
    Act {
        actor: Actor,
        action_id: String,
        action: Action,
    },
    Close,
}
impl Operation {
    pub(crate) fn normalize(self) -> Result<Self> {
        match self {
            Self::Launch | Self::Close => Ok(self),
            Self::Pages { actor } => {
                actor.validate()?;
                Ok(Self::Pages { actor })
            }
            Self::Observe { actor, page_id } => {
                actor.validate()?;
                if !identifier(&page_id) {
                    return Err(Error::Protocol);
                }
                Ok(Self::Observe { actor, page_id })
            }
            Self::Act {
                actor,
                action_id,
                action,
            } => {
                actor.validate()?;
                if !identifier(&action_id) {
                    return Err(Error::Protocol);
                }
                Ok(Self::Act {
                    actor,
                    action_id,
                    action: action.normalize()?,
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Launch,
    Observe,
    Control,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationRequest {
    pub scope: Scope,
    pub actor: Actor,
    pub permission: Permission,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub scope: Scope,
    pub actor: Actor,
    pub permission: Permission,
    pub control_epoch: u64,
    pub valid_for_ms: u32,
}
impl Grant {
    pub(crate) fn matches(&self, request: &AuthorizationRequest) -> bool {
        self.scope == request.scope
            && self.actor == request.actor
            && self.permission == request.permission
            && self.control_epoch < (1 << 53)
            && (self.permission != Permission::Control || self.control_epoch > 0)
            && self.valid_for_ms > 0
            && self.valid_for_ms
                <= if self.permission == Permission::Launch {
                    300000
                } else {
                    30000
                }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionIntent {
    pub scope: Scope,
    pub actor: Actor,
    pub control_epoch: u64,
    pub action_id: String,
    /// Opaque driver digest; persist and compare the entire typed input as well.
    pub input_digest: String,
    pub action: Action,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Delivered,
    Unknown,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionReceipt {
    pub action_id: String,
    pub input_digest: String,
    pub app_instance_id: String,
    pub app_generation: u64,
    pub control_epoch: u64,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
}
impl ActionReceipt {
    pub(crate) fn matches(&self, intent: &ActionIntent) -> bool {
        self.action_id == intent.action_id
            && self.input_digest == intent.input_digest
            && self.app_instance_id == intent.scope.app_instance_id
            && self.app_generation == intent.scope.app_generation
            && self.control_epoch == intent.control_epoch
            && self.page_id.as_deref().is_none_or(identifier)
    }
}
pub(crate) fn digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Clone, Debug)]
pub enum Callback {
    Authorize(AuthorizationRequest),
    Begin(ActionIntent),
    Complete(ActionReceipt),
}
pub enum Decision {
    Grant(Grant),
    Dispatch,
    Replay(ActionReceipt),
    Recorded,
    Deny,
}
