use super::{state, types::*};
use crate::{Error, Result, Store, plans};
use agent_computer_core::identity::OrganizationId;
use sqlx::Row;
use std::time::Duration;

impl Store {
    /// Abandon only blocked, undispatched remaining work. Completed resources
    /// stay intact; unresolved external effects prohibit this operation.
    pub async fn abandon_reconciliation(
        &self,
        org: &OrganizationId,
        operation: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let status: Option<String> = sqlx::query_scalar(
            "SELECT state FROM operations WHERE organization=$1 AND operation_id=$2",
        )
        .bind(org.as_str())
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await?;
        if status.as_deref() != Some("Blocked") {
            return Err(Error::OperationNotBlocked);
        }
        let uncertain:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM reconcile_intents WHERE organization=$1 AND operation_id=$2 AND dispatch_started AND state<>'Succeeded')")
            .bind(org.as_str()).bind(operation).fetch_one(&mut *tx).await?;
        if uncertain {
            return Err(Error::InvalidReconcileResult);
        }
        let event = plans::transactions::emit(
            &mut tx,
            org.as_str(),
            seq,
            "reconciliation.abandoned",
            serde_json::json!({"operation_id":operation}),
        )
        .await?;
        sqlx::query("UPDATE reconcile_intents SET state='Failed',reason_code='operator_abandoned',lease_owner=NULL,lease_until_ms=NULL,event_sequence=$3 WHERE organization=$1 AND operation_id=$2 AND state IN ('Pending','Blocked')")
            .bind(org.as_str()).bind(operation).bind(event).execute(&mut *tx).await?;
        state::operation_state(&mut tx, org.as_str(), operation).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Trusted worker entry point. Work in an operation is dependency ordered;
    /// shared resources also follow operation publication order across plans.
    pub async fn claim_reconciliation(
        &self,
        org: &OrganizationId,
        worker: &WorkerId,
        lifetime: Duration,
    ) -> Result<ClaimOutcome> {
        self.claim_reconciliation_filtered(org, worker, lifetime, None)
            .await
    }

    /// Claim only a backend's supported kind, preserving all dependency and
    /// cross-operation ordering. Unhandled kinds remain available to other workers.
    pub async fn claim_reconciliation_kind(
        &self,
        org: &OrganizationId,
        worker: &WorkerId,
        lifetime: Duration,
        kind: plans::DefinitionKind,
    ) -> Result<ClaimOutcome> {
        self.claim_reconciliation_filtered(org, worker, lifetime, Some(kind))
            .await
    }

    async fn claim_reconciliation_filtered(
        &self,
        org: &OrganizationId,
        worker: &WorkerId,
        lifetime: Duration,
        kind: Option<plans::DefinitionKind>,
    ) -> Result<ClaimOutcome> {
        let ttl = duration_ms(lifetime, 300)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let now = plans::transactions::now(&mut tx).await?;
        let row=sqlx::query("SELECT i.*,d.kind,v.spec,v.digest,v.dependencies FROM reconcile_intents i JOIN operations o USING (organization,operation_id) JOIN resource_definitions d USING (organization,resource_id) JOIN resource_spec_versions v ON v.organization=i.organization AND v.resource_id=i.resource_id AND v.revision=i.revision WHERE i.organization=$1 AND ($3::text IS NULL OR d.kind=$3) AND o.state IN ('Queued','Running') AND i.available_at_ms <= $2 AND (i.state='Pending' OR (i.state='Running' AND i.lease_until_ms <= $2)) AND NOT EXISTS (SELECT 1 FROM reconcile_intents earlier WHERE earlier.organization=i.organization AND earlier.operation_id=i.operation_id AND earlier.ordinal<i.ordinal AND earlier.state<>'Succeeded') AND NOT EXISTS (SELECT 1 FROM reconcile_intents older JOIN operations old_op USING (organization,operation_id) WHERE older.organization=i.organization AND older.resource_id=i.resource_id AND old_op.event_sequence<o.event_sequence AND old_op.state IN ('Queued','Running','Blocked') AND older.state NOT IN ('Succeeded','Failed')) ORDER BY o.event_sequence,i.ordinal LIMIT 1")
            .bind(org.as_str()).bind(now).bind(kind.map(|k|k.as_str())).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(ClaimOutcome::Idle);
        };
        let task = ReconcileTask {
            organization: org.as_str().into(),
            operation_id: row.try_get("operation_id")?,
            step_id: row.try_get("step_id")?,
            resource_id: row.try_get("resource_id")?,
            revision: row.try_get("revision")?,
            kind: row.try_get::<String, _>("kind")?.parse()?,
            spec_digest: row.try_get("digest")?,
            spec: row.try_get("spec")?,
            dependencies: serde_json::from_value(row.try_get("dependencies")?)
                .map_err(|_| Error::InvalidStoredData)?,
            requires_drain: row.try_get("requires_drain")?,
        };
        if let Err(error) = state::authorize(&mut tx, org.as_str(), &task.operation_id).await {
            let Some(reason) = state::denial(&error) else {
                return Err(error);
            };
            let result = state::blocked(
                &mut tx,
                org.as_str(),
                &task.operation_id,
                &task.step_id,
                seq,
                reason,
            )
            .await?;
            tx.commit().await?;
            return Ok(ClaimOutcome::Blocked(result));
        }
        let epoch = row
            .try_get::<i64, _>("lease_epoch")?
            .checked_add(1)
            .ok_or(Error::CounterExhausted)?;
        let until = plans::transactions::now(&mut tx)
            .await?
            .checked_add(ttl)
            .ok_or(Error::CounterExhausted)?;
        let mode = if row.try_get::<bool, _>("dispatch_started")? {
            ClaimMode::Observe
        } else {
            ClaimMode::Execute
        };
        let event=plans::transactions::emit(&mut tx,org.as_str(),seq,"reconciliation.claimed",serde_json::json!({"operation_id":task.operation_id,"step_id":task.step_id,"worker":worker.as_str(),"lease_epoch":epoch,"mode":mode})).await?;
        sqlx::query("UPDATE reconcile_intents SET state='Running',lease_epoch=$3,lease_owner=$4,lease_until_ms=$5,reason_code=NULL,event_sequence=$6 WHERE organization=$1 AND step_id=$2")
            .bind(org.as_str()).bind(&task.step_id).bind(epoch).bind(worker.as_str()).bind(until).bind(event).execute(&mut *tx).await?;
        state::operation_state(&mut tx, org.as_str(), &task.operation_id).await?;
        state::authorize(&mut tx, org.as_str(), &task.operation_id).await?;
        state::check_deadline(&mut tx, until).await?;
        tx.commit().await?;
        Ok(ClaimOutcome::Claimed(Box::new(ReconcileLease {
            task,
            owner: worker.clone(),
            epoch,
            until_ms: until,
            mode,
        })))
    }

    pub async fn renew_reconciliation(
        &self,
        lease: &ReconcileLease,
        lifetime: Duration,
    ) -> Result<i64> {
        let ttl = duration_ms(lifetime, 300)?;
        let org = &lease.task.organization;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let row = state::lease_row(&mut tx, lease).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        let until = plans::transactions::now(&mut tx)
            .await?
            .checked_add(ttl)
            .ok_or(Error::CounterExhausted)?
            .max(row.try_get("lease_until_ms")?);
        let event=plans::transactions::emit(&mut tx,org,seq,"reconciliation.renewed",serde_json::json!({"step_id":lease.task.step_id,"lease_epoch":lease.epoch,"lease_until_ms":until})).await?;
        // Check the old lease again after potentially slow authorization work;
        // a renewal must never resurrect an expired epoch.
        state::lease_row(&mut tx, lease).await?;
        sqlx::query("UPDATE reconcile_intents SET lease_until_ms=$3,event_sequence=$4 WHERE organization=$1 AND step_id=$2")
            .bind(org).bind(&lease.task.step_id).bind(until).bind(event).execute(&mut *tx).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        state::check_deadline(&mut tx, row.try_get("lease_until_ms")?).await?;
        tx.commit().await?;
        Ok(until)
    }

    /// Persist uncertainty BEFORE crossing an external side-effect boundary.
    /// A lost response is not permission to send the action a second time.
    pub async fn begin_reconciliation_dispatch(
        &self,
        lease: &ReconcileLease,
    ) -> Result<DispatchPermit> {
        let org = &lease.task.organization;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let row = state::lease_row(&mut tx, lease).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        if row.try_get::<bool, _>("dispatch_started")? {
            return Err(Error::DispatchAlreadyStarted);
        }
        let event=plans::transactions::emit(&mut tx,org,seq,"reconciliation.dispatch_started",serde_json::json!({"step_id":lease.task.step_id,"lease_epoch":lease.epoch,"resource_id":lease.task.resource_id,"revision":lease.task.revision})).await?;
        state::lease_row(&mut tx, lease).await?;
        sqlx::query("UPDATE reconcile_intents SET dispatch_started=TRUE,event_sequence=$3 WHERE organization=$1 AND step_id=$2")
            .bind(org).bind(&lease.task.step_id).bind(event).execute(&mut *tx).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        state::check_deadline(&mut tx, row.try_get("lease_until_ms")?).await?;
        tx.commit().await?;
        Ok(DispatchPermit {
            task: lease.task.clone(),
            epoch: lease.epoch,
        })
    }

    /// Trusted operator repair after addressing the recorded blocker. Resuming
    /// keeps dispatch uncertainty and effect identity; it does not grant a replay.
    pub async fn resume_reconciliation(&self, org: &OrganizationId, operation: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let status: Option<String> = sqlx::query_scalar(
            "SELECT state FROM operations WHERE organization=$1 AND operation_id=$2",
        )
        .bind(org.as_str())
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await?;
        if status.as_deref() != Some("Blocked") {
            return Err(Error::OperationNotBlocked);
        }
        state::authorize(&mut tx, org.as_str(), operation).await?;
        let event = plans::transactions::emit(
            &mut tx,
            org.as_str(),
            seq,
            "reconciliation.resumed",
            serde_json::json!({"operation_id":operation}),
        )
        .await?;
        sqlx::query("UPDATE reconcile_intents SET state='Pending',available_at_ms=0,reason_code=NULL,event_sequence=$3 WHERE organization=$1 AND operation_id=$2 AND state='Blocked'")
            .bind(org.as_str()).bind(operation).bind(event).execute(&mut *tx).await?;
        state::operation_state(&mut tx, org.as_str(), operation).await?;
        state::authorize(&mut tx, org.as_str(), operation).await?;
        tx.commit().await?;
        Ok(())
    }
}
