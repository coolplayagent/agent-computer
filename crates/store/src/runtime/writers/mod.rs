//! Durable Candidate modification ownership. Expiry revokes admission, not IO.
mod authority;
mod executions;
mod files;
mod types;
use super::*;
use crate::plans::types::{digest, random_id};
use agent_computer_core::identity::{ComputerId, IdempotencyKey, OrganizationId};
pub use executions::{
    CancelExecution, ExecutionCommand, ExecutionDispatchAttempt, ExecutionDispatchIntent,
    ExecutionLifetime, ExecutionRequest, ExecutionState, SubmitExecution,
};
pub use files::ClosedWriter;
use sqlx::postgres::PgRow;
pub use types::*;

fn valid_id(id: &str) -> Result<()> {
    ComputerId::new(id)
        .map(|_| ())
        .map_err(|_| Error::InvalidRuntimeRequest)
}
fn deadline(now: i64, seconds: u32, session_until: i64) -> Result<i64> {
    if !(1..=30).contains(&seconds) {
        return Err(Error::InvalidRuntimeRequest);
    }
    let until = now
        .checked_add(i64::from(seconds) * 1000)
        .ok_or(Error::CounterExhausted)?
        .min(session_until);
    if until <= now {
        return Err(Error::ConnectionInactive);
    }
    Ok(until)
}
async fn own(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<PgRow> {
    valid_id(id)?;
    let row = authority::row(tx, identity.organization().as_str(), id).await?;
    authority::owner(
        tx,
        token,
        identity,
        &row.try_get::<String, _>("session_id")?,
    )
    .await?;
    Ok(row)
}
async fn current(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<WriterLease> {
    let row = own(tx, token, identity, id).await?;
    let result = authority::view(tx, identity.organization().as_str(), &row).await?;
    Store::authorize_service_in(tx, token, ServiceScope::RuntimeConnect).await?;
    Ok(result)
}
async fn live(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<(
    PgRow,
    connections::ConnectionSession,
    agent_computer_storage::Prepared,
)> {
    let row = own(tx, token, identity, id).await?;
    let (connection, prepared) = authority::require(
        tx,
        token,
        identity,
        &row.try_get::<String, _>("session_id")?,
        &row.try_get::<String, _>("computer_id")?,
        row.try_get("generation")?,
        &row.try_get::<String, _>("candidate_id")?,
    )
    .await?;
    if row.try_get::<String, _>("state")? != "Held"
        || row.try_get::<i64, _>("expires_at_ms")? <= transactions::now(tx).await?
    {
        return Err(Error::WriterLeaseInactive);
    }
    if digest("agent-computer/writer-prepared-v1", &prepared)?
        != row.try_get::<String, _>("prepared_digest")?
    {
        return Err(Error::InvalidStoredData);
    }
    Ok((row, connection, prepared))
}

/// Lower authority in the same transaction as closing a connection or revoking
/// a grant. The enclosing connection/grant event records the changed count.
pub(super) async fn invalidate_session(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    session: &str,
) -> Result<u64> {
    Ok(sqlx::query("UPDATE candidate_writer_leases SET state='Draining',revision=revision+1 WHERE organization=$1 AND session_id=$2 AND state='Held'")
        .bind(org).bind(session).execute(&mut **tx).await?.rows_affected())
}
pub(super) async fn invalidate_grant(
    tx: &mut Transaction<'_, Postgres>,
    grant: &RuntimeGrant<'_>,
) -> Result<u64> {
    if !matches!(
        (grant.kind, grant.permission),
        (
            RuntimeKind::Computer,
            RuntimePermission::Connect | RuntimePermission::Read | RuntimePermission::Modify
        ) | (
            RuntimeKind::Workspace,
            RuntimePermission::Read | RuntimePermission::Modify
        )
    ) {
        return Ok(0);
    }
    Ok(sqlx::query("UPDATE candidate_writer_leases l SET state='Draining',revision=l.revision+1 FROM connection_sessions s,runtime_start_requests r WHERE l.organization=$1 AND l.organization=s.organization AND l.session_id=s.session_id AND s.principal=$2 AND r.organization=l.organization AND r.request_id=l.request_id AND l.state='Held' AND (($3='computer' AND r.computer_id=$4) OR ($3='workspace' AND r.workspace_id=$4))")
        .bind(grant.organization.as_str()).bind(grant.principal.as_str()).bind(grant.kind.as_str()).bind(grant.resource_id).execute(&mut **tx).await?.rows_affected())
}

async fn drain(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    mut seq: i64,
) -> Result<()> {
    let row = authority::row(tx, org, id).await?;
    let state: String = row.try_get("state")?;
    let epoch: i64 = row.try_get("epoch")?;
    if state == "Released" {
        return Ok(());
    }
    if state == "Held" {
        sqlx::query("UPDATE candidate_writer_leases SET state='Draining',revision=revision+1 WHERE organization=$1 AND lease_id=$2").bind(org).bind(id).execute(&mut **tx).await?;
        seq = transactions::emit(
            tx,
            org,
            seq,
            "writer.draining",
            serde_json::json!({"lease_id":id,"epoch":epoch,"process_termination_confirmed":false}),
        )
        .await?;
    }
    // This proof is only about our admission journal. A dispatched writer needs
    // sealed bounded-file completion or real fencing evidence; no caller-supplied stopped flag is accepted.
    let proof = if !row.try_get::<bool, _>("dispatched")? {
        Some("no_dispatch")
    } else if sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM candidate_writer_completions WHERE organization=$1 AND lease_id=$2 AND epoch=$3 AND observed->>'drain_confirmed'='true')").bind(org).bind(id).bind(epoch).fetch_one(&mut **tx).await? {
        Some("bounded_file_drained")
    } else { None };
    if let Some(proof) = proof {
        seq = executions::cancel_reserved(tx, org, id, epoch, "writer_unavailable", seq).await?;
        sqlx::query("INSERT INTO candidate_writer_drains (organization,lease_id,epoch,proof) VALUES ($1,$2,$3,$4)").bind(org).bind(id).bind(epoch).bind(proof).execute(&mut **tx).await?;
        sqlx::query("UPDATE candidate_writer_leases SET state='Released',revision=revision+1 WHERE organization=$1 AND lease_id=$2").bind(org).bind(id).execute(&mut **tx).await?;
        transactions::emit(
            tx,
            org,
            seq,
            "writer.released",
            serde_json::json!({"lease_id":id,"epoch":epoch,"proof":proof}),
        )
        .await?;
    }
    Ok(())
}

impl Store {
    pub async fn acquire_candidate_writer(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        input: &AcquireWriterLease,
    ) -> Result<WriterLease> {
        valid_id(computer)?;
        valid_id(&input.candidate_id)?;
        valid_id(&input.connection_session_id)?;
        if input.generation < 1 || !(1..=30).contains(&input.duration_seconds) {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let hash = digest(
            "agent-computer/writer-acquire-v1",
            &(computer, input, crate::auth::token_id(token)?),
        )?;
        let operation = "runtime.writer-acquire.v1";
        if let Some((id, epoch)) =
            transactions::retry::<(String, i64)>(&mut tx, &identity, operation, key, &hash).await?
        {
            let result = current(&mut tx, token, &identity, &id).await?;
            if result.epoch != epoch {
                return Err(Error::WriterLeaseConflict);
            }
            tx.commit().await?;
            return Ok(result);
        }
        let (connection, prepared) = authority::require(
            &mut tx,
            token,
            &identity,
            &input.connection_session_id,
            computer,
            input.generation,
            &input.candidate_id,
        )
        .await?;
        let org = identity.organization().as_str();
        let (request, _, _) = authority::prepared(
            &mut tx,
            org,
            computer,
            input.generation,
            &input.candidate_id,
        )
        .await?;
        let previous=sqlx::query("SELECT lease_id,epoch,state,prepared_digest FROM candidate_writer_leases WHERE organization=$1 AND request_id=$2").bind(org).bind(&request).fetch_optional(&mut *tx).await?;
        let now = transactions::now(&mut tx).await?;
        let until = deadline(now, input.duration_seconds, connection.expires_at_ms)?;
        let prepared_digest = digest("agent-computer/writer-prepared-v1", &prepared)?;
        let (id, epoch) = if let Some(previous) = previous {
            if previous.try_get::<String, _>("state")? != "Released" {
                return Err(Error::WriterLeaseBusy);
            }
            if previous.try_get::<String, _>("prepared_digest")? != prepared_digest {
                return Err(Error::InvalidStoredData);
            }
            let id: String = previous.try_get("lease_id")?;
            let epoch = previous
                .try_get::<i64, _>("epoch")?
                .checked_add(1)
                .ok_or(Error::CounterExhausted)?;
            sqlx::query("UPDATE candidate_writer_leases SET state='Held',session_id=$3,epoch=$4,revision=revision+1,expires_at_ms=$5 WHERE organization=$1 AND lease_id=$2")
                .bind(org).bind(&id).bind(&input.connection_session_id).bind(epoch).bind(until).execute(&mut *tx).await?;
            (id, epoch)
        } else {
            let id = random_id("lease")?;
            sqlx::query("INSERT INTO candidate_writer_leases (organization,lease_id,request_id,session_id,epoch,revision,state,expires_at_ms,prepared_digest) VALUES ($1,$2,$3,$4,1,1,'Held',$5,$6)")
                .bind(org).bind(&id).bind(&request).bind(&input.connection_session_id).bind(until).bind(prepared_digest).execute(&mut *tx).await?;
            (id, 1)
        };
        sqlx::query("INSERT INTO candidate_writer_epochs (organization,lease_id,epoch,session_id,created_at_ms) VALUES ($1,$2,$3,$4,$5)").bind(org).bind(&id).bind(epoch).bind(&input.connection_session_id).bind(now).execute(&mut *tx).await?;
        transactions::emit(&mut tx,org,seq,"writer.acquired",serde_json::json!({"lease_id":id,"epoch":epoch,"connection_session_id":input.connection_session_id,"candidate_id":input.candidate_id,"generation":input.generation,"expires_at_ms":until})).await?;
        transactions::save_receipt(&mut tx, &identity, operation, key, &hash, &(&id, epoch))
            .await?;
        live(&mut tx, token, &identity, &id).await?;
        let result = current(&mut tx, token, &identity, &id).await?;
        if result.state != WriterLeaseState::Held {
            return Err(Error::WriterLeaseInactive);
        }
        tx.commit().await?;
        Ok(result)
    }

    pub async fn candidate_writer(&self, token: &str, id: &str) -> Result<WriterLease> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let result = current(&mut tx, token, &identity, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn renew_candidate_writer(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        input: &RenewWriterLease,
    ) -> Result<WriterLease> {
        if !(1..=30).contains(&input.duration_seconds) {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let operation = "runtime.writer-renew.v1";
        let hash = digest(
            "agent-computer/writer-renew-v1",
            &(id, input, crate::auth::token_id(token)?),
        )?;
        if let Some(epoch) =
            transactions::retry::<i64>(&mut tx, &identity, operation, key, &hash).await?
        {
            let result = current(&mut tx, token, &identity, id).await?;
            if result.epoch != epoch {
                return Err(Error::WriterLeaseConflict);
            }
            tx.commit().await?;
            return Ok(result);
        }
        let (row, connection, _) = live(&mut tx, token, &identity, id).await?;
        authority::command(&row, &input.lease)?;
        let now = transactions::now(&mut tx).await?;
        let until = deadline(now, input.duration_seconds, connection.expires_at_ms)?
            .max(row.try_get("expires_at_ms")?);
        sqlx::query("UPDATE candidate_writer_leases SET expires_at_ms=$3,revision=revision+1 WHERE organization=$1 AND lease_id=$2").bind(identity.organization().as_str()).bind(id).bind(until).execute(&mut *tx).await?;
        transactions::emit(
            &mut tx,
            identity.organization().as_str(),
            seq,
            "writer.renewed",
            serde_json::json!({"lease_id":id,"epoch":input.lease.epoch,"expires_at_ms":until}),
        )
        .await?;
        transactions::save_receipt(
            &mut tx,
            &identity,
            operation,
            key,
            &hash,
            &input.lease.epoch,
        )
        .await?;
        live(&mut tx, token, &identity, id).await?;
        let result = current(&mut tx, token, &identity, id).await?;
        if result.state != WriterLeaseState::Held {
            return Err(Error::WriterLeaseInactive);
        }
        tx.commit().await?;
        Ok(result)
    }

    pub async fn release_candidate_writer(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        input: &WriterLeaseCommand,
    ) -> Result<WriterLease> {
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let operation = "runtime.writer-release.v1";
        let hash = digest(
            "agent-computer/writer-release-v1",
            &(id, input, crate::auth::token_id(token)?),
        )?;
        if let Some(epoch) =
            transactions::retry::<i64>(&mut tx, &identity, operation, key, &hash).await?
        {
            let result = current(&mut tx, token, &identity, id).await?;
            if result.epoch != epoch {
                return Err(Error::WriterLeaseConflict);
            }
            tx.commit().await?;
            return Ok(result);
        }
        let row = own(&mut tx, token, &identity, id).await?;
        authority::command(&row, input)?;
        drain(&mut tx, identity.organization().as_str(), id, seq).await?;
        transactions::save_receipt(&mut tx, &identity, operation, key, &hash, &input.epoch).await?;
        let result = current(&mut tx, token, &identity, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Trusted one-shot maintenance. Only zero-dispatch or sealed bounded-file
    /// drain evidence can release ownership; uncertain effects stay Draining.
    pub async fn reconcile_candidate_writer(
        &self,
        org: &OrganizationId,
        id: &str,
    ) -> Result<WriterLease> {
        valid_id(id)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let row = authority::row(&mut tx, org.as_str(), id).await?;
        let result = authority::view(&mut tx, org.as_str(), &row).await?;
        if result.state == WriterLeaseState::Draining {
            drain(&mut tx, org.as_str(), id, seq).await?;
        }
        let row = authority::row(&mut tx, org.as_str(), id).await?;
        let result = authority::view(&mut tx, org.as_str(), &row).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Internal dispatch boundary for a supervised writer. The caller
    /// supplies a digest of its normalized operation, not a reusable HTTP permit.
    /// This implementation admits one writer dispatch per ownership epoch and
    /// deliberately provides no asserted-stopped/fence bypass.
    pub async fn begin_candidate_writer_dispatch(
        &self,
        token: &str,
        id: &str,
        input: &WriterLeaseCommand,
        dispatch: WriterDispatch<'_>,
    ) -> Result<WriterDispatchPermit> {
        let local_started = std::time::Instant::now();
        valid_id(dispatch.dispatch_id)?;
        if dispatch.input_digest.len() != 71
            || !dispatch.input_digest.starts_with("sha256:")
            || !dispatch.input_digest[7..]
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let (row, _, prepared) = live(&mut tx, token, &identity, id).await?;
        if row.try_get::<bool, _>("dispatched")? {
            return Err(Error::DispatchAlreadyStarted);
        }
        if executions::reserved(&mut tx, identity.organization().as_str(), id, input.epoch).await? {
            return Err(Error::WriterLeaseBusy);
        }
        authority::command(&row, input)?;
        let used:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM candidate_writer_dispatches WHERE organization=$1 AND dispatch_id=$2)").bind(identity.organization().as_str()).bind(dispatch.dispatch_id).fetch_one(&mut *tx).await?;
        if used {
            return Err(Error::IdempotencyConflict);
        }
        sqlx::query("INSERT INTO candidate_writer_dispatches (organization,dispatch_id,lease_id,epoch,input_digest) VALUES ($1,$2,$3,$4,$5)").bind(identity.organization().as_str()).bind(dispatch.dispatch_id).bind(id).bind(input.epoch).bind(dispatch.input_digest).execute(&mut *tx).await?;
        sqlx::query("UPDATE candidate_writer_leases SET revision=revision+1 WHERE organization=$1 AND lease_id=$2").bind(identity.organization().as_str()).bind(id).execute(&mut *tx).await?;
        transactions::emit(&mut tx,identity.organization().as_str(),seq,"writer.dispatched",serde_json::json!({"lease_id":id,"epoch":input.epoch,"dispatch_id":dispatch.dispatch_id,"input_digest":dispatch.input_digest})).await?;
        live(&mut tx, token, &identity, id).await?;
        let lease = current(&mut tx, token, &identity, id).await?;
        if lease.state != WriterLeaseState::Held {
            return Err(Error::WriterLeaseInactive);
        }
        tx.commit().await?;
        Ok(WriterDispatchPermit {
            organization: identity.organization().as_str().into(),
            deadline: local_started
                + std::time::Duration::from_millis(
                    (lease.expires_at_ms - lease.checked_at_ms).max(0) as u64,
                ),
            lease,
            dispatch_id: dispatch.dispatch_id.into(),
            input_digest: dispatch.input_digest.into(),
            prepared,
        })
    }
}
