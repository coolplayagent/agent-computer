//! Fresh authorization for a trusted runtime's one-shot startup challenge.
use super::*;
use agent_computer_sandbox::{
    Bootstrap, Request, STARTUP_PROTOCOL, StartupChallenge, StartupGrant,
};

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionStartupGrant {
    pub pod_uid: String,
    pub challenge: StartupChallenge,
    pub grant: StartupGrant,
    pub grant_digest: String,
    pub granted_at_ms: i64,
}

/// A single authorization result, not a replayable runtime dispatch permit.
/// A lost response is reconciled against its fixed Pod UID and challenge; the
/// controller must never create a replacement Pod or request a fresh grant.
#[derive(Debug)]
pub struct ExecutionStartupAttempt {
    grant: ExecutionStartupGrant,
    deadline: Instant,
}
impl ExecutionStartupAttempt {
    pub fn grant(&self) -> &ExecutionStartupGrant {
        &self.grant
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
impl ExecutionDispatchIntent {
    /// Fixed bytes to deliver read-only with the trusted supervisor. The ceiling
    /// here cannot start a command; startup also requires a fresh challenge grant.
    pub fn bootstrap(&self) -> Result<Bootstrap> {
        let command = &self.input.command;
        let request = Request {
            execution_id: self.execution.execution_id.clone(),
            generation: self.execution.generation as u64,
            argv: command.argv.clone(),
            cwd: command.cwd.clone(),
            timeout_seconds: command.timeout_seconds,
            lease_budget_ms: u32::try_from(self.deadline_at_ms - self.started_at_ms)
                .map_err(|_| Error::InvalidStoredData)?,
            term_grace_ms: command.term_grace_ms,
            output_limit_bytes: command.output_limit_bytes,
        };
        let result = Bootstrap {
            version: STARTUP_PROTOCOL,
            intent_digest: self.intent_digest.clone(),
            request,
        };
        result.validate().map_err(|_| Error::InvalidStoredData)?;
        Ok(result)
    }
}
pub(super) async fn receipt(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
) -> Result<Option<ExecutionStartupGrant>> {
    let Some(row) = sqlx::query(
        "SELECT * FROM execution_startup_grants WHERE organization=$1 AND execution_id=$2",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    let challenge: StartupChallenge =
        serde_json::from_value(row.try_get("challenge")?).map_err(|_| Error::InvalidStoredData)?;
    let grant: StartupGrant =
        serde_json::from_value(row.try_get("grant_body")?).map_err(|_| Error::InvalidStoredData)?;
    let grant_digest: String = row.try_get("grant_digest")?;
    if grant.digest().map_err(|_| Error::InvalidStoredData)? != grant_digest
        || challenge.digest().map_err(|_| Error::InvalidStoredData)? != grant.challenge_digest
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(Some(ExecutionStartupGrant {
        pod_uid: row.try_get("pod_uid")?,
        challenge,
        grant,
        grant_digest,
        granted_at_ms: row.try_get("granted_at_ms")?,
    }))
}
impl Store {
    /// Trusted runtime controller only. Read and authenticate the challenge from
    /// the fixed Pod UID before calling this method; no tenant body can select
    /// a principal, credential, budget, command or alternative execution here.
    pub async fn authorize_candidate_execution_startup(
        &self,
        org: &OrganizationId,
        id: &str,
        expected_revision: i64,
        pod_uid: &str,
        challenge: &StartupChallenge,
    ) -> Result<ExecutionStartupAttempt> {
        valid_id(pod_uid)?;
        challenge
            .validate()
            .map_err(|_| Error::InvalidRuntimeRequest)?;
        if expected_revision < 1 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let local_start = Instant::now();
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        if receipt(&mut tx, org.as_str(), id).await?.is_some() {
            return Err(Error::DispatchAlreadyStarted);
        }
        if record.try_get::<i64, _>("revision")? != expected_revision {
            return Err(Error::RuntimeConflict);
        }
        if record.try_get::<String, _>("state")? != "Dispatching" {
            return Err(Error::WriterLeaseInactive);
        }
        let dispatch = intent(&mut tx, org.as_str(), &record).await?;
        if !challenge
            .matches(&dispatch.bootstrap()?)
            .map_err(|_| Error::InvalidRuntimeRequest)?
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        pods::require_observed(&mut tx, org.as_str(), id, pod_uid).await?;
        match bound_authority(&mut tx, org.as_str(), &record).await {
            Ok(()) => {}
            Err(Error::Unauthenticated | Error::Forbidden) => {
                uncertain(&mut tx, org.as_str(), &record, "writer_unavailable", seq).await?;
                tx.commit().await?;
                return Err(Error::WriterLeaseInactive);
            }
            Err(error) => return Err(error),
        }
        reconcile(&mut tx, org.as_str(), &record, seq).await?;
        let current = row(&mut tx, org.as_str(), id).await?;
        if current.try_get::<String, _>("state")? != "Dispatching" {
            tx.commit().await?;
            return Err(Error::WriterLeaseInactive);
        }
        // This read MUST follow reception of the runtime's challenge. Its budget
        // will be charged from before that challenge was emitted by PID 1.
        let now = transactions::now(&mut tx).await?;
        let budget =
            u32::try_from(dispatch.deadline_at_ms - now).map_err(|_| Error::WriterLeaseInactive)?;
        if budget == 0 {
            return Err(Error::WriterLeaseInactive);
        }
        let grant = StartupGrant {
            version: STARTUP_PROTOCOL,
            challenge_digest: challenge
                .digest()
                .map_err(|_| Error::InvalidRuntimeRequest)?,
            lease_budget_ms: budget,
        };
        let grant_digest = grant.digest().map_err(|_| Error::InvalidStoredData)?;
        sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(org.as_str()).bind(id).bind(pod_uid).bind(serde_json::to_value(challenge).map_err(|_|Error::InvalidRuntimeRequest)?).bind(serde_json::to_value(&grant).map_err(|_|Error::InvalidStoredData)?).bind(&grant_digest).bind(now).execute(&mut *tx).await?;
        transactions::emit(&mut tx,org.as_str(),seq,"execution.startup_authorized",serde_json::json!({"execution_id":id,"pod_uid":pod_uid,"grant_digest":grant_digest,"deadline_at_ms":dispatch.deadline_at_ms,"process_started_confirmed":false})).await?;
        bound_authority(&mut tx, org.as_str(), &record).await?;
        let owner = authority::row(&mut tx, org.as_str(), &dispatch.execution.lease_id).await?;
        if transactions::now(&mut tx).await? >= dispatch.deadline_at_ms
            || !authority::active(&mut tx, org.as_str(), &owner).await?
        {
            return Err(Error::WriterLeaseInactive);
        }
        let result = ExecutionStartupAttempt {
            grant: receipt(&mut tx, org.as_str(), id)
                .await?
                .ok_or(Error::InvalidStoredData)?,
            deadline: local_start + Duration::from_millis(budget.into()),
        };
        result.remaining_budget_ms()?;
        tx.commit().await?;
        Ok(result)
    }

    /// Recovery data only. Re-reading does not renew authorization or permit
    /// another runtime instance, challenge or external mutation.
    pub async fn candidate_execution_startup(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<Option<ExecutionStartupGrant>> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        let result = receipt(&mut tx, org.as_str(), id).await?;
        if let Some(value) = &result {
            pods::require_observed(&mut tx, org.as_str(), id, &value.pod_uid)
                .await
                .map_err(|error| match error {
                    Error::RuntimeConflict => Error::InvalidStoredData,
                    other => other,
                })?;
        }
        if let Some(value) = &result
            && !value
                .challenge
                .matches(&intent(&mut tx, org.as_str(), &record).await?.bootstrap()?)
                .map_err(|_| Error::InvalidStoredData)?
        {
            return Err(Error::InvalidStoredData);
        }
        tx.commit().await?;
        Ok(result)
    }
}
