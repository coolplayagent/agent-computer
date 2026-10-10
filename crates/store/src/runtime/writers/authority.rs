use super::*;
use agent_computer_storage::Prepared;
use sqlx::postgres::PgRow;

pub(super) async fn row(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<PgRow> {
    sqlx::query("SELECT l.*,r.computer_id,r.candidate_id,r.workspace_id,r.generation, EXISTS(SELECT 1 FROM candidate_writer_dispatches d WHERE d.organization=l.organization AND d.lease_id=l.lease_id AND d.epoch=l.epoch) AS dispatched, (SELECT proof FROM candidate_writer_drains d WHERE d.organization=l.organization AND d.lease_id=l.lease_id AND d.epoch=l.epoch) AS proof, (SELECT accepted FROM candidate_writer_completions d WHERE d.organization=l.organization AND d.lease_id=l.lease_id AND d.epoch=l.epoch) AS file_edit FROM candidate_writer_leases l JOIN runtime_start_requests r USING(organization,request_id) WHERE l.organization=$1 AND l.lease_id=$2")
        .bind(org).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)
}

pub(super) async fn prepared(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    computer: &str,
    generation: i64,
    candidate: &str,
) -> Result<(String, String, Prepared)> {
    prepared_inner(tx, org, computer, generation, candidate, false).await
}

async fn prepared_inner(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    computer: &str,
    generation: i64,
    candidate: &str,
    cancelling: bool,
) -> Result<(String, String, Prepared)> {
    let row=sqlx::query("SELECT r.request_id,r.workspace_id,p.receipt FROM runtime_controls c JOIN runtime_start_requests r ON r.organization=c.organization AND r.computer_id=c.computer_id AND r.request_id=c.active_request AND r.generation=c.generation JOIN candidate_preparations p ON p.organization=r.organization AND p.request_id=r.request_id WHERE c.organization=$1 AND c.computer_id=$2 AND r.generation=$3 AND r.candidate_id=$4 AND (r.state='Prepared' OR ($5 AND r.state='Draining' AND EXISTS(SELECT 1 FROM artifact_commits a WHERE a.organization=r.organization AND a.request_id=r.request_id AND a.state='Draining' AND a.cancel_running))) AND p.receipt IS NOT NULL")
        .bind(org).bind(computer).bind(generation).bind(candidate).bind(cancelling).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeConflict)?;
    let request: String = row.try_get("request_id")?;
    // Catalog disable/revision changes still block new writes. A collaborator's
    // own grants are checked separately from the original start credential.
    super::super::start::graph::validate_catalogs(tx, org, &request).await?;
    let receipt: Prepared =
        serde_json::from_value(row.try_get("receipt")?).map_err(|_| Error::InvalidStoredData)?;
    Ok((request, row.try_get("workspace_id")?, receipt))
}

pub(super) async fn owner(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<connections::ConnectionSession> {
    connections::final_view(tx, token, identity, id).await
}

pub(super) async fn require(
    tx: &mut Transaction<'_, Postgres>,
    token: &str,
    identity: &AuthenticatedPrincipal,
    session: &str,
    computer: &str,
    generation: i64,
    candidate: &str,
) -> Result<(connections::ConnectionSession, Prepared)> {
    Store::authorize_service_in(tx, token, ServiceScope::RuntimeRead).await?;
    Store::authorize_service_in(tx, token, ServiceScope::RuntimeModify).await?;
    let connection = owner(tx, token, identity, session).await?;
    if connection.computer_id != computer {
        return Err(Error::RuntimeAccessUnavailable);
    }
    if connection.state != connections::ConnectionState::Active {
        return Err(Error::ConnectionInactive);
    }
    if ![RuntimePermission::Read, RuntimePermission::Modify]
        .iter()
        .all(|p| connection.capabilities.contains(p))
    {
        return Err(Error::RuntimeAccessUnavailable);
    }
    let (_, workspace, receipt) = prepared(
        tx,
        identity.organization().as_str(),
        computer,
        generation,
        candidate,
    )
    .await?;
    authorize_in(
        tx,
        token,
        &[
            RuntimeRequirement {
                kind: RuntimeKind::Workspace,
                resource_id: workspace.clone(),
                permission: RuntimePermission::Read,
                runtime_seconds: None,
            },
            RuntimeRequirement {
                kind: RuntimeKind::Workspace,
                resource_id: workspace,
                permission: RuntimePermission::Modify,
                runtime_seconds: None,
            },
        ],
    )
    .await?;
    Ok((connection, receipt))
}

/// Check the originally bound owner without accepting a credential-ID caller.
/// Used only to lower authority or expose current own-lease state.
pub(super) async fn active(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    row: &PgRow,
) -> Result<bool> {
    active_inner(tx, org, row, false).await
}

/// A durable stop already requested cancellation. Preserve that pending state
/// while its original identity is valid; this never grants a new dispatch.
pub(super) async fn cancellation_active(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    row: &PgRow,
) -> Result<bool> {
    active_inner(tx, org, row, true).await
}

async fn active_inner(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    row: &PgRow,
    cancelling: bool,
) -> Result<bool> {
    let session: String = row.try_get("session_id")?;
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM connection_sessions s JOIN service_credentials c ON c.credential_id=s.credential_id AND c.organization=s.organization AND c.principal=s.principal JOIN principals p ON p.organization=s.organization AND p.principal=s.principal WHERE s.organization=$1 AND s.session_id=$2 AND s.computer_id=$3 AND s.state='Active' AND s.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000) AND NOT c.revoked AND c.expires_at>clock_timestamp() AND p.enabled AND s.requested @> '[\"connect\",\"read\",\"modify\"]'::jsonb AND c.scopes @> ARRAY['runtime.connect','runtime.read','runtime.modify']::text[] AND (SELECT count(*) FROM runtime_grants g WHERE g.organization=s.organization AND g.principal=s.principal AND ((g.kind='computer' AND g.resource_id=$3 AND g.permission IN ('connect','read','modify')) OR (g.kind='workspace' AND g.resource_id=$4 AND g.permission IN ('read','modify'))))=5)")
        .bind(org).bind(session).bind(row.try_get::<String,_>("computer_id")?).bind(row.try_get::<String,_>("workspace_id")?).fetch_one(&mut **tx).await?;
    if !valid {
        return Ok(false);
    }
    match prepared_inner(
        tx,
        org,
        &row.try_get::<String, _>("computer_id")?,
        row.try_get("generation")?,
        &row.try_get::<String, _>("candidate_id")?,
        cancelling,
    )
    .await
    {
        Ok((_, _, receipt)) => Ok(digest("agent-computer/writer-prepared-v1", &receipt)?
            == row.try_get::<String, _>("prepared_digest")?),
        Err(Error::RuntimeConflict | Error::ReferenceUnavailable) => Ok(false),
        Err(e) => Err(e),
    }
}

pub(super) async fn view(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    row: &PgRow,
) -> Result<WriterLease> {
    let mut state: WriterLeaseState =
        serde_json::from_value(row.try_get::<String, _>("state")?.into())
            .map_err(|_| Error::InvalidStoredData)?;
    if state == WriterLeaseState::Held && !active(tx, org, row).await? {
        state = WriterLeaseState::Draining;
    }
    let now = transactions::now(tx).await?;
    if state == WriterLeaseState::Held && now >= row.try_get::<i64, _>("expires_at_ms")? {
        state = WriterLeaseState::Draining;
    }
    Ok(WriterLease {
        lease_id: row.try_get("lease_id")?,
        computer_id: row.try_get("computer_id")?,
        candidate_id: row.try_get("candidate_id")?,
        connection_session_id: row.try_get("session_id")?,
        generation: row.try_get("generation")?,
        epoch: row.try_get("epoch")?,
        revision: row.try_get("revision")?,
        state,
        expires_at_ms: row.try_get("expires_at_ms")?,
        checked_at_ms: now,
        dispatch_recorded: row.try_get("dispatched")?,
        release_proof: row.try_get("proof")?,
        file_edit: row
            .try_get::<Option<serde_json::Value>, _>("file_edit")?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| Error::InvalidStoredData)?,
    })
}

pub(super) fn command(row: &PgRow, input: &WriterLeaseCommand) -> Result<()> {
    if input.generation < 1 || input.epoch < 1 || input.expected_revision < 1 {
        return Err(Error::InvalidRuntimeRequest);
    }
    if row.try_get::<String, _>("session_id")? != input.connection_session_id
        || row.try_get::<i64, _>("generation")? != input.generation
        || row.try_get::<i64, _>("epoch")? != input.epoch
        || row.try_get::<i64, _>("revision")? != input.expected_revision
    {
        return Err(Error::WriterLeaseConflict);
    }
    Ok(())
}
