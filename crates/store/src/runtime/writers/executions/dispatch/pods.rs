//! Durable identity for one trusted adapter's creation attempt; never a scheduler.
mod validation;
use super::*;
use serde_json::{Value, json};

/// Recovery data, including the exact proposed manifest. Reading or cloning it
/// cannot issue another creation permit. Only trusted store clients may read it:
/// it contains command bytes and internal storage paths.
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionPodPlan {
    pub organization: String,
    pub execution_id: String,
    pub namespace_uid: String,
    pub namespace: String,
    pub pod_name: String,
    pub plan_digest: String,
    pub manifest: Value,
    pub pod_uid: Option<String>,
}

/// Issued only after the immutable plan and its outbox event commit. A trusted
/// adapter may attempt one POST, checking this original deadline immediately
/// beforehand. Lost responses require observation, never another POST.
#[derive(Debug)]
pub struct ExecutionPodAttempt {
    plan: ExecutionPodPlan,
    deadline: Instant,
}
impl ExecutionPodAttempt {
    pub fn plan(&self) -> &ExecutionPodPlan {
        &self.plan
    }
    pub fn remaining_budget_ms(&self) -> Result<u32> {
        let remaining = self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        if remaining == 0 {
            return Err(Error::WriterLeaseInactive);
        }
        u32::try_from(remaining).map_err(|_| Error::InvalidStoredData)
    }
}

fn hash(
    dispatch: &ExecutionDispatchIntent,
    namespace_uid: &str,
    manifest: &Value,
) -> Result<String> {
    digest(
        "agent-computer/execution-pod-v1",
        &(
            &dispatch.organization,
            &dispatch.execution.execution_id,
            &dispatch.intent_digest,
            namespace_uid,
            manifest,
        ),
    )
}

async fn receipt(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
) -> Result<Option<ExecutionPodPlan>> {
    let Some(row) = sqlx::query("SELECT p.*,o.pod_uid FROM execution_pod_plans p LEFT JOIN execution_pod_observations o USING(organization,execution_id) WHERE p.organization=$1 AND p.execution_id=$2")
        .bind(&dispatch.organization).bind(&dispatch.execution.execution_id).fetch_optional(&mut **tx).await? else { return Ok(None) };
    let manifest: Value = row.try_get("manifest")?;
    let namespace_uid: String = row.try_get("namespace_uid")?;
    let plan_digest: String = row.try_get("plan_digest")?;
    let (namespace, pod_name) = validation::identity(dispatch, &namespace_uid, &manifest)
        .map_err(|_| Error::InvalidStoredData)?;
    if hash(dispatch, &namespace_uid, &manifest)? != plan_digest
        || namespace != row.try_get::<String, _>("namespace")?
        || pod_name != row.try_get::<String, _>("pod_name")?
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(Some(ExecutionPodPlan {
        organization: dispatch.organization.clone(),
        execution_id: dispatch.execution.execution_id.clone(),
        namespace_uid,
        namespace,
        pod_name,
        plan_digest,
        manifest,
        pod_uid: row.try_get("pod_uid")?,
    }))
}

/// A registered plan must have its original UID recorded before a grant is
/// issued. Historical/component dispatches without a plan retain migration 14's
/// trusted API; this is not a public runtime authorization path.
pub(super) async fn require_observed(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    uid: &str,
) -> Result<()> {
    let invalid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_pod_plans p LEFT JOIN execution_pod_observations o USING(organization,execution_id) WHERE p.organization=$1 AND p.execution_id=$2 AND o.pod_uid IS DISTINCT FROM $3)")
        .bind(org).bind(id).bind(uid).fetch_one(&mut **tx).await?;
    if invalid {
        return Err(Error::RuntimeConflict);
    }
    Ok(())
}

impl Store {
    /// Journal an already compiled Candidate startup Pod before any Kubernetes
    /// mutation. This validates execution identity, not Kubernetes admission or
    /// the live filesystem: the trusted adapter must independently verify both.
    /// Identical retries also fail after commit, including a lost commit response.
    pub async fn register_candidate_execution_pod(
        &self,
        attempt: &ExecutionDispatchAttempt,
        namespace_uid: &str,
        manifest: &Value,
    ) -> Result<ExecutionPodAttempt> {
        let original = attempt.intent();
        let (namespace, pod_name) = validation::identity(original, namespace_uid, manifest)?;
        let org = original.organization.as_str();
        let id = original.execution.execution_id.as_str();
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let record = row(&mut tx, org, id).await?;
        let current = intent(&mut tx, org, &record).await?;
        if current.intent_digest != original.intent_digest {
            return Err(Error::RuntimeConflict);
        }
        if receipt(&mut tx, &current).await?.is_some()
            || startup::receipt(&mut tx, org, id).await?.is_some()
        {
            return Err(Error::DispatchAlreadyStarted);
        }
        if current.execution.state != ExecutionState::Dispatching {
            return Err(Error::WriterLeaseInactive);
        }
        match bound_authority(&mut tx, org, &record).await {
            Ok(()) => {}
            Err(Error::Unauthenticated | Error::Forbidden) => {
                uncertain(&mut tx, org, &record, "writer_unavailable", seq).await?;
                tx.commit().await?;
                return Err(Error::WriterLeaseInactive);
            }
            Err(error) => return Err(error),
        }
        reconcile(&mut tx, org, &record, seq).await?;
        if row(&mut tx, org, id).await?.try_get::<String, _>("state")? != "Dispatching" {
            tx.commit().await?;
            return Err(Error::WriterLeaseInactive);
        }
        attempt.remaining_budget_ms()?;
        let plan_digest = hash(&current, namespace_uid, manifest)?;
        sqlx::query("INSERT INTO execution_pod_plans (organization,execution_id,namespace_uid,namespace,pod_name,plan_digest,manifest) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(org).bind(id).bind(namespace_uid).bind(&namespace).bind(&pod_name).bind(&plan_digest).bind(manifest).execute(&mut *tx).await?;
        transactions::emit(&mut tx, org, seq, "execution.pod_planned", json!({"execution_id":id,"namespace_uid":namespace_uid,"pod_name":pod_name,"plan_digest":plan_digest,"pod_creation_confirmed":false})).await?;
        // Slow journal/outbox writes cannot extend the original attempt or keep
        // an expired/revoked writer authorized.
        bound_authority(&mut tx, org, &record).await?;
        let owner = authority::row(&mut tx, org, &current.execution.lease_id).await?;
        if transactions::now(&mut tx).await? >= current.deadline_at_ms
            || !authority::active(&mut tx, org, &owner).await?
        {
            return Err(Error::WriterLeaseInactive);
        }
        attempt.remaining_budget_ms()?;
        let result = ExecutionPodAttempt {
            plan: receipt(&mut tx, &current)
                .await?
                .ok_or(Error::InvalidStoredData)?,
            deadline: attempt.deadline,
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Record a UID after the trusted adapter strictly verifies the actual Pod
    /// against the persisted plan. Allowed after authority expires for recovery;
    /// it cannot authorize a process, establish completion or release a writer.
    pub async fn record_candidate_execution_pod(
        &self,
        org: &OrganizationId,
        id: &str,
        plan_digest: &str,
        pod_uid: &str,
    ) -> Result<ExecutionPodPlan> {
        valid_id(pod_uid)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        let dispatch = intent(&mut tx, org.as_str(), &record).await?;
        let plan = receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::RuntimeAccessUnavailable)?;
        if plan.plan_digest != plan_digest
            || plan.pod_uid.as_deref().is_some_and(|uid| uid != pod_uid)
        {
            return Err(Error::RuntimeConflict);
        }
        if plan.pod_uid.is_none() {
            sqlx::query("INSERT INTO execution_pod_observations (organization,execution_id,pod_uid) VALUES ($1,$2,$3)")
                .bind(org.as_str()).bind(id).bind(pod_uid).execute(&mut *tx).await?;
            transactions::emit(&mut tx, org.as_str(), seq, "execution.pod_observed", json!({"execution_id":id,"plan_digest":plan_digest,"pod_uid":pod_uid,"process_started_confirmed":false})).await?;
        }
        let result = receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        tx.commit().await?;
        Ok(result)
    }

    /// Recovery only. This never reissues a creation permit or a startup grant.
    pub async fn candidate_execution_pod(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<Option<ExecutionPodPlan>> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        let dispatch = intent(&mut tx, org.as_str(), &record).await?;
        let result = receipt(&mut tx, &dispatch).await?;
        tx.commit().await?;
        Ok(result)
    }
}
