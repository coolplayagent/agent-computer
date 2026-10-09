use super::*;
use agent_computer_storage::{
    MountedVolume,
    files::{ClosedFileEdit, FileEdit, FileEditReport, FileEditState},
};

/// A consumed dispatch and the bounded file adapter's sealed outcome. This cannot
/// be deserialized from a caller's claimed stopped flag or synthetic IO receipt.
pub struct ClosedWriter {
    permit: WriterDispatchPermit,
    storage: ClosedFileEdit,
}
impl WriterDispatchPermit {
    /// Consumes the only dispatch handle. The worker owns the mount and no
    /// writable descriptor/task escapes the synchronous file adapter.
    pub fn edit_file(self, mount: MountedVolume, edit: &FileEdit) -> Result<ClosedWriter> {
        if edit
            .digest(&self.prepared)
            .map_err(|_| Error::InvalidRuntimeRequest)?
            != self.input_digest
        {
            return Err(Error::IdempotencyConflict);
        }
        let storage = mount
            .edit_file(&self.prepared, edit, self.deadline)
            .map_err(|_| Error::InvalidReconcileResult)?;
        Ok(ClosedWriter {
            permit: self,
            storage,
        })
    }
}
impl ClosedWriter {
    pub fn observed(&self) -> &FileEditReport {
        self.storage.report()
    }
}

impl Store {
    /// Exact intent retry/query. A journal without a completion remains unknown
    /// and never yields another IO permit, including after a worker restart.
    pub async fn candidate_file_edit_result(
        &self,
        token: &str,
        id: &str,
        input: &WriterLeaseCommand,
        dispatch: &str,
        edit: &FileEdit,
    ) -> Result<Option<WriterLease>> {
        valid_id(dispatch)?;
        edit.validate().map_err(|_| Error::InvalidRuntimeRequest)?;
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let row = own(&mut tx, token, &identity, id).await?;
        if row.try_get::<String, _>("session_id")? != input.connection_session_id
            || row.try_get::<i64, _>("generation")? != input.generation
            || row.try_get::<i64, _>("epoch")? != input.epoch
        {
            return Err(Error::WriterLeaseConflict);
        }
        let Some(record) = sqlx::query("SELECT d.input_digest,p.receipt FROM candidate_writer_dispatches d JOIN candidate_writer_leases l USING(organization,lease_id) JOIN candidate_preparations p USING(organization,request_id) WHERE d.organization=$1 AND d.lease_id=$2 AND d.epoch=$3 AND d.dispatch_id=$4").bind(identity.organization().as_str()).bind(id).bind(input.epoch).bind(dispatch).fetch_optional(&mut *tx).await? else { tx.commit().await?; return Ok(None) };
        let prepared = serde_json::from_value(record.try_get("receipt")?)
            .map_err(|_| Error::InvalidStoredData)?;
        if edit
            .digest(&prepared)
            .map_err(|_| Error::InvalidRuntimeRequest)?
            != record.try_get::<String, _>("input_digest")?
        {
            return Err(Error::IdempotencyConflict);
        }
        if row
            .try_get::<Option<serde_json::Value>, _>("file_edit")?
            .is_none()
        {
            return Err(Error::DispatchAlreadyStarted);
        }
        let result = current(&mut tx, token, &identity, id).await?;
        tx.commit().await?;
        Ok(Some(result))
    }
    /// Preview only for a trusted worker to bind normalized file intent. Dispatch
    /// rechecks authority; this receipt is never itself a write capability.
    pub async fn candidate_writer_storage(
        &self,
        token: &str,
        id: &str,
        input: &WriterLeaseCommand,
    ) -> Result<(
        agent_computer_storage::Prepared,
        super::super::preparation::PreparationTarget,
    )> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        let (row, _, prepared) = live(&mut tx, token, &identity, id).await?;
        authority::command(&row, input)?;
        let target: serde_json::Value = sqlx::query_scalar(
            "SELECT binding FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(identity.organization().as_str())
        .bind(row.try_get::<String, _>("request_id")?)
        .fetch_one(&mut *tx)
        .await?;
        let target = serde_json::from_value(target).map_err(|_| Error::InvalidStoredData)?;
        tx.commit().await?;
        Ok((prepared, target))
    }

    /// Persist only a sealed, locally consumed bounded file outcome. Rechecking
    /// authorization decides whether Applied can be accepted; closed IO evidence
    /// may still lower authority after credential revocation or lease expiry.
    pub async fn finish_candidate_file_edit(&self, closed: &ClosedWriter) -> Result<WriterLease> {
        let p = &closed.permit;
        let org = &p.organization;
        let id = &p.lease.lease_id;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let digest = digest("agent-computer/writer-prepared-v1", &p.prepared)?;
        let observed =
            serde_json::to_value(closed.observed()).map_err(|_| Error::InvalidStoredData)?;
        if closed.storage.input_digest() != p.input_digest {
            return Err(Error::InvalidReconcileResult);
        }
        if let Some(old) = sqlx::query("SELECT lease_id,epoch,prepared_digest,input_digest,observed FROM candidate_writer_completions WHERE organization=$1 AND dispatch_id=$2").bind(org).bind(&p.dispatch_id).fetch_optional(&mut *tx).await? {
            if old.try_get::<String,_>("lease_id")? != *id || old.try_get::<i64,_>("epoch")? != p.lease.epoch
                || old.try_get::<String,_>("prepared_digest")? != digest || old.try_get::<String,_>("input_digest")? != p.input_digest
                || old.try_get::<serde_json::Value,_>("observed")? != observed { return Err(Error::IdempotencyConflict); }
            let row = authority::row(&mut tx, org, id).await?;
            if row.try_get::<i64,_>("epoch")? != p.lease.epoch { return Err(Error::WriterLeaseConflict); }
            let result = authority::view(&mut tx, org, &row).await?;
            tx.commit().await?;
            return Ok(result);
        }
        let row = authority::row(&mut tx, org, id).await?;
        if row.try_get::<i64, _>("epoch")? != p.lease.epoch
            || row.try_get::<String, _>("prepared_digest")? != digest
        {
            return Err(Error::WriterLeaseConflict);
        }
        // Credential revocation does not acquire the organization lock. Hold the
        // same principal/credential share locks as ordinary effect admission.
        sqlx::query("SELECT c.credential_id FROM connection_sessions s JOIN service_credentials c ON c.organization=s.organization AND c.credential_id=s.credential_id JOIN principals p ON p.organization=s.organization AND p.principal=s.principal WHERE s.organization=$1 AND s.session_id=$2 FOR SHARE OF c,p").bind(org).bind(&p.lease.connection_session_id).fetch_one(&mut *tx).await?;
        let authorized = row.try_get::<String, _>("state")? == "Held"
            && row.try_get::<i64, _>("expires_at_ms")? > transactions::now(&mut tx).await?
            && authority::active(&mut tx, org, &row).await?;
        let mut accepted = closed.observed().clone();
        if accepted.state == FileEditState::Applied && !authorized {
            accepted.state = FileEditState::Unknown;
        }
        let accepted = serde_json::to_value(&accepted).map_err(|_| Error::InvalidStoredData)?;
        sqlx::query("INSERT INTO candidate_writer_completions (organization,dispatch_id,lease_id,epoch,prepared_digest,input_digest,observed,accepted) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)").bind(org).bind(&p.dispatch_id).bind(id).bind(p.lease.epoch).bind(digest).bind(&p.input_digest).bind(observed).bind(&accepted).execute(&mut *tx).await?;
        let seq = transactions::emit(&mut tx,org,seq,"writer.file_completed",serde_json::json!({"lease_id":id,"epoch":p.lease.epoch,"dispatch_id":p.dispatch_id,"result":accepted})).await?;
        drain(&mut tx, org, id, seq).await?;
        if authorized
            && (!authority::active(&mut tx, org, &row).await?
                || transactions::now(&mut tx).await? >= row.try_get::<i64, _>("expires_at_ms")?)
        {
            return Err(Error::WriterLeaseInactive);
        }
        let row = authority::row(&mut tx, org, id).await?;
        let result = authority::view(&mut tx, org, &row).await?;
        tx.commit().await?;
        Ok(result)
    }
}
