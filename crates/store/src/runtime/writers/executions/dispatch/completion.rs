//! Atomic publication of a live process/IO seal, accepted outcome and writer drain.
use super::*;
use agent_computer_node::SealedExecution;
use agent_computer_sandbox::Outcome;
use serde_json::json;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionCompletion {
    pub execution_id: String,
    pub seal_digest: String,
    pub output_manifest_digest: Option<String>,
    pub accepted_state: ExecutionState,
    pub completed_at_ms: i64,
}

fn outcome(state: ExecutionState, authorized: bool, report: Option<Outcome>) -> ExecutionState {
    if state == ExecutionState::CancelRequested {
        return ExecutionState::Cancelled;
    }
    if state != ExecutionState::Dispatching || !authorized {
        return ExecutionState::Unknown;
    }
    match report {
        Some(Outcome::Succeeded) => ExecutionState::Succeeded,
        Some(
            Outcome::Failed
            | Outcome::SpawnFailed
            | Outcome::TimedOut
            | Outcome::DescendantsTerminated,
        ) => ExecutionState::Failed,
        _ => ExecutionState::Unknown,
    }
}

async fn load(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
) -> Result<Option<ExecutionCompletion>> {
    let record = sqlx::query(
        "SELECT * FROM execution_completions WHERE organization=$1 AND execution_id=$2",
    )
    .bind(org)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    record
        .map(|r| {
            Ok(ExecutionCompletion {
                execution_id: id.into(),
                seal_digest: r.try_get("seal_digest")?,
                output_manifest_digest: r.try_get("output_manifest_digest")?,
                accepted_state: serde_json::from_value(
                    r.try_get::<String, _>("accepted_state")?.into(),
                )
                .map_err(|_| Error::InvalidStoredData)?,
                completed_at_ms: r.try_get("completed_at_ms")?,
            })
        })
        .transpose()
}

impl Store {
    /// First publication requires the original dispatch handle and an opaque
    /// live node/IO seal. A receipt retry never mutates a later writer epoch.
    pub async fn finish_candidate_execution(
        &self,
        attempt: &ExecutionDispatchAttempt,
        sealed: &SealedExecution,
    ) -> Result<ExecutionCompletion> {
        let original = attempt.intent();
        let org = original.organization.as_str();
        let id = original.execution.execution_id.as_str();
        let seal =
            serde_json::to_value(sealed.evidence()).map_err(|_| Error::InvalidReconcileResult)?;
        let seal_digest = digest("agent-computer/execution-seal-v1", &seal)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let record = row(&mut tx, org, id).await?;
        let dispatch = intent(&mut tx, org, &record).await?;
        let arm = watchdogs::receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::InvalidReconcileResult)?;
        let mount = &arm.evidence["runtime"]["workspace_mount"];
        if dispatch.intent_digest != original.intent_digest
            || seal["arm"] != arm.evidence
            || mount.is_null()
            || seal["io"]["instance"] != mount["instance"]
            || seal["io"]["prepared"] != dispatch.binding["prepared"]
            || seal["io"]["prepared"] != mount["prepared"]
        {
            return Err(Error::InvalidReconcileResult);
        }
        if let Some(existing) = load(&mut tx, org, id).await? {
            if existing.seal_digest != seal_digest {
                return Err(Error::IdempotencyConflict);
            }
            tx.commit().await?;
            return Ok(existing);
        }
        if !matches!(
            dispatch.execution.state,
            ExecutionState::Dispatching | ExecutionState::CancelRequested | ExecutionState::Unknown
        ) {
            return Err(Error::RuntimeConflict);
        }
        let lease = authority::row(&mut tx, org, &dispatch.execution.lease_id).await?;
        if lease.try_get::<i64, _>("epoch")? != dispatch.execution.epoch
            || !matches!(
                lease.try_get::<String, _>("state")?.as_str(),
                "Held" | "Draining"
            )
        {
            return Err(Error::WriterLeaseConflict);
        }
        // Credential/principal revocation does not take the organization lock.
        sqlx::query("SELECT c.credential_id FROM connection_sessions s JOIN service_credentials c ON c.organization=s.organization AND c.credential_id=s.credential_id JOIN principals p ON p.organization=s.organization AND p.principal=s.principal WHERE s.organization=$1 AND s.session_id=$2 FOR SHARE OF c,p")
            .bind(org).bind(record.try_get::<String,_>("session_id")?).fetch_one(&mut *tx).await?;
        let authorized = lease.try_get::<String, _>("state")? == "Held"
            && authority::active(&mut tx, org, &lease).await?
            && transactions::now(&mut tx).await? < dispatch.deadline_at_ms
            && attempt.remaining_budget_ms().is_ok();
        let output = outputs::verified_outcome(&mut tx, &dispatch, &arm).await?;
        let accepted = outcome(
            dispatch.execution.state,
            authorized,
            output.as_ref().map(|(_, v)| *v),
        );
        let state = serde_json::to_value(accepted).map_err(|_| Error::InvalidStoredData)?;
        let state = state.as_str().ok_or(Error::InvalidStoredData)?;
        let completed_at = transactions::now(&mut tx).await?;
        let manifest = output.as_ref().map(|(digest, _)| digest.as_str());
        sqlx::query("INSERT INTO execution_completions (organization,execution_id,lease_id,epoch,dispatch_digest,arm_digest,seal,seal_digest,output_manifest_digest,accepted_state,completed_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(org).bind(id).bind(&dispatch.execution.lease_id).bind(dispatch.execution.epoch).bind(&dispatch.intent_digest)
            .bind(&arm.evidence_digest).bind(&seal).bind(&seal_digest).bind(manifest).bind(state).bind(completed_at).execute(&mut *tx).await?;
        if dispatch.execution.state != ExecutionState::Unknown {
            let reason = match accepted {
                ExecutionState::Succeeded | ExecutionState::Failed => "completed",
                ExecutionState::Cancelled => "completed_cancelled",
                _ => "completion_unconfirmed",
            };
            sqlx::query("UPDATE execution_requests SET state=$3,reason=$4,revision=revision+1 WHERE organization=$1 AND execution_id=$2")
                .bind(org).bind(id).bind(state).bind(reason).execute(&mut *tx).await?;
        }
        let seq = transactions::emit(&mut tx,org,seq,"execution.completed",json!({"execution_id":id,"state":state,"seal_digest":seal_digest,"output_manifest_digest":manifest,"process_termination_confirmed":true,"io_drain_confirmed":true})).await?;
        drain(&mut tx, org, &dispatch.execution.lease_id, seq).await?;
        if matches!(accepted, ExecutionState::Succeeded | ExecutionState::Failed)
            && (!authority::active(&mut tx, org, &lease).await?
                || transactions::now(&mut tx).await? >= dispatch.deadline_at_ms
                || attempt.remaining_budget_ms().is_err())
        {
            return Err(Error::WriterLeaseInactive);
        }
        let result = load(&mut tx, org, id)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        tx.commit().await?;
        Ok(result)
    }

    /// Historical completion metadata is queryable, but never restores its live seal.
    pub async fn candidate_execution_completion(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<Option<ExecutionCompletion>> {
        valid_id(id)?;
        let mut tx = self.pool.begin().await?;
        let result = load(&mut tx, org.as_str(), id).await?;
        tx.commit().await?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_outcome_requires_authority_and_verified_output_except_explicit_cancellation() {
        assert_eq!(
            outcome(ExecutionState::Dispatching, true, Some(Outcome::Succeeded)),
            ExecutionState::Succeeded
        );
        assert_eq!(
            outcome(ExecutionState::Dispatching, true, Some(Outcome::TimedOut)),
            ExecutionState::Failed
        );
        for state in [ExecutionState::Dispatching, ExecutionState::Unknown] {
            assert_eq!(
                outcome(state, false, Some(Outcome::Succeeded)),
                ExecutionState::Unknown
            );
            assert_eq!(outcome(state, true, None), ExecutionState::Unknown);
        }
        assert_eq!(
            outcome(ExecutionState::Unknown, true, Some(Outcome::Succeeded)),
            ExecutionState::Unknown
        );
        assert_eq!(
            outcome(ExecutionState::CancelRequested, false, None),
            ExecutionState::Cancelled
        );
        assert_eq!(
            outcome(
                ExecutionState::Dispatching,
                true,
                Some(Outcome::LeaseExpired)
            ),
            ExecutionState::Unknown
        );
    }
}
