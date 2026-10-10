use crate::{Error, INPUT_FRAME_BYTES, Result, identifier};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub organization_id: String,
    pub computer_id: String,
    pub computer_generation: u64,
    pub app_instance_id: String,
    pub app_generation: u64,
    pub profile_owner_id: String,
}
impl Scope {
    pub fn validate(&self) -> Result<()> {
        if ![
            &self.organization_id,
            &self.computer_id,
            &self.app_instance_id,
            &self.profile_owner_id,
        ]
        .into_iter()
        .all(|s| identifier(s))
            || !(1..=MAX_SAFE_INTEGER).contains(&self.computer_generation)
            || !(1..=MAX_SAFE_INTEGER).contains(&self.app_generation)
        {
            return Err(Error::InvalidBinding);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub principal_id: String,
    pub connection_session_id: String,
}
impl Actor {
    pub fn validate(&self) -> Result<()> {
        if !identifier(&self.principal_id) || !identifier(&self.connection_session_id) {
            return Err(Error::InvalidBinding);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub scope: Scope,
    pub actor: Actor,
    pub viewport: Viewport,
    pub runtime_ms: u32,
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        self.actor.validate()?;
        if !(320..=1920).contains(&self.viewport.width)
            || !(240..=1080).contains(&self.viewport.height)
            || !(1000..=300_000).contains(&self.runtime_ms)
        {
            return Err(Error::InvalidBinding);
        }
        Ok(())
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > INPUT_FRAME_BYTES {
            return Err(Error::Limit);
        }
        let binding: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidBinding)?;
        binding.validate()?;
        Ok(binding)
    }
    pub fn json(&self) -> Result<String> {
        self.validate()?;
        serde_json::to_string(self).map_err(|_| Error::InvalidBinding)
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        // This restricted binding contains ASCII identifiers and safe integers,
        // never action floats or arbitrary Unicode. Match the TS canonical form
        // explicitly rather than depending on serde_json's map feature flags.
        let value = serde_json::to_value(self).map_err(|_| Error::InvalidBinding)?;
        let bytes = canonical(&value)?;
        Ok(format!(
            "sha256:{:x}",
            Sha256::new()
                .chain_update(b"agent-computer/browser-bridge/v1\0")
                .chain_update(bytes.as_bytes())
                .finalize()
        ))
    }
}

fn canonical(value: &serde_json::Value) -> Result<String> {
    if let serde_json::Value::Object(map) = value {
        let mut keys: Vec<_> = map.keys().collect();
        keys.sort();
        let fields = keys
            .into_iter()
            .map(|key| {
                Ok(format!(
                    "{}:{}",
                    serde_json::to_string(key).map_err(|_| Error::InvalidBinding)?,
                    canonical(&map[key])?
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!("{{{}}}", fields.join(",")))
    } else {
        serde_json::to_string(value).map_err(|_| Error::InvalidBinding)
    }
}
