//! Trusted live guard registration. Reading persisted evidence cannot recreate a guard.
use super::*;
use agent_computer_node::ArmedGuard;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
pub struct ExecutionWatchdogArm {
    pub evidence: Value,
    pub evidence_digest: String,
    pub registered_at_ms: i64,
    pub expires_at_ms: i64,
}

fn hash(
    dispatch: &ExecutionDispatchIntent,
    plan: &ExecutionPodPlan,
    evidence: &Value,
    registered: i64,
    expires: i64,
) -> Result<String> {
    digest(
        "agent-computer/execution-watchdog-v1",
        &(
            &dispatch.organization,
            &dispatch.execution.execution_id,
            &dispatch.intent_digest,
            &plan.plan_digest,
            evidence,
            registered,
            expires,
        ),
    )
}

async fn receipt(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
) -> Result<Option<ExecutionWatchdogArm>> {
    let Some(row) = sqlx::query(
        "SELECT * FROM execution_watchdog_arms WHERE organization=$1 AND execution_id=$2",
    )
    .bind(&dispatch.organization)
    .bind(&dispatch.execution.execution_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    let plan = pods::receipt(tx, dispatch)
        .await?
        .ok_or(Error::InvalidStoredData)?;
    let result = ExecutionWatchdogArm {
        evidence: row.try_get("evidence")?,
        evidence_digest: row.try_get("evidence_digest")?,
        registered_at_ms: row.try_get("registered_at_ms")?,
        expires_at_ms: row.try_get("expires_at_ms")?,
    };
    if hash(
        dispatch,
        &plan,
        &result.evidence,
        result.registered_at_ms,
        result.expires_at_ms,
    )? != result.evidence_digest
        || result.evidence["runtime"]["identity"]["pod_uid"]
            != row.try_get::<String, _>("pod_uid")?
        || plan.pod_uid.as_deref() != result.evidence["runtime"]["identity"]["pod_uid"].as_str()
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(Some(result))
}

pub(super) async fn require_live(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
    pod_uid: &str,
    guard: Option<&mut ArmedGuard>,
) -> Result<Option<u32>> {
    let plan = pods::receipt(tx, dispatch).await?;
    if plan.is_none() {
        return if guard.is_none() {
            Ok(None)
        } else {
            Err(Error::RuntimeConflict)
        };
    }
    let guard = guard.ok_or(Error::RuntimeAccessUnavailable)?;
    let arm = receipt(tx, dispatch)
        .await?
        .ok_or(Error::RuntimeAccessUnavailable)?;
    let now = transactions::now(tx).await?;
    let node_remaining = guard
        .remaining_budget_ms()
        .map_err(|_| Error::WriterLeaseInactive)?;
    if guard.evidence().runtime.identity.pod_uid() != pod_uid
        || guard.evidence().armed.request.execution_id != dispatch.execution.execution_id
        || serde_json::to_value(guard.evidence()).map_err(|_| Error::InvalidRuntimeRequest)?
            != arm.evidence
        || now >= arm.expires_at_ms
    {
        return Err(Error::WriterLeaseInactive);
    }
    Ok(Some(node_remaining.min(
        u32::try_from(arm.expires_at_ms - now).map_err(|_| Error::InvalidStoredData)?,
    )))
}

impl Store {
    /// Only a live node adapter handle can register. One insert; a lost response
    /// is recovery data, never permission to arm another process or regrant.
    pub async fn register_candidate_execution_watchdog(
        &self,
        attempt: &ExecutionDispatchAttempt,
        guard: &mut ArmedGuard,
    ) -> Result<ExecutionWatchdogArm> {
        let original = attempt.intent();
        let org = &original.organization;
        let id = &original.execution.execution_id;
        let evidence =
            serde_json::to_value(guard.evidence()).map_err(|_| Error::InvalidRuntimeRequest)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let record = row(&mut tx, org, id).await?;
        let dispatch = intent(&mut tx, org, &record).await?;
        let plan = pods::receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::RuntimeAccessUnavailable)?;
        let runtime = &guard.evidence().runtime;
        if dispatch.intent_digest != original.intent_digest
            || runtime.identity.namespace_uid() != plan.namespace_uid
            || runtime.identity.namespace() != plan.namespace
            || runtime.identity.pod_name() != plan.pod_name
            || plan.pod_uid.as_deref() != Some(runtime.identity.pod_uid())
            || guard.evidence().armed.request.execution_id != *id
            || dispatch.binding["prepared"]["data_inode"].as_u64() != Some(runtime.workspace_inode)
            || dispatch.binding["storage_target"]["volume_path"].as_str()
                != Some(&runtime.volume_path)
        {
            return Err(Error::RuntimeConflict);
        }
        if receipt(&mut tx, &dispatch).await?.is_some()
            || startup::receipt(&mut tx, org, id).await?.is_some()
        {
            return Err(Error::DispatchAlreadyStarted);
        }
        if dispatch.execution.state != ExecutionState::Dispatching {
            return Err(Error::WriterLeaseInactive);
        }
        bound_authority(&mut tx, org, &record).await?;
        reconcile(&mut tx, org, &record, seq).await?;
        if row(&mut tx, org, id).await?.try_get::<String, _>("state")? != "Dispatching" {
            tx.commit().await?;
            return Err(Error::WriterLeaseInactive);
        }
        let now = transactions::now(&mut tx).await?;
        let remaining = guard
            .remaining_budget_ms()
            .map_err(|_| Error::WriterLeaseInactive)?
            .min(attempt.remaining_budget_ms()?);
        let expires = dispatch.deadline_at_ms.min(now + remaining as i64);
        let hash = hash(&dispatch, &plan, &evidence, now, expires)?;
        let runtime = &guard.evidence().runtime;
        sqlx::query("INSERT INTO execution_watchdog_arms (organization,execution_id,pod_uid,node_uid,boot_id,container_id,cgroup_inode,evidence,evidence_digest,registered_at_ms,expires_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(org).bind(id).bind(runtime.identity.pod_uid()).bind(&runtime.identity.node().uid).bind(&runtime.identity.node().boot_id).bind(runtime.identity.container_id())
            .bind(i64::try_from(runtime.cgroup_inode).map_err(|_|Error::InvalidRuntimeRequest)?).bind(&evidence).bind(&hash).bind(now).bind(expires).execute(&mut *tx).await?;
        transactions::emit(&mut tx,org,seq,"execution.watchdog_armed",json!({"execution_id":id,"pod_uid":runtime.identity.pod_uid(),"node_uid":runtime.identity.node().uid,"evidence_digest":hash,"expires_at_ms":expires,"process_termination_confirmed":false})).await?;
        bound_authority(&mut tx, org, &record).await?;
        let owner = authority::row(&mut tx, org, &dispatch.execution.lease_id).await?;
        if transactions::now(&mut tx).await? >= expires
            || !authority::active(&mut tx, org, &owner).await?
        {
            return Err(Error::WriterLeaseInactive);
        }
        guard
            .remaining_budget_ms()
            .map_err(|_| Error::WriterLeaseInactive)?;
        attempt.remaining_budget_ms()?;
        let result = receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn candidate_execution_watchdog(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<Option<ExecutionWatchdogArm>> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        let dispatch = intent(&mut tx, org.as_str(), &record).await?;
        let result = receipt(&mut tx, &dispatch).await?;
        tx.commit().await?;
        Ok(result)
    }
}
