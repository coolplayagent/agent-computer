use super::*;
use crate::plans::{DefinitionKind, Dependency};
use agent_computer_browser::{Actor, Binding, Scope, Viewport};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{InstanceIdentity, NodeIdentity};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserDeployment {
    pub namespace: String,
    pub namespace_uid: String,
    pub approved_image: String,
    pub node: NodeIdentity,
    pub viewport: Viewport,
}
pub(super) fn compile(
    org: &str,
    app: &AppInstance,
    pinned: &Value,
    deployment: &BrowserDeployment,
    remaining: u32,
) -> Result<BrowserPlan> {
    valid(&deployment.namespace_uid)?;
    let deps: Vec<Dependency> = serde_json::from_value(pinned["sandbox"]["dependencies"].clone())
        .map_err(|_| Error::InvalidStoredData)?;
    let policy = pinned["sandbox"]["spec"]["networkPolicyRef"]
        .as_str()
        .ok_or(Error::InvalidStoredData)?;
    if !deps.iter().any(|d| {
        d.kind == DefinitionKind::NetworkPolicy && policy == format!("id:{}", d.resource_id)
    }) {
        return Err(Error::ReferenceUnavailable);
    }
    let mut sandbox = pinned["sandbox"]["spec"].clone();
    sandbox
        .as_object_mut()
        .ok_or(Error::InvalidStoredData)?
        .insert("name".into(), json!("browser"));
    let declaration = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"browser-controller"},"spec":{"sandboxes":[sandbox]}});
    let definition = validate_bytes(
        &serde_json::to_vec(&declaration).map_err(|_| Error::InvalidStoredData)?,
        Format::Json,
    )
    .map_err(|_| Error::ReferenceUnavailable)?;
    let binding = Binding {
        scope: Scope {
            organization_id: org.into(),
            computer_id: app.computer_id.clone(),
            computer_generation: app.computer_generation as u64,
            app_instance_id: app.instance_id.clone(),
            app_generation: app.app_generation as u64,
            profile_owner_id: app.profile_owner_id.clone(),
        },
        actor: Actor {
            principal_id: app.profile_owner_id.clone(),
            connection_session_id: app.connection_session_id.clone(),
        },
        viewport: deployment.viewport,
        runtime_ms: remaining,
    };
    BrowserPlan::new(
        &definition,
        "browser",
        InstanceIdentity {
            organization: org.into(),
            computer: app.computer_id.clone(),
            sandbox: app.sandbox_id.clone(),
            instance: app.instance_id.clone(),
            generation: app.app_generation as u64,
            spec_revision: pinned["sandbox"]["reference"]["revision"]
                .as_u64()
                .ok_or(Error::InvalidStoredData)?,
        },
        &deployment.namespace,
        &deployment.approved_image,
        binding,
        deployment.node.clone(),
    )
    .map_err(|_| Error::ReferenceUnavailable)
}
