use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{
    CandidateMount, Client, InstanceIdentity, StartupSandboxPlan, volume::StorageClassBinding,
};
use agent_computer_store::{
    Error, Result,
    plans::{DefinitionKind, Dependency},
    runtime::writers::ExecutionRuntimeInputs,
};
use serde_json::json;

/// Deterministic compilation for review/recovery, never a dispatch permit. All
/// versions come from the admitted snapshot; operator image/catalog bindings must
/// still match it. The caller must verify actual mount and current authority.
pub fn compile_plan(
    inputs: &ExecutionRuntimeInputs,
    client: &Client,
    storage: &StorageClassBinding,
    approved_image: &str,
) -> Result<StartupSandboxPlan> {
    compile(inputs, client, storage, approved_image, None)
}
pub(super) fn compile(
    inputs: &ExecutionRuntimeInputs,
    client: &Client,
    storage: &StorageClassBinding,
    approved_image: &str,
    fence: Option<(
        agent_computer_fence::MountReference,
        agent_computer_kubernetes::NodeIdentity,
    )>,
) -> Result<StartupSandboxPlan> {
    let volume = crate::compile_volume(&inputs.volume, client, storage)
        .map_err(|_| Error::ReferenceUnavailable)?;
    if inputs.target.namespace_uid != client.namespace_uid()
        || inputs.pvc.name != volume.name()
        || inputs.pv.name != inputs.target.volume_path
        || inputs.target.writer_uid != 1000
        || inputs.target.writer_gid != 1000
    {
        return Err(Error::ReferenceUnavailable);
    }
    let dispatch = &inputs.dispatch;
    let execution = &dispatch.execution;
    let pinned = &dispatch.binding["sandbox"];
    let deps: Vec<Dependency> = serde_json::from_value(pinned["dependencies"].clone())
        .map_err(|_| Error::InvalidStoredData)?;
    let policy = pinned["spec"]["networkPolicyRef"]
        .as_str()
        .ok_or(Error::InvalidStoredData)?;
    if !deps.iter().any(|d| {
        d.kind == DefinitionKind::NetworkPolicy && policy == format!("id:{}", d.resource_id)
    }) {
        return Err(Error::ReferenceUnavailable);
    }
    let mut sandbox = pinned["spec"].clone();
    sandbox
        .as_object_mut()
        .ok_or(Error::InvalidStoredData)?
        .insert("name".into(), json!("sandbox"));
    let declaration = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"execution-worker"},"spec":{"sandboxes":[sandbox]}});
    let definition = validate_bytes(
        &serde_json::to_vec(&declaration).map_err(|_| Error::InvalidStoredData)?,
        Format::Json,
    )
    .map_err(|_| Error::ReferenceUnavailable)?;
    let candidate = CandidateMount::new(
        volume,
        &inputs.target.namespace_uid,
        &inputs.target.pv_uid,
        &inputs.target.volume_path,
        &inputs.preparation,
        &inputs.prepared,
    )
    .map_err(|_| Error::ReferenceUnavailable)?;
    let candidate = if let Some((reference, node)) = fence {
        candidate
            .with_fence(reference, node)
            .map_err(|_| Error::ReferenceUnavailable)?
    } else {
        candidate
    };
    StartupSandboxPlan::with_candidate(
        &definition,
        "sandbox",
        InstanceIdentity {
            organization: dispatch.organization.clone(),
            computer: execution.computer_id.clone(),
            sandbox: execution.sandbox_id.clone(),
            instance: execution.execution_id.clone(),
            generation: execution.generation as u64,
            spec_revision: execution.sandbox_revision as u64,
        },
        client.namespace(),
        approved_image,
        dispatch.bootstrap()?,
        candidate,
    )
    .map_err(|_| Error::ReferenceUnavailable)
}
