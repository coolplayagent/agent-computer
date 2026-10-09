//! Fixed data for trusted runtime compilation; these reads grant no authority.
use super::*;
use crate::{
    reconciliation::{ReconcileObject, ReconcileTask},
    runtime::{preparation::PreparationTarget, start::graph::Snapshot},
};
use agent_computer_storage::{PrepareRequest, Prepared};
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionRuntimeInputs {
    pub dispatch: ExecutionDispatchIntent,
    pub preparation: PrepareRequest,
    pub prepared: Prepared,
    pub target: PreparationTarget,
    pub volume: ReconcileTask,
    pub pvc: ReconcileObject,
    pub pv: ReconcileObject,
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| Error::InvalidStoredData)
}

impl Store {
    /// Read the original prepared input and successful Volume effect, including
    /// internal paths and command bytes. No HTTP exposure, catalog re-resolution,
    /// original-starter authorization, mutation or dispatch permit is implied.
    /// Recovery remains possible after a credential/catalog is disabled.
    pub async fn candidate_execution_runtime_inputs(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<ExecutionRuntimeInputs> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        let dispatch = intent(&mut tx, org.as_str(), &record).await?;
        let source = sqlx::query("SELECT p.binding,p.request,p.request_digest,p.receipt,r.snapshot,r.snapshot_digest,i.revision,v.manifest,v.digest AS manifest_digest FROM candidate_writer_leases l JOIN candidate_preparations p USING(organization,request_id) JOIN runtime_start_requests r USING(organization,request_id) JOIN runtime_start_inputs i USING(organization,request_id) JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision WHERE l.organization=$1 AND l.lease_id=$2 AND l.epoch=$3")
            .bind(org.as_str()).bind(&dispatch.execution.lease_id).bind(dispatch.execution.epoch).fetch_one(&mut *tx).await?;
        let target: PreparationTarget = decode(source.try_get("binding")?)?;
        let preparation: PrepareRequest = decode(source.try_get("request")?)?;
        let prepared: Prepared = decode(source.try_get("receipt")?)?;
        let snapshot: Snapshot = decode(source.try_get("snapshot")?)?;
        let request_digest = preparation
            .binding_digest(&target.volume_path, target.writer_uid, target.writer_gid)
            .map_err(|_| Error::InvalidStoredData)?;
        let execution = &dispatch.execution;
        if digest("agent-computer/start-snapshot-v1", &snapshot)?
            != source.try_get::<String, _>("snapshot_digest")?
            || serde_json::to_value(&target).map_err(|_| Error::InvalidStoredData)?
                != dispatch.binding["storage_target"]
            || serde_json::to_value(&prepared).map_err(|_| Error::InvalidStoredData)?
                != dispatch.binding["prepared"]
            || dispatch.binding["input_revision"] != source.try_get::<i64, _>("revision")?
            || preparation.organization != org.as_str()
            || preparation.computer != execution.computer_id
            || preparation.candidate != execution.candidate_id
            || preparation.generation != execution.generation as u64
            || preparation.volume_uid != target.pvc_uid
            || prepared.filesystem_uuid != target.filesystem_uuid
            || prepared.version != 1
            || prepared.data_inode == 0
            || prepared.volume_uid != preparation.volume_uid
            || prepared.path_ref != preparation.path_ref()
            || prepared.quota_bytes != preparation.quota_bytes
            || request_digest != source.try_get::<String, _>("request_digest")?
            || prepared.request_digest != request_digest
            || prepared.manifest_digest != preparation.manifest_digest
            || preparation.manifest_digest != source.try_get::<String, _>("manifest_digest")?
            || serde_json::to_value(&preparation.manifest).map_err(|_| Error::InvalidStoredData)?
                != source.try_get::<Value, _>("manifest")?
            || serde_json::to_value(
                snapshot.resource(DefinitionKind::Sandbox, &execution.sandbox_id)?,
            )
            .map_err(|_| Error::InvalidStoredData)?
                != dispatch.binding["sandbox"]
        {
            return Err(Error::InvalidStoredData);
        }
        let volume = snapshot.resource(DefinitionKind::Volume, &target.volume_id)?;
        if digest(
            "agent-computer/resource-spec-v1",
            &(DefinitionKind::Volume, &volume.spec, &volume.dependencies),
        )? != volume.reference.digest
        {
            return Err(Error::InvalidStoredData);
        }
        // Exact successful effect and both durable UID records, never a current
        // resource head or a plausible object found only by its display name.
        let rows = sqlx::query("SELECT i.operation_id,i.step_id,i.requires_drain,pvc.binding AS pvc,pv.binding AS pv FROM reconcile_intents i JOIN reconciliation_results r USING(organization,step_id) JOIN reconciliation_objects pvc ON pvc.organization=i.organization AND pvc.step_id=i.step_id AND pvc.role='pvc' JOIN reconciliation_objects pv ON pv.organization=i.organization AND pv.step_id=i.step_id AND pv.role='pv' WHERE i.organization=$1 AND i.resource_id=$2 AND i.revision=$3 AND i.state='Succeeded' AND r.receipt->>'backend'='kubernetes_juicefs' AND r.receipt->>'spec_digest'=$4 AND r.receipt->>'object_uid'=$5 AND r.receipt->>'evidence_id'=$6 AND pvc.binding->>'backend'='kubernetes_juicefs' AND pv.binding->>'backend'='kubernetes_juicefs' AND pvc.binding->>'uid'=$5 AND pv.binding->>'uid'=$6 AND pvc.binding->>'scope_uid'=$7 AND pv.binding->>'scope_uid'=$7")
            .bind(org.as_str()).bind(&target.volume_id).bind(volume.reference.revision).bind(&volume.reference.digest).bind(&target.pvc_uid).bind(&target.pv_uid).bind(&target.namespace_uid).fetch_all(&mut *tx).await?;
        let [effect] = rows.as_slice() else {
            return Err(Error::ReferenceUnavailable);
        };
        let result = ExecutionRuntimeInputs {
            preparation,
            prepared,
            target,
            volume: ReconcileTask {
                organization: org.as_str().into(),
                operation_id: effect.try_get("operation_id")?,
                step_id: effect.try_get("step_id")?,
                resource_id: volume.reference.resource_id.clone(),
                revision: volume.reference.revision,
                kind: DefinitionKind::Volume,
                spec_digest: volume.reference.digest.clone(),
                spec: volume.spec.clone(),
                dependencies: volume.dependencies.clone(),
                requires_drain: effect.try_get("requires_drain")?,
            },
            pvc: decode(effect.try_get("pvc")?)?,
            pv: decode(effect.try_get("pv")?)?,
            dispatch,
        };
        tx.commit().await?;
        Ok(result)
    }
}
