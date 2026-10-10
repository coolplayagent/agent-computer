use super::*;

async fn authority(tx: &mut Transaction<'_, Postgres>, record: &PgRow) -> Result<()> {
    let org: String = record.try_get("organization")?;
    let request: String = record.try_get("request_id")?;
    let revision = decode::<CommitArtifact>(record.try_get("input")?)?
        .expected_revision
        .checked_add(1)
        .ok_or(Error::CounterExhausted)?;
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_start_requests r JOIN runtime_controls c ON c.organization=r.organization AND c.computer_id=r.computer_id AND c.active_request=r.request_id AND c.generation=r.generation WHERE r.organization=$1 AND r.request_id=$2 AND r.state='Sealing' AND c.revision=$3)").bind(&org).bind(&request).bind(revision).fetch_one(&mut **tx).await?;
    if !active {
        return Err(Error::RuntimeConflict);
    }
    let principal: String = record.try_get("principal")?;
    let credential: String = record.try_get("credential_id")?;
    for need in requirements(
        &record.try_get::<String, _>("workspace_id")?,
        &record.try_get::<String, _>("computer_id")?,
        record.try_get("stop_after_commit")?,
    ) {
        let identity = Store::authorize_bound_runtime_in(
            tx,
            &org,
            &principal,
            &credential,
            need.permission.scope(),
        )
        .await?;
        require_in(tx, &identity, &need).await?;
    }
    if record.try_get("stop_after_commit")? {
        admission::available(tx, &org, &request, &principal).await?;
    }
    super::super::start::graph::validate_catalogs(tx, &org, &request).await
}
async fn live(tx: &mut Transaction<'_, Postgres>, lease: &ArtifactLease) -> Result<PgRow> {
    let record = row(tx, &lease.org, &lease.commit).await?;
    if record.try_get::<String, _>("state")? != "Capturing"
        || record.try_get::<i64, _>("lease_epoch")? != lease.epoch
        || record
            .try_get::<Option<String>, _>("lease_owner")?
            .as_deref()
            != Some(&lease.owner)
        || record
            .try_get::<Option<i64>, _>("lease_until_ms")?
            .is_none_or(|v| v <= 0)
    {
        return Err(Error::StaleReconcileLease);
    }
    if record.try_get::<i64, _>("lease_until_ms")? <= transactions::now(tx).await? {
        return Err(Error::StaleReconcileLease);
    }
    authority(tx, &record).await?;
    Ok(record)
}
impl Store {
    /// Releasing a read/upload worker cannot reopen the permanently sealed Candidate.
    pub async fn release_artifact_worker(&self, lease: &ArtifactLease) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, &lease.org).await?;
        sqlx::query("UPDATE artifact_commits SET lease_epoch=lease_epoch+1,lease_owner=NULL,lease_until_ms=NULL WHERE organization=$1 AND commit_id=$2 AND state='Capturing' AND lease_epoch=$3 AND lease_owner=$4")
            .bind(&lease.org).bind(&lease.commit).bind(lease.epoch).bind(&lease.owner).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn claim_artifact(
        &self,
        org: &OrganizationId,
        id: &str,
        owner: &crate::reconciliation::WorkerId,
    ) -> Result<Option<ArtifactLease>> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        let record = row(&mut tx, org.as_str(), id).await?;
        let now = transactions::now(&mut tx).await?;
        if record.try_get::<String, _>("state")? != "Capturing"
            || record
                .try_get::<Option<i64>, _>("lease_until_ms")?
                .is_some_and(|v| v > now)
        {
            tx.commit().await?;
            return Ok(None);
        }
        authority(&mut tx, &record).await?;
        let epoch = record
            .try_get::<i64, _>("lease_epoch")?
            .checked_add(1)
            .ok_or(Error::CounterExhausted)?;
        let source=sqlx::query("SELECT receipt,binding FROM candidate_preparations WHERE organization=$1 AND request_id=$2").bind(org.as_str()).bind(record.try_get::<String,_>("request_id")?).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE artifact_commits SET lease_epoch=$3,lease_owner=$4,lease_until_ms=$5 WHERE organization=$1 AND commit_id=$2").bind(org.as_str()).bind(id).bind(epoch).bind(owner.as_str()).bind(now+300_000).execute(&mut *tx).await?;
        let lease = ArtifactLease {
            org: org.as_str().into(),
            commit: id.into(),
            epoch,
            owner: owner.as_str().into(),
            prepared: decode(source.try_get("receipt")?)?,
            target: decode(source.try_get("binding")?)?,
            capture: record
                .try_get::<Option<serde_json::Value>, _>("capture")?
                .map(decode)
                .transpose()?,
        };
        authority(&mut tx, &record).await?;
        tx.commit().await?;
        Ok(Some(lease))
    }
    pub async fn renew_artifact(&self, lease: &ArtifactLease) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, &lease.org).await?;
        live(&mut tx, lease).await?;
        let until = transactions::now(&mut tx).await? + 300_000;
        sqlx::query(
            "UPDATE artifact_commits SET lease_until_ms=$3 WHERE organization=$1 AND commit_id=$2",
        )
        .bind(&lease.org)
        .bind(&lease.commit)
        .bind(until)
        .execute(&mut *tx)
        .await?;
        live(&mut tx, lease).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn record_artifact_capture(
        &self,
        lease: &ArtifactLease,
        capture: &CapturedArtifact,
    ) -> Result<()> {
        let bundle = capture.bundle();
        if bundle.organization != lease.org
            || bundle.commit_id != lease.commit
            || bundle.prepared != lease.prepared
        {
            return Err(Error::InvalidReconcileResult);
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &lease.org).await?;
        let record = live(&mut tx, lease).await?;
        let encoded = serde_json::to_value(bundle).map_err(|_| Error::InvalidStoredData)?;
        if let Some(old) = record.try_get::<Option<serde_json::Value>, _>("capture")? {
            if old != encoded {
                return Err(Error::IdempotencyConflict);
            }
        } else {
            sqlx::query(
                "UPDATE artifact_commits SET capture=$3 WHERE organization=$1 AND commit_id=$2",
            )
            .bind(&lease.org)
            .bind(&lease.commit)
            .bind(encoded)
            .execute(&mut *tx)
            .await?;
            transactions::emit(&mut tx,&lease.org,seq,"artifact.captured",serde_json::json!({"commit_id":lease.commit,"manifest_digest":bundle.manifest.digest().map_err(|_|Error::InvalidStoredData)?})).await?;
        }
        live(&mut tx, lease).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn finish_artifact(
        &self,
        lease: &ArtifactLease,
        verified: &VerifiedArtifact,
    ) -> Result<ArtifactCommit> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, &lease.org).await?;
        let record = row(&mut tx, &lease.org, &lease.commit).await?;
        let recorded: Bundle = decode(
            record
                .try_get::<Option<serde_json::Value>, _>("capture")?
                .ok_or(Error::InvalidReconcileResult)?,
        )?;
        if recorded != *verified.bundle()
            || recorded.prepared != lease.prepared
            || recorded.organization != lease.org
            || recorded.commit_id != lease.commit
        {
            return Err(Error::InvalidReconcileResult);
        }
        if record.try_get::<String, _>("state")? != "Capturing" {
            if decode::<agent_computer_objects::ObjectRef>(record.try_get("object_ref")?)?
                != *verified.object()
            {
                return Err(Error::IdempotencyConflict);
            }
            let result = view(&record)?;
            tx.commit().await?;
            return Ok(result);
        }
        live(&mut tx, lease).await?;
        let input: CommitArtifact = decode(record.try_get("input")?)?;
        let workspace: String = record.try_get("workspace_id")?;
        let revision:i64=sqlx::query_scalar("SELECT COALESCE(max(revision),0)+1 FROM workspace_input_versions WHERE organization=$1 AND workspace_id=$2").bind(&lease.org).bind(&workspace).fetch_one(&mut *tx).await?;
        let manifest_digest = recorded
            .manifest
            .digest()
            .map_err(|_| Error::InvalidStoredData)?;
        sqlx::query("INSERT INTO workspace_input_versions (organization,workspace_id,revision,manifest,digest,origin,artifact_commit_id) VALUES ($1,$2,$3,$4,$5,'artifact',$6)").bind(&lease.org).bind(&workspace).bind(revision).bind(serde_json::to_value(&recorded.manifest).map_err(|_|Error::InvalidStoredData)?).bind(&manifest_digest).bind(&lease.commit).execute(&mut *tx).await?;
        let state = if input.publish_current {
            let changed=sqlx::query("UPDATE workspace_input_heads SET revision=$3 WHERE organization=$1 AND workspace_id=$2 AND revision=$4").bind(&lease.org).bind(&workspace).bind(revision).bind(input.base_revision).execute(&mut *tx).await?.rows_affected();
            if changed == 1 {
                "Committed"
            } else {
                "Conflict"
            }
        } else {
            "Committed"
        };
        let now = transactions::now(&mut tx).await?;
        sqlx::query("UPDATE artifact_commits SET state=$3,object_ref=$4,input_revision=$5,published_at_ms=$6 WHERE organization=$1 AND commit_id=$2").bind(&lease.org).bind(&lease.commit).bind(state).bind(serde_json::to_value(verified.object()).map_err(|_|Error::InvalidStoredData)?).bind(revision).bind(now).execute(&mut *tx).await?;
        // Check credentials and lease before removing the Sealing authority boundary.
        authority(&mut tx, &record).await?;
        if transactions::now(&mut tx).await? >= record.try_get::<i64, _>("lease_until_ms")? {
            return Err(Error::StaleReconcileLease);
        }
        sqlx::query("UPDATE runtime_start_requests SET state='Sealed' WHERE organization=$1 AND request_id=$2").bind(&lease.org).bind(&input.request_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE runtime_controls SET revision=revision+1 WHERE organization=$1 AND active_request=$2").bind(&lease.org).bind(&input.request_id).execute(&mut *tx).await?;
        let committed_seq = transactions::emit(&mut tx,&lease.org,seq,"artifact.committed",serde_json::json!({"commit_id":lease.commit,"workspace_id":workspace,"input_revision":revision,"manifest_digest":manifest_digest,"publish_current":input.publish_current,"state":state})).await?;
        if record.try_get("stop_after_commit")? {
            admission::available(
                &mut tx,
                &lease.org,
                &input.request_id,
                &record.try_get::<String, _>("principal")?,
            )
            .await?;
            super::super::start::commit_stop(
                &mut tx,
                &lease.org,
                &record.try_get::<String, _>("computer_id")?,
                &StopPreparedComputer {
                    request_id: input.request_id.clone(),
                    expected_revision: input
                        .expected_revision
                        .checked_add(2)
                        .ok_or(Error::CounterExhausted)?,
                },
                committed_seq,
            )
            .await?;
        }
        // Fresh principal authorization after the event/outbox too.
        for need in requirements(
            &workspace,
            &record.try_get::<String, _>("computer_id")?,
            record.try_get("stop_after_commit")?,
        ) {
            let identity = Store::authorize_bound_runtime_in(
                &mut tx,
                &lease.org,
                &record.try_get::<String, _>("principal")?,
                &record.try_get::<String, _>("credential_id")?,
                need.permission.scope(),
            )
            .await?;
            require_in(&mut tx, &identity, &need).await?;
        }
        // Outbox work may outlive the lease even while the stream lock excludes
        // other writers. A terminal state must not commit on expired authority.
        if transactions::now(&mut tx).await? >= record.try_get::<i64, _>("lease_until_ms")? {
            return Err(Error::StaleReconcileLease);
        }
        super::super::start::graph::validate_catalogs(&mut tx, &lease.org, &input.request_id)
            .await?;
        let result = view(&row(&mut tx, &lease.org, &lease.commit).await?)?;
        tx.commit().await?;
        Ok(result)
    }
    /// Trusted preparation worker: returns only the immutable artifact selected
    /// by the original authorized start admission, never a caller-chosen digest.
    pub async fn candidate_input_artifact(
        &self,
        org: &OrganizationId,
        request: &str,
    ) -> Result<Option<Bundle>> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        super::super::start::graph::authorize_bound(&mut tx, org.as_str(), request).await?;
        let captured:Option<serde_json::Value>=sqlx::query_scalar("SELECT a.capture FROM runtime_start_inputs i JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision JOIN artifact_commits a ON a.organization=v.organization AND a.commit_id=v.artifact_commit_id AND a.workspace_id=v.workspace_id AND a.input_revision=v.revision WHERE i.organization=$1 AND i.request_id=$2 AND a.state<>'Capturing'").bind(org.as_str()).bind(request).fetch_optional(&mut *tx).await?;
        let result = captured.map(decode).transpose()?;
        tx.commit().await?;
        Ok(result)
    }
}
