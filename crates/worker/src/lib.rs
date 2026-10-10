//! Trusted, one-claim reconciliation workers. Runtime activation and storage
//! mounting are separate admissions; a definition is never an implicit Pod start.
#![forbid(unsafe_code)]
pub mod candidate;
pub mod execution;
pub mod files;

use agent_computer_core::identity::OrganizationId;
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{
    Client, Error as BackendError,
    volume::{StorageClassBinding, VolumeIdentity, VolumePlan},
};
use agent_computer_store::{Store, plans::DefinitionKind, reconciliation::*};
use serde::Serialize;
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkResult {
    Idle,
    Progress { progress: IntentProgress },
}

/// Provision one first-revision Volume, or observe a previous dispatch. Only the
/// original operation's current authority can claim, record UIDs or complete it.
pub async fn reconcile_volume_once(
    store: &Store,
    client: &Client,
    storage: &StorageClassBinding,
    org: &OrganizationId,
    worker: &WorkerId,
) -> agent_computer_store::Result<WorkResult> {
    let lease = match store
        .claim_reconciliation_kind(
            org,
            worker,
            Duration::from_secs(180),
            DefinitionKind::Volume,
        )
        .await?
    {
        ClaimOutcome::Idle => return Ok(WorkResult::Idle),
        ClaimOutcome::Blocked(progress) => return Ok(WorkResult::Progress { progress }),
        ClaimOutcome::Claimed(lease) => lease,
    };
    let outcome = perform(store, client, storage, &lease).await?;
    let progress = store.finish_reconciliation(&lease, outcome).await?;
    Ok(WorkResult::Progress { progress })
}

pub(crate) fn compile_volume(
    task: &ReconcileTask,
    client: &Client,
    storage: &StorageClassBinding,
) -> Result<VolumePlan, BackendError> {
    // Verify the mapping is a pinned dependency of the admitted spec, not a name
    // supplied by an untrusted caller or a mutable catalog lookup.
    let reference = task.spec["storageClass"]
        .as_str()
        .ok_or(BackendError::UnsupportedVolume)?;
    if reference != storage.reference
        || !task.dependencies.iter().any(|d| {
            d.kind == DefinitionKind::StorageClass && reference == format!("id:{}", d.resource_id)
        })
        || task.requires_drain
    {
        return Err(BackendError::UnsupportedVolume);
    }
    let mut volume = task.spec.clone();
    volume
        .as_object_mut()
        .ok_or(BackendError::UnsupportedVolume)?
        .insert("name".into(), json!("volume"));
    let declaration = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"worker"},"spec":{"volumes":[volume]}});
    let validated = validate_bytes(
        &serde_json::to_vec(&declaration).map_err(|_| BackendError::UnsupportedVolume)?,
        Format::Json,
    )
    .map_err(|_| BackendError::UnsupportedVolume)?;
    VolumePlan::new(
        &validated,
        "volume",
        VolumeIdentity {
            organization: task.organization.clone(),
            resource_id: task.resource_id.clone(),
            revision: task.revision,
            step_id: task.step_id.clone(),
            spec_digest: task.spec_digest.clone(),
        },
        client.namespace(),
        storage.clone(),
    )
}

async fn perform(
    store: &Store,
    client: &Client,
    storage: &StorageClassBinding,
    lease: &ReconcileLease,
) -> agent_computer_store::Result<ReconcileOutcome> {
    let plan = match compile_volume(lease.task(), client, storage) {
        Ok(plan) => plan,
        Err(e) => return Ok(backend_failure(e)),
    };
    let known = store.reconciliation_object(lease, "pvc").await?;
    let known_pv = store.reconciliation_object(lease, "pv").await?;
    if known.as_ref().is_some_and(|b| {
        b.backend != "kubernetes_juicefs"
            || b.name != plan.name()
            || b.scope_uid != client.namespace_uid()
    }) || known_pv
        .as_ref()
        .is_some_and(|b| b.backend != "kubernetes_juicefs" || b.scope_uid != client.namespace_uid())
    {
        return Ok(blocked(ReconcileReason::ResourceConflict));
    }
    let claim = if lease.mode() == ClaimMode::Execute {
        if let Err(e) = client.probe_volume(&plan).await {
            return Ok(backend_failure(e));
        }
        let permit = store.begin_reconciliation_dispatch(lease).await?;
        // Do not retain or re-use a dispatch permit across network failures.
        debug_assert_eq!(permit.task().step_id, lease.task().step_id);
        match client.create_volume(&plan).await {
            Ok(claim) => Some(claim),
            Err(BackendError::ExistingObject | BackendError::MutationUnconfirmed) => {
                return Ok(retry(ReconcileReason::RuntimeUnknown));
            }
            Err(e) => return Ok(backend_failure(e)),
        }
    } else {
        match client
            .observe_volume_claim(&plan, known.as_ref().map(|b| b.uid.as_str()))
            .await
        {
            Ok(claim) => claim,
            Err(e) => return Ok(backend_failure(e)),
        }
    };
    let Some(claim) = claim else {
        return Ok(blocked(ReconcileReason::RuntimeUnknown));
    };
    store
        .record_reconciliation_object(
            lease,
            "pvc",
            &ReconcileObject {
                backend: "kubernetes_juicefs".into(),
                name: claim.name().into(),
                uid: claim.uid().into(),
                scope_uid: claim.namespace_uid().into(),
            },
        )
        .await?;
    let volume = match client
        .observe_bound_volume(&plan, &claim, known_pv.as_ref().map(|b| b.uid.as_str()))
        .await
    {
        Ok(Some(volume)) => volume,
        Ok(None) => return Ok(retry(ReconcileReason::BackendTransient)),
        Err(e) => return Ok(backend_failure(e)),
    };
    if known_pv.as_ref().is_some_and(|b| b.name != volume.name()) {
        return Ok(blocked(ReconcileReason::ResourceConflict));
    }
    store
        .record_reconciliation_object(
            lease,
            "pv",
            &ReconcileObject {
                backend: "kubernetes_juicefs".into(),
                name: volume.name().into(),
                uid: volume.uid().into(),
                scope_uid: claim.namespace_uid().into(),
            },
        )
        .await?;
    let task = lease.task();
    Ok(ReconcileOutcome::Applied {
        receipt: EffectReceipt {
            step_id: task.step_id.clone(),
            resource_id: task.resource_id.clone(),
            revision: task.revision,
            spec_digest: task.spec_digest.clone(),
            backend: "kubernetes_juicefs".into(),
            object_uid: claim.uid().into(),
            evidence_id: volume.uid().into(),
        },
    })
}

fn retry(reason: ReconcileReason) -> ReconcileOutcome {
    ReconcileOutcome::Retry {
        reason,
        delay_seconds: 2,
    }
}
fn blocked(reason: ReconcileReason) -> ReconcileOutcome {
    ReconcileOutcome::Blocked { reason }
}
fn backend_failure(error: BackendError) -> ReconcileOutcome {
    match error {
        BackendError::Transport | BackendError::ApiRejected => {
            retry(ReconcileReason::BackendTransient)
        }
        BackendError::ExistingObject | BackendError::IdentityMismatch => {
            blocked(ReconcileReason::ResourceConflict)
        }
        BackendError::MutationUnconfirmed => retry(ReconcileReason::RuntimeUnknown),
        BackendError::PreconditionFailed => blocked(ReconcileReason::ReferenceUnavailable),
        _ => blocked(ReconcileReason::BackendRejected),
    }
}

pub mod artifacts;
