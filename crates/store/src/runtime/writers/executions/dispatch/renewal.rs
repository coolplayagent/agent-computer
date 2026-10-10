//! Fresh authorization, then durable acknowledgment by the two original guards.
//! Neither phase reconstructs the original worker, attach stream or kernel seal.
use super::*;
use agent_computer_node::ArmedGuard;
use agent_computer_sandbox::renewal::{Challenge, Grant};
use agent_computer_watchdog::renewal::{Command, Receipt};

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionRenewalGrant {
    pub sequence: u32,
    pub challenge: Challenge,
    pub grant: Grant,
    pub node_command: Command,
    pub grant_digest: String,
    pub previous_grant_digest: String,
    pub renewal_digest: String,
    pub granted_at_ms: i64,
    pub previous_deadline_at_ms: i64,
    pub deadline_at_ms: i64,
}
impl ExecutionRenewalGrant {
    fn hash(&self, dispatch: &ExecutionDispatchIntent) -> Result<String> {
        digest(
            "agent-computer/execution-renewal-authorization-v1",
            &(
                &dispatch.organization,
                &dispatch.execution.execution_id,
                &dispatch.intent_digest,
                self.sequence,
                &self.challenge,
                &self.grant,
                &self.node_command,
                &self.grant_digest,
                &self.previous_grant_digest,
                self.granted_at_ms,
                self.previous_deadline_at_ms,
                self.deadline_at_ms,
            ),
        )
    }
}
#[derive(Debug)]
pub struct ExecutionRenewalAttempt {
    grant: ExecutionRenewalGrant,
    intent_digest: String,
    previous_deadline: Instant,
    deadline: Instant,
}
impl ExecutionRenewalAttempt {
    pub fn grant(&self) -> &ExecutionRenewalGrant {
        &self.grant
    }
}

pub(super) struct Acknowledgment {
    pub grant: ExecutionRenewalGrant,
    pub evidence: [Receipt; 2],
}
async fn load(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
    sequence: u32,
) -> Result<Option<ExecutionRenewalGrant>> {
    let Some(row) = sqlx::query("SELECT * FROM execution_renewal_grants WHERE organization=$1 AND execution_id=$2 AND sequence=$3")
        .bind(&dispatch.organization).bind(&dispatch.execution.execution_id).bind(sequence as i32).fetch_optional(&mut **tx).await? else { return Ok(None); };
    let value = ExecutionRenewalGrant {
        sequence: row.try_get::<i32, _>("sequence")? as u32,
        challenge: serde_json::from_value(row.try_get("challenge")?)
            .map_err(|_| Error::InvalidStoredData)?,
        grant: serde_json::from_value(row.try_get("grant_body")?)
            .map_err(|_| Error::InvalidStoredData)?,
        node_command: serde_json::from_value(row.try_get("node_command")?)
            .map_err(|_| Error::InvalidStoredData)?,
        grant_digest: row.try_get("grant_digest")?,
        previous_grant_digest: row.try_get("previous_grant_digest")?,
        renewal_digest: row.try_get("renewal_digest")?,
        granted_at_ms: row.try_get("granted_at_ms")?,
        previous_deadline_at_ms: row.try_get("previous_deadline_at_ms")?,
        deadline_at_ms: row.try_get("deadline_at_ms")?,
    };
    if value.sequence != sequence
        || value.challenge.sequence != sequence
        || value.node_command.sequence != sequence
        || value.grant.digest().map_err(|_| Error::InvalidStoredData)? != value.grant_digest
        || value
            .challenge
            .digest()
            .map_err(|_| Error::InvalidStoredData)?
            != value.grant.challenge_digest
        || value.node_command.grant_digest != value.grant_digest
        || value.hash(dispatch)? != value.renewal_digest
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(Some(value))
}
fn ack_hash(
    dispatch: &ExecutionDispatchIntent,
    grant: &ExecutionRenewalGrant,
    evidence: &[Receipt; 2],
    at: i64,
) -> Result<String> {
    digest(
        "agent-computer/execution-renewal-ack-v1",
        &(
            &dispatch.organization,
            &dispatch.execution.execution_id,
            &grant.renewal_digest,
            evidence,
            at,
        ),
    )
}
pub(super) async fn latest_ack(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
) -> Result<Option<Acknowledgment>> {
    let Some(row) = sqlx::query("SELECT * FROM execution_renewal_acks WHERE organization=$1 AND execution_id=$2 ORDER BY sequence DESC LIMIT 1")
        .bind(&dispatch.organization).bind(&dispatch.execution.execution_id).fetch_optional(&mut **tx).await? else { return Ok(None); };
    let grant = load(tx, dispatch, row.try_get::<i32, _>("sequence")? as u32)
        .await?
        .ok_or(Error::InvalidStoredData)?;
    let evidence: [Receipt; 2] =
        serde_json::from_value(row.try_get("evidence")?).map_err(|_| Error::InvalidStoredData)?;
    let at: i64 = row.try_get("acknowledged_at_ms")?;
    if ack_hash(dispatch, &grant, &evidence, at)? != row.try_get::<String, _>("evidence_digest")?
        || at < grant.granted_at_ms
        || at >= grant.previous_deadline_at_ms
    {
        return Err(Error::InvalidStoredData);
    }
    let arm = watchdogs::receipt(tx, dispatch)
        .await?
        .ok_or(Error::InvalidStoredData)?;
    validate_evidence(&arm, &grant, &evidence)?;
    Ok(Some(Acknowledgment { grant, evidence }))
}
fn validate_evidence(
    arm: &ExecutionWatchdogArm,
    grant: &ExecutionRenewalGrant,
    evidence: &[Receipt; 2],
) -> Result<()> {
    let request = serde_json::from_value(arm.evidence["armed"]["request"].clone())
        .map_err(|_| Error::InvalidStoredData)?;
    for (name, receipt) in ["armed", "backup_armed"].iter().zip(evidence) {
        let journal = serde_json::from_value(arm.evidence[*name]["journal"].clone())
            .map_err(|_| Error::InvalidStoredData)?;
        receipt
            .validate(&request, &journal)
            .map_err(|_| Error::InvalidStoredData)?;
        if receipt.command != grant.node_command {
            return Err(Error::InvalidStoredData);
        }
    }
    Ok(())
}
pub(super) async fn effective_deadline(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
) -> Result<i64> {
    if let Some(ack) = latest_ack(tx, dispatch).await? {
        return Ok(ack.grant.deadline_at_ms);
    }
    let mut deadline = dispatch.deadline_at_ms;
    if dispatch.hard_deadline_at_ms.is_some()
        && let Some(arm) = watchdogs::receipt(tx, dispatch).await?
    {
        deadline = deadline.min(arm.expires_at_ms);
    }
    Ok(deadline)
}

impl Store {
    pub async fn authorize_candidate_execution_renewal(
        &self,
        attempt: &ExecutionDispatchAttempt,
        challenge: &Challenge,
        guard: &mut ArmedGuard,
    ) -> Result<ExecutionRenewalAttempt> {
        attempt.remaining_budget_ms()?;
        challenge
            .validate()
            .map_err(|_| Error::InvalidRuntimeRequest)?;
        if challenge.sequence != attempt.renewal_sequence + 1 {
            return Err(Error::RuntimeConflict);
        }
        let anchor = Instant::now();
        let node_anchor = agent_computer_watchdog::boottime_ms();
        let original = attempt.intent();
        let (org, id) = (&original.organization, &original.execution.execution_id);
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let record = row(&mut tx, org, id).await?;
        let dispatch = intent(&mut tx, org, &record).await?;
        if dispatch.intent_digest != original.intent_digest
            || dispatch.execution.state != ExecutionState::Dispatching
        {
            return Err(Error::WriterLeaseInactive);
        }
        bound_authority(&mut tx, org, &record).await?;
        reconcile(&mut tx, org, &record, seq).await?;
        if row(&mut tx, org, id).await?.try_get::<String, _>("state")? != "Dispatching" {
            return Err(Error::WriterLeaseInactive);
        }
        let startup = startup::receipt(&mut tx, org, id)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        if startup.grant.hard_budget_ms.is_none()
            || challenge.startup_grant_digest != startup.grant_digest
        {
            return Err(Error::RuntimeConflict);
        }
        watchdogs::require_live(&mut tx, &dispatch, &startup.pod_uid, Some(guard)).await?;
        let previous = latest_ack(&mut tx, &dispatch).await?;
        if previous.as_ref().map_or(0, |a| a.grant.sequence) != attempt.renewal_sequence {
            return Err(Error::RuntimeConflict);
        }
        let arm = watchdogs::receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        let previous_deadline = previous
            .as_ref()
            .map_or(dispatch.deadline_at_ms.min(arm.expires_at_ms), |a| {
                a.grant.deadline_at_ms
            });
        let now = transactions::now(&mut tx).await?;
        if now >= previous_deadline {
            return Err(Error::WriterLeaseInactive);
        }
        let grant = if let Some(existing) = load(&mut tx, &dispatch, challenge.sequence).await? {
            if existing.challenge != *challenge
                || existing.previous_deadline_at_ms != previous_deadline
            {
                return Err(Error::IdempotencyConflict);
            }
            existing
        } else {
            let hard = dispatch.hard_deadline_at_ms.ok_or(Error::RuntimeConflict)?;
            let node_policy = guard
                .evidence()
                .armed
                .request
                .renewal
                .as_ref()
                .ok_or(Error::RuntimeConflict)?;
            // Each clock domain clamps the same window against its original
            // hard ceiling. Shrinking the wire budget by the node's remaining
            // time would charge transport delay twice and prompt another
            // challenge after the node had already reached its final window.
            let budget = agent_computer_sandbox::renewal::WINDOW_MS;
            let node_deadline =
                (node_anchor + u64::from(budget)).min(node_policy.hard_deadline_boottime_ms);
            let deadline = (now + i64::from(budget)).min(hard);
            if node_deadline <= guard.deadline_boottime_ms() || deadline <= previous_deadline {
                return Err(Error::WriterLeaseInactive);
            }
            let response = Grant {
                version: 1,
                challenge_digest: challenge
                    .digest()
                    .map_err(|_| Error::InvalidRuntimeRequest)?,
                lease_budget_ms: budget,
            };
            let grant_digest = response
                .digest()
                .map_err(|_| Error::InvalidRuntimeRequest)?;
            let node_command = Command {
                version: 1,
                request_digest: agent_computer_watchdog::renewal::request_digest(
                    &guard.evidence().armed.request,
                )
                .map_err(|_| Error::InvalidRuntimeRequest)?,
                sequence: challenge.sequence,
                grant_digest: grant_digest.clone(),
                deadline_boottime_ms: node_deadline,
            };
            let mut grant = ExecutionRenewalGrant {
                sequence: challenge.sequence,
                challenge: challenge.clone(),
                grant: response,
                node_command,
                grant_digest,
                previous_grant_digest: previous
                    .as_ref()
                    .map_or(startup.grant_digest.clone(), |a| {
                        a.grant.grant_digest.clone()
                    }),
                renewal_digest: String::new(),
                granted_at_ms: now,
                previous_deadline_at_ms: previous_deadline,
                deadline_at_ms: deadline,
            };
            grant.renewal_digest = grant.hash(&dispatch)?;
            sqlx::query("INSERT INTO execution_renewal_grants (organization,execution_id,sequence,challenge,grant_body,node_command,grant_digest,previous_grant_digest,renewal_digest,granted_at_ms,previous_deadline_at_ms,deadline_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
                .bind(org).bind(id).bind(grant.sequence as i32).bind(serde_json::to_value(&grant.challenge).map_err(|_| Error::InvalidStoredData)?)
                .bind(serde_json::to_value(&grant.grant).map_err(|_| Error::InvalidStoredData)?).bind(serde_json::to_value(&grant.node_command).map_err(|_| Error::InvalidStoredData)?)
                .bind(&grant.grant_digest).bind(&grant.previous_grant_digest).bind(&grant.renewal_digest).bind(now).bind(previous_deadline).bind(grant.deadline_at_ms).execute(&mut *tx).await?;
            transactions::emit(&mut tx, org, seq, "execution.renewal_authorized", serde_json::json!({"execution_id":id,"sequence":grant.sequence,"renewal_digest":grant.renewal_digest,"deadline_at_ms":grant.deadline_at_ms,"acknowledged":false})).await?;
            grant
        };
        bound_authority(&mut tx, org, &record).await?;
        let owner = authority::row(&mut tx, org, &dispatch.execution.lease_id).await?;
        if transactions::now(&mut tx).await? >= previous_deadline
            || !authority::active(&mut tx, org, &owner).await?
        {
            return Err(Error::WriterLeaseInactive);
        }
        watchdogs::require_live(&mut tx, &dispatch, &startup.pod_uid, Some(guard)).await?;
        attempt.remaining_budget_ms()?;
        let deadline = anchor
            + Duration::from_millis(
                grant
                    .node_command
                    .deadline_boottime_ms
                    .saturating_sub(node_anchor),
            );
        tx.commit().await?;
        attempt.remaining_budget_ms()?;
        Ok(ExecutionRenewalAttempt {
            grant,
            intent_digest: dispatch.intent_digest,
            previous_deadline: attempt.deadline,
            deadline,
        })
    }

    pub async fn acknowledge_candidate_execution_renewal(
        &self,
        attempt: &mut ExecutionDispatchAttempt,
        renewal: ExecutionRenewalAttempt,
        guard: &mut ArmedGuard,
    ) -> Result<()> {
        attempt.remaining_budget_ms()?;
        if renewal.intent_digest != attempt.intent.intent_digest
            || renewal.previous_deadline != attempt.deadline
            || renewal.grant.sequence != attempt.renewal_sequence + 1
        {
            return Err(Error::RuntimeConflict);
        }
        let proof = guard
            .renewal_evidence()
            .cloned()
            .ok_or(Error::RuntimeConflict)?;
        guard
            .remaining_budget_ms()
            .map_err(|_| Error::WriterLeaseInactive)?;
        let (org, id) = (
            &attempt.intent.organization,
            &attempt.intent.execution.execution_id,
        );
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let record = row(&mut tx, org, id).await?;
        let dispatch = intent(&mut tx, org, &record).await?;
        bound_authority(&mut tx, org, &record).await?;
        reconcile(&mut tx, org, &record, seq).await?;
        if dispatch.intent_digest != renewal.intent_digest
            || row(&mut tx, org, id).await?.try_get::<String, _>("state")? != "Dispatching"
        {
            return Err(Error::WriterLeaseInactive);
        }
        let grant = load(&mut tx, &dispatch, renewal.grant.sequence)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        if grant.renewal_digest != renewal.grant.renewal_digest {
            return Err(Error::RuntimeConflict);
        }
        let arm = watchdogs::receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        if serde_json::to_value(guard.evidence()).map_err(|_| Error::InvalidStoredData)?
            != arm.evidence
        {
            return Err(Error::RuntimeConflict);
        }
        validate_evidence(&arm, &grant, &proof)?;
        let now = transactions::now(&mut tx).await?;
        if now >= grant.previous_deadline_at_ms {
            return Err(Error::WriterLeaseInactive);
        }
        let evidence_digest = ack_hash(&dispatch, &grant, &proof, now)?;
        sqlx::query("INSERT INTO execution_renewal_acks (organization,execution_id,sequence,evidence,evidence_digest,acknowledged_at_ms) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(org).bind(id).bind(grant.sequence as i32).bind(serde_json::to_value(&proof).map_err(|_| Error::InvalidStoredData)?).bind(&evidence_digest).bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE candidate_writer_leases SET expires_at_ms=GREATEST(expires_at_ms,$4),revision=revision+1 WHERE organization=$1 AND lease_id=$2 AND epoch=$3 AND state='Held'")
            .bind(org).bind(&dispatch.execution.lease_id).bind(dispatch.execution.epoch).bind(grant.deadline_at_ms).execute(&mut *tx).await?;
        transactions::emit(&mut tx, org, seq, "execution.renewal_acknowledged", serde_json::json!({"execution_id":id,"sequence":grant.sequence,"renewal_digest":grant.renewal_digest,"evidence_digest":evidence_digest,"deadline_at_ms":grant.deadline_at_ms})).await?;
        bound_authority(&mut tx, org, &record).await?;
        let owner = authority::row(&mut tx, org, &dispatch.execution.lease_id).await?;
        if transactions::now(&mut tx).await? >= grant.previous_deadline_at_ms
            || !authority::active(&mut tx, org, &owner).await?
        {
            return Err(Error::WriterLeaseInactive);
        }
        attempt.remaining_budget_ms()?;
        guard
            .remaining_budget_ms()
            .map_err(|_| Error::WriterLeaseInactive)?;
        tx.commit().await?;
        attempt.deadline = renewal.deadline.min(attempt.hard_deadline);
        attempt.renewal_sequence = grant.sequence;
        attempt.remaining_budget_ms()?;
        Ok(())
    }
}
