//! Trusted worker journal API. No Kubernetes mutation or process launch here.
mod inputs;
mod outputs;
pub use outputs::{ExecutionOutput, OutputState};
mod pods;
mod startup;
mod watchdogs;
use super::*;
pub use inputs::ExecutionRuntimeInputs;
pub use pods::{ExecutionPodAttempt, ExecutionPodPlan};
pub use startup::{ExecutionStartupAttempt, ExecutionStartupGrant};
use std::time::{Duration, Instant};
pub use watchdogs::ExecutionWatchdogArm;

/// Fixed dispatch inputs and current metadata for a trusted store client. Reading
/// this record does not grant permission to repeat the external mutation.
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionDispatchIntent {
    pub organization: String,
    pub execution: ExecutionRequest,
    pub input: SubmitExecution,
    pub binding: serde_json::Value,
    pub intent_digest: String,
    pub started_at_ms: i64,
    pub deadline_at_ms: i64,
}

/// Returned once, after the intent commits. Not cloneable or deserializable.
/// A future runtime adapter must recheck authority at actual process start and
/// enforce the absolute deadline externally; a delayed Pod cannot restart this
/// budget. Dropping this value proves neither non-dispatch nor writer drainage.
#[derive(Debug)]
pub struct ExecutionDispatchAttempt {
    intent: ExecutionDispatchIntent,
    deadline: Instant,
}
impl ExecutionDispatchAttempt {
    pub fn intent(&self) -> &ExecutionDispatchIntent {
        &self.intent
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

async fn bound_authority(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    record: &PgRow,
) -> Result<()> {
    let session = sqlx::query("SELECT principal,credential_id FROM connection_sessions WHERE organization=$1 AND session_id=$2")
        .bind(org).bind(record.try_get::<String,_>("session_id")?).fetch_one(&mut **tx).await?;
    for scope in [
        ServiceScope::RuntimeConnect,
        ServiceScope::RuntimeRead,
        ServiceScope::RuntimeModify,
    ] {
        Store::authorize_bound_runtime_in(
            tx,
            org,
            &session.try_get::<String, _>("principal")?,
            &session.try_get::<String, _>("credential_id")?,
            scope,
        )
        .await?;
    }
    Ok(())
}
fn hash(org: &str, record: &PgRow, started: i64, deadline: i64) -> Result<String> {
    digest(
        "agent-computer/execution-dispatch-v1",
        &(
            org,
            record.try_get::<String, _>("execution_id")?,
            record.try_get::<String, _>("lease_id")?,
            record.try_get::<i64, _>("epoch")?,
            record.try_get::<String, _>("input_digest")?,
            record.try_get::<String, _>("binding_digest")?,
            started,
            deadline,
        ),
    )
}
async fn intent(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    record: &PgRow,
) -> Result<ExecutionDispatchIntent> {
    let execution = view(record)?;
    let input: SubmitExecution =
        serde_json::from_value(record.try_get("input")?).map_err(|_| Error::InvalidStoredData)?;
    input
        .command
        .validate()
        .map_err(|_| Error::InvalidStoredData)?;
    let credential: String = sqlx::query_scalar(
        "SELECT credential_id FROM connection_sessions WHERE organization=$1 AND session_id=$2",
    )
    .bind(org)
    .bind(record.try_get::<String, _>("session_id")?)
    .fetch_one(&mut **tx)
    .await?;
    if digest(
        "agent-computer/execution-submit-v1",
        &(&execution.computer_id, &input, credential),
    )? != execution.input_digest
    {
        return Err(Error::InvalidStoredData);
    }
    let row = sqlx::query(
        "SELECT * FROM execution_dispatch_intents WHERE organization=$1 AND execution_id=$2",
    )
    .bind(org)
    .bind(&execution.execution_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::RuntimeAccessUnavailable)?;
    let started = row.try_get("started_at_ms")?;
    let deadline = row.try_get("deadline_at_ms")?;
    let intent_digest: String = row.try_get("intent_digest")?;
    if hash(org, record, started, deadline)? != intent_digest {
        return Err(Error::InvalidStoredData);
    }
    Ok(ExecutionDispatchIntent {
        organization: org.into(),
        execution,
        input,
        binding: record.try_get("binding")?,
        intent_digest,
        started_at_ms: started,
        deadline_at_ms: deadline,
    })
}

/// Lowering authority does not establish that the external writer has stopped.
pub(super) async fn uncertain(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    record: &PgRow,
    reason: &str,
    seq: i64,
) -> Result<()> {
    let state: String = record.try_get("state")?;
    if !matches!(state.as_str(), "Dispatching" | "CancelRequested") {
        return Ok(());
    }
    let id: String = record.try_get("execution_id")?;
    sqlx::query("UPDATE execution_requests SET state='Unknown',reason=$3,revision=revision+1 WHERE organization=$1 AND execution_id=$2")
        .bind(org).bind(&id).bind(reason).execute(&mut **tx).await?;
    let seq=transactions::emit(tx,org,seq,"execution.status_changed",serde_json::json!({"execution_id":id,"state":"Unknown","reason":reason,"dispatch_started":true})).await?;
    stop_writer(tx, org, record, seq).await
}
pub(super) async fn stop_writer(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    record: &PgRow,
    seq: i64,
) -> Result<()> {
    let lease: String = record.try_get("lease_id")?;
    let epoch: i64 = record.try_get("epoch")?;
    if sqlx::query("UPDATE candidate_writer_leases SET state='Draining',revision=revision+1 WHERE organization=$1 AND lease_id=$2 AND epoch=$3 AND state='Held'")
        .bind(org).bind(&lease).bind(epoch).execute(&mut **tx).await?.rows_affected()>0 {
        transactions::emit(tx,org,seq,"writer.draining",serde_json::json!({"lease_id":lease,"epoch":epoch,"process_termination_confirmed":false})).await?;
    }
    Ok(())
}

impl Store {
    /// Trusted worker only, never credential-ID authentication. The original
    /// credential and all current grants are checked while the journal commits.
    /// Any retry after commitment returns DispatchAlreadyStarted, including a
    /// lost response or a dropped attempt. Recovery must observe the fixed ID.
    pub async fn begin_candidate_execution_dispatch(
        &self,
        org: &OrganizationId,
        id: &str,
        expected_revision: i64,
    ) -> Result<ExecutionDispatchAttempt> {
        if expected_revision < 1 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let local_start = Instant::now();
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        if record.try_get::<String, _>("state")? != "Queued" {
            return Err(if view(&record)?.dispatch_started {
                Error::DispatchAlreadyStarted
            } else {
                Error::WriterLeaseInactive
            });
        }
        if record.try_get::<i64, _>("revision")? != expected_revision {
            return Err(Error::RuntimeConflict);
        }
        match bound_authority(&mut tx, org.as_str(), &record).await {
            Ok(()) => {}
            Err(Error::Unauthenticated | Error::Forbidden) => {
                cancel_reserved(
                    &mut tx,
                    org.as_str(),
                    &record.try_get::<String, _>("lease_id")?,
                    record.try_get("epoch")?,
                    "writer_unavailable",
                    seq,
                )
                .await?;
                tx.commit().await?;
                return Err(Error::WriterLeaseInactive);
            }
            Err(e) => return Err(e),
        }
        reconcile(&mut tx, org.as_str(), &record, seq).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        if record.try_get::<String, _>("state")? != "Queued" {
            tx.commit().await?;
            return Err(Error::WriterLeaseInactive);
        }
        let started = transactions::now(&mut tx).await?;
        let deadline: i64 = record.try_get("queue_deadline_at_ms")?;
        let lease: String = record.try_get("lease_id")?;
        let epoch: i64 = record.try_get("epoch")?;
        let hash = hash(org.as_str(), &record, started, deadline)?;
        sqlx::query("INSERT INTO execution_dispatch_intents (organization,execution_id,lease_id,epoch,intent_digest,started_at_ms,deadline_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(org.as_str()).bind(id).bind(&lease).bind(epoch).bind(&hash).bind(started).bind(deadline).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO candidate_writer_dispatches (organization,dispatch_id,lease_id,epoch,input_digest) VALUES ($1,$2,$3,$4,$5)")
            .bind(org.as_str()).bind(id).bind(&lease).bind(epoch).bind(&hash).execute(&mut *tx).await?;
        sqlx::query("UPDATE execution_requests SET state='Dispatching',reason='dispatch_committed',revision=revision+1 WHERE organization=$1 AND execution_id=$2")
            .bind(org.as_str()).bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE candidate_writer_leases SET revision=revision+1 WHERE organization=$1 AND lease_id=$2")
            .bind(org.as_str()).bind(&lease).execute(&mut *tx).await?;
        transactions::emit(&mut tx,org.as_str(),seq,"execution.status_changed",serde_json::json!({"execution_id":id,"state":"Dispatching","reason":"dispatch_committed","dispatch_started":true,"intent_digest":hash,"deadline_at_ms":deadline})).await?;
        // Recheck expiry after potentially slow journal/outbox writes. Credential
        // and principal share locks also serialize concurrent revocation.
        bound_authority(&mut tx, org.as_str(), &record).await?;
        let owner = authority::row(&mut tx, org.as_str(), &lease).await?;
        if transactions::now(&mut tx).await? >= deadline
            || !authority::active(&mut tx, org.as_str(), &owner).await?
        {
            return Err(Error::WriterLeaseInactive);
        }
        let current = row(&mut tx, org.as_str(), id).await?;
        let result = ExecutionDispatchAttempt {
            intent: intent(&mut tx, org.as_str(), &current).await?,
            deadline: local_start + Duration::from_millis((deadline - started) as u64),
        };
        result.remaining_budget_ms()?;
        tx.commit().await?;
        Ok(result)
    }

    /// Read-only recovery snapshot; never creates another dispatch attempt.
    pub async fn candidate_execution_dispatch(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<ExecutionDispatchIntent> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        let result = intent(&mut tx, org.as_str(), &record).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// A failed/uncertain external mutation lowers authority. It never authorizes
    /// retry or accepts caller-supplied success, stopped, or drain evidence.
    pub async fn mark_candidate_execution_unknown(
        &self,
        org: &OrganizationId,
        id: &str,
        expected_revision: i64,
    ) -> Result<ExecutionRequest> {
        if expected_revision < 1 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        if record.try_get::<String, _>("state")? != "Unknown" {
            if record.try_get::<i64, _>("revision")? != expected_revision {
                return Err(Error::RuntimeConflict);
            }
            if !view(&record)?.dispatch_started {
                return Err(Error::RuntimeConflict);
            }
            uncertain(&mut tx, org.as_str(), &record, "dispatch_unconfirmed", seq).await?;
        }
        let result = view(&row(&mut tx, org.as_str(), id).await?)?;
        tx.commit().await?;
        Ok(result)
    }
}
