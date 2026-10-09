//! Trusted Candidate preparation protocol. Lease expiry permits observation only
//! after dispatch; it never proves a filesystem writer stopped.
use super::{start::graph, *};
use crate::{
    plans::DefinitionKind,
    reconciliation::{ClaimMode, WorkerId},
};
use agent_computer_core::identity::{ComputerId, OrganizationId};
use agent_computer_storage::{Manifest, PrepareRequest, Prepared};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationTarget {
    pub volume_id: String,
    pub namespace_uid: String,
    pub pvc_uid: String,
    pub pv_uid: String,
    pub filesystem_uuid: String,
    pub volume_path: String,
    pub writer_uid: u32,
    pub writer_gid: u32,
}
impl PreparationTarget {
    fn validate(&self) -> Result<()> {
        if [
            &self.volume_id,
            &self.namespace_uid,
            &self.pvc_uid,
            &self.pv_uid,
            &self.filesystem_uuid,
        ]
        .into_iter()
        .any(|s| ComputerId::new(s).is_err())
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PreparationLease {
    org: String,
    request_id: String,
    owner: WorkerId,
    epoch: i64,
    until_ms: i64,
    request: PrepareRequest,
    target: PreparationTarget,
    mode: ClaimMode,
}
impl PreparationLease {
    pub fn request(&self) -> &PrepareRequest {
        &self.request
    }
    pub fn target(&self) -> &PreparationTarget {
        &self.target
    }
    pub fn mode(&self) -> ClaimMode {
        self.mode
    }
    pub fn epoch(&self) -> i64 {
        self.epoch
    }
    pub fn expires_at_ms(&self) -> i64 {
        self.until_ms
    }
}

#[derive(Debug)]
pub enum PreparationClaim {
    Busy,
    Prepared(Prepared),
    Claimed(Box<PreparationLease>),
}

/// Single-use in-process dispatch admission, never accepted from HTTP.
#[derive(Debug)]
pub struct PreparationPermit {
    lease: PreparationLease,
}
impl PreparationPermit {
    pub fn lease(&self) -> &PreparationLease {
        &self.lease
    }
}

async fn lease_row(
    tx: &mut Transaction<'_, Postgres>,
    lease: &PreparationLease,
) -> Result<sqlx::postgres::PgRow> {
    let row =
        sqlx::query("SELECT * FROM candidate_preparations WHERE organization=$1 AND request_id=$2")
            .bind(&lease.org)
            .bind(&lease.request_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::StaleReconcileLease)?;
    if row.try_get::<i64, _>("lease_epoch")? != lease.epoch
        || row.try_get::<Option<String>, _>("lease_owner")?.as_deref() != Some(lease.owner.as_str())
        || row.try_get::<Option<i64>, _>("lease_until_ms")? != Some(lease.until_ms)
        || transactions::now(tx).await?
            >= row
                .try_get::<Option<i64>, _>("lease_until_ms")?
                .unwrap_or(0)
        || row
            .try_get::<Option<serde_json::Value>, _>("receipt")?
            .is_some()
    {
        return Err(Error::StaleReconcileLease);
    }
    Ok(row)
}

async fn volume_evidence(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    target: &PreparationTarget,
    snapshot: &graph::Snapshot,
) -> Result<()> {
    let reference = &snapshot
        .resource(DefinitionKind::Volume, &target.volume_id)?
        .reference;
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM reconcile_intents i JOIN reconciliation_results r USING (organization,step_id) JOIN reconciliation_objects pvc ON pvc.organization=i.organization AND pvc.step_id=i.step_id AND pvc.role='pvc' JOIN reconciliation_objects pv ON pv.organization=i.organization AND pv.step_id=i.step_id AND pv.role='pv' WHERE i.organization=$1 AND i.resource_id=$2 AND i.revision=$3 AND i.state='Succeeded' AND r.receipt->>'backend'='kubernetes_juicefs' AND r.receipt->>'spec_digest'=$4 AND r.receipt->>'object_uid'=$5 AND r.receipt->>'evidence_id'=$6 AND pvc.binding->>'backend'='kubernetes_juicefs' AND pv.binding->>'backend'='kubernetes_juicefs' AND pvc.binding->>'uid'=$5 AND pv.binding->>'uid'=$6 AND pvc.binding->>'scope_uid'=$7 AND pv.binding->>'scope_uid'=$7)")
        .bind(org).bind(&target.volume_id).bind(reference.revision).bind(&reference.digest).bind(&target.pvc_uid).bind(&target.pv_uid).bind(&target.namespace_uid).fetch_one(&mut **tx).await?;
    if !valid {
        return Err(Error::ReferenceUnavailable);
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| Error::InvalidStoredData)
}

impl Store {
    /// Claim one specific admitted request on an operator-bound local Volume.
    /// No global polling scheduler or caller-supplied input manifest is implied.
    pub async fn claim_candidate_preparation(
        &self,
        org: &OrganizationId,
        request_id: &str,
        owner: &WorkerId,
        target: &PreparationTarget,
    ) -> Result<PreparationClaim> {
        target.validate()?;
        ComputerId::new(request_id).map_err(|_| Error::InvalidRuntimeRequest)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let snapshot = graph::authorize_bound(&mut tx, org.as_str(), request_id).await?;
        volume_evidence(&mut tx, org.as_str(), target, &snapshot).await?;
        let row = sqlx::query("SELECT r.*,v.manifest,v.digest AS manifest_digest FROM runtime_start_requests r JOIN runtime_start_inputs i USING (organization,request_id) JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision WHERE r.organization=$1 AND r.request_id=$2 AND r.workspace_id=i.workspace_id")
            .bind(org.as_str()).bind(request_id).fetch_optional(&mut *tx).await?.ok_or(Error::WorkspaceInputUnavailable)?;
        if row.try_get::<String, _>("volume_id")? != target.volume_id {
            return Err(Error::ReferenceUnavailable);
        }
        let manifest: Manifest = decode(row.try_get("manifest")?)?;
        let request = PrepareRequest {
            organization: org.as_str().into(),
            volume_uid: target.pvc_uid.clone(),
            workspace: row.try_get("workspace_id")?,
            candidate: row.try_get("candidate_id")?,
            computer: row.try_get("computer_id")?,
            generation: row
                .try_get::<i64, _>("generation")?
                .try_into()
                .map_err(|_| Error::InvalidStoredData)?,
            quota_bytes: row
                .try_get::<i64, _>("storage_bytes")?
                .try_into()
                .map_err(|_| Error::InvalidStoredData)?,
            manifest_digest: row.try_get("manifest_digest")?,
            manifest,
        };
        let hash = request
            .binding_digest(&target.volume_path, target.writer_uid, target.writer_gid)
            .map_err(|_| Error::InvalidRuntimeRequest)?;
        let previous = sqlx::query(
            "SELECT * FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(org.as_str())
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?;
        let now = transactions::now(&mut tx).await?;
        let mut epoch = 1;
        let mut mode = ClaimMode::Execute;
        if let Some(previous) = previous {
            if decode::<PreparationTarget>(previous.try_get("binding")?)? != *target
                || decode::<PrepareRequest>(previous.try_get("request")?)? != request
                || previous.try_get::<String, _>("request_digest")? != hash
            {
                return Err(Error::IdempotencyConflict);
            }
            if let Some(receipt) = previous.try_get::<Option<serde_json::Value>, _>("receipt")? {
                let receipt = decode(receipt)?;
                graph::authorize_bound(&mut tx, org.as_str(), request_id).await?;
                tx.commit().await?;
                return Ok(PreparationClaim::Prepared(receipt));
            }
            if previous
                .try_get::<Option<i64>, _>("lease_until_ms")?
                .is_some_and(|until| until > now)
            {
                tx.commit().await?;
                return Ok(PreparationClaim::Busy);
            }
            epoch = previous
                .try_get::<i64, _>("lease_epoch")?
                .checked_add(1)
                .ok_or(Error::CounterExhausted)?;
            if previous.try_get::<bool, _>("dispatch_started")? {
                mode = ClaimMode::Observe;
            }
        }
        if mode == ClaimMode::Execute
            && (row.try_get::<String, _>("state")? != "Queued"
                || now >= row.try_get::<i64, _>("queue_deadline_at_ms")?)
        {
            return Err(Error::RuntimeConflict);
        }
        let until = now.checked_add(180_000).ok_or(Error::CounterExhausted)?;
        let event = transactions::emit(
            &mut tx,
            org.as_str(),
            seq,
            "candidate.preparation_claimed",
            serde_json::json!({"request_id":request_id,"epoch":epoch,"mode":mode}),
        )
        .await?;
        sqlx::query("INSERT INTO candidate_preparations (organization,request_id,binding,request,request_digest,lease_epoch,lease_owner,lease_until_ms,event_sequence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (organization,request_id) DO UPDATE SET lease_epoch=$6,lease_owner=$7,lease_until_ms=$8,event_sequence=$9,reason=NULL")
            .bind(org.as_str()).bind(request_id).bind(serde_json::to_value(target).map_err(|_|Error::InvalidStoredData)?)
            .bind(serde_json::to_value(&request).map_err(|_|Error::InvalidStoredData)?).bind(hash).bind(epoch).bind(owner.as_str()).bind(until).bind(event).execute(&mut *tx).await?;
        graph::authorize_bound(&mut tx, org.as_str(), request_id).await?;
        if transactions::now(&mut tx).await? >= until {
            return Err(Error::StaleReconcileLease);
        }
        tx.commit().await?;
        Ok(PreparationClaim::Claimed(Box::new(PreparationLease {
            org: org.as_str().into(),
            request_id: request_id.into(),
            owner: owner.clone(),
            epoch,
            until_ms: until,
            request,
            target: target.clone(),
            mode,
        })))
    }

    pub async fn begin_candidate_preparation(
        &self,
        lease: &PreparationLease,
    ) -> Result<PreparationPermit> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &lease.org).await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        let row = lease_row(&mut tx, lease).await?;
        if lease.mode != ClaimMode::Execute || row.try_get::<bool, _>("dispatch_started")? {
            return Err(Error::DispatchAlreadyStarted);
        }
        let changed = sqlx::query("UPDATE runtime_start_requests SET state='Preparing' WHERE organization=$1 AND request_id=$2 AND state='Queued' AND queue_deadline_at_ms>floor(extract(epoch from clock_timestamp())*1000)::bigint")
            .bind(&lease.org).bind(&lease.request_id).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(Error::RuntimeConflict);
        }
        sqlx::query("UPDATE runtime_controls SET revision=revision+1 WHERE organization=$1 AND active_request=$2 AND revision<9223372036854775807")
            .bind(&lease.org).bind(&lease.request_id).execute(&mut *tx).await?.rows_affected().eq(&1).then_some(()).ok_or(Error::CounterExhausted)?;
        let event = transactions::emit(
            &mut tx,
            &lease.org,
            seq,
            "candidate.preparation_dispatched",
            serde_json::json!({"request_id":lease.request_id,"epoch":lease.epoch}),
        )
        .await?;
        sqlx::query("UPDATE candidate_preparations SET dispatch_started=TRUE,event_sequence=$3 WHERE organization=$1 AND request_id=$2")
            .bind(&lease.org).bind(&lease.request_id).bind(event).execute(&mut *tx).await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        lease_row(&mut tx, lease).await?;
        tx.commit().await?;
        Ok(PreparationPermit {
            lease: lease.clone(),
        })
    }

    /// Receipt recording is atomic and idempotent. The trusted storage adapter
    /// must first observe the actual inode, durable receipt and directory quota.
    pub async fn finish_candidate_preparation(
        &self,
        lease: &PreparationLease,
        receipt: &Prepared,
    ) -> Result<()> {
        let expected = lease
            .request
            .binding_digest(
                &lease.target.volume_path,
                lease.target.writer_uid,
                lease.target.writer_gid,
            )
            .map_err(|_| Error::InvalidStoredData)?;
        if receipt.version != 1
            || receipt.request_digest != expected
            || receipt.filesystem_uuid != lease.target.filesystem_uuid
            || receipt.volume_uid != lease.target.pvc_uid
            || receipt.path_ref != lease.request.path_ref()
            || receipt.manifest_digest != lease.request.manifest_digest
            || receipt.quota_bytes != lease.request.quota_bytes
            || receipt.data_inode == 0
        {
            return Err(Error::InvalidReconcileResult);
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &lease.org).await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        let previous: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(&lease.org)
        .bind(&lease.request_id)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(previous) = previous {
            if decode::<Prepared>(previous)? != *receipt {
                return Err(Error::IdempotencyConflict);
            }
            graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
            tx.commit().await?;
            return Ok(());
        }
        let row = lease_row(&mut tx, lease).await?;
        if !row.try_get::<bool, _>("dispatch_started")? {
            return Err(Error::InvalidReconcileResult);
        }
        let changed = sqlx::query("UPDATE runtime_start_requests SET state='Prepared' WHERE organization=$1 AND request_id=$2 AND state='Preparing'")
            .bind(&lease.org).bind(&lease.request_id).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(Error::RuntimeConflict);
        }
        let changed = sqlx::query("UPDATE runtime_controls SET revision=revision+1 WHERE organization=$1 AND active_request=$2 AND revision<9223372036854775807")
            .bind(&lease.org).bind(&lease.request_id).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(Error::CounterExhausted);
        }
        let event = transactions::emit(&mut tx, &lease.org, seq, "candidate.prepared", serde_json::json!({"request_id":lease.request_id,"generation":lease.request.generation,"ready":false})).await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        lease_row(&mut tx, lease).await?;
        sqlx::query("UPDATE candidate_preparations SET receipt=$3,lease_owner=NULL,lease_until_ms=NULL,reason=NULL,event_sequence=$4 WHERE organization=$1 AND request_id=$2")
            .bind(&lease.org).bind(&lease.request_id).bind(serde_json::to_value(receipt).map_err(|_|Error::InvalidStoredData)?).bind(event).execute(&mut *tx).await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        if transactions::now(&mut tx).await? >= lease.until_ms {
            return Err(Error::StaleReconcileLease);
        }
        tx.commit().await?;
        Ok(())
    }

    /// Release coordination after uncertain storage work. All resource reservations
    /// remain held and later claims can only observe the original publication.
    pub async fn defer_candidate_preparation(&self, lease: &PreparationLease) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &lease.org).await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        let row = lease_row(&mut tx, lease).await?;
        if !row.try_get::<bool, _>("dispatch_started")? {
            return Err(Error::InvalidReconcileResult);
        }
        let event = transactions::emit(
            &mut tx,
            &lease.org,
            seq,
            "candidate.preparation_unknown",
            serde_json::json!({"request_id":lease.request_id,"epoch":lease.epoch}),
        )
        .await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        lease_row(&mut tx, lease).await?;
        sqlx::query("UPDATE candidate_preparations SET lease_owner=NULL,lease_until_ms=NULL,reason='storage_unknown',event_sequence=$3 WHERE organization=$1 AND request_id=$2")
            .bind(&lease.org).bind(&lease.request_id).bind(event).execute(&mut *tx).await?;
        graph::authorize_bound(&mut tx, &lease.org, &lease.request_id).await?;
        if transactions::now(&mut tx).await? >= lease.until_ms {
            return Err(Error::StaleReconcileLease);
        }
        tx.commit().await?;
        Ok(())
    }
}
