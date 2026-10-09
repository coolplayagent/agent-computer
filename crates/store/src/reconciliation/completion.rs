use super::{state, types::*};
use crate::{Error, Result, Store, plans};
use sqlx::Row;

fn validate(lease: &ReconcileLease, result: &ReconcileOutcome, dispatched: bool) -> Result<()> {
    match result {
        ReconcileOutcome::Applied { receipt: r } => {
            if !dispatched
                || r.step_id != lease.task.step_id
                || r.resource_id != lease.task.resource_id
                || r.revision != lease.task.revision
                || r.spec_digest != lease.task.spec_digest
                || !identifier(&r.backend)
                || !identifier(&r.object_uid)
                || !identifier(&r.evidence_id)
            {
                return Err(Error::InvalidReconcileResult);
            }
        }
        ReconcileOutcome::Retry { delay_seconds, .. } if !(1..=3600).contains(delay_seconds) => {
            return Err(Error::InvalidReconcileResult);
        }
        ReconcileOutcome::Failed { .. } if dispatched => return Err(Error::InvalidReconcileResult),
        _ => {}
    }
    Ok(())
}

impl Store {
    /// Store one durable result per lease epoch. Unknown completion outcomes are
    /// retried with this exact lease/result, never by creating another effect.
    pub async fn finish_reconciliation(
        &self,
        lease: &ReconcileLease,
        outcome: ReconcileOutcome,
    ) -> Result<IntentProgress> {
        let org = &lease.task.organization;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        let hash = plans::types::digest("agent-computer/reconciliation-result-v1", &outcome)?;
        if let Some(row)=sqlx::query("SELECT input_digest,response FROM reconciliation_results WHERE organization=$1 AND step_id=$2 AND lease_epoch=$3")
            .bind(org).bind(&lease.task.step_id).bind(lease.epoch).fetch_optional(&mut *tx).await? {
            if row.try_get::<String,_>("input_digest")?!=hash { return Err(Error::IdempotencyConflict) }
            let previous=serde_json::from_value(row.try_get("response")?).map_err(|_|Error::InvalidStoredData)?;
            state::authorize(&mut tx,org,&lease.task.operation_id).await?;
            tx.commit().await?;
            return Ok(previous);
        }
        let row = state::lease_row(&mut tx, lease).await?;
        validate(lease, &outcome, row.try_get("dispatch_started")?)?;
        let now = plans::transactions::now(&mut tx).await?;
        let (status, reason, available, receipt) = match &outcome {
            ReconcileOutcome::Applied { receipt } => (
                "Succeeded",
                None,
                0,
                Some(serde_json::to_value(receipt).map_err(|_| Error::InvalidReconcileResult)?),
            ),
            ReconcileOutcome::Retry {
                reason,
                delay_seconds,
            } => (
                "Pending",
                Some(*reason),
                now.checked_add(i64::from(*delay_seconds) * 1000)
                    .ok_or(Error::CounterExhausted)?,
                None,
            ),
            ReconcileOutcome::Blocked { reason } => ("Blocked", Some(*reason), 0, None),
            ReconcileOutcome::Failed { reason } => ("Failed", Some(*reason), 0, None),
        };
        let event=plans::transactions::emit(&mut tx,org,seq,"reconciliation.result",serde_json::json!({"operation_id":lease.task.operation_id,"step_id":lease.task.step_id,"lease_epoch":lease.epoch,"state":status,"reason":reason})).await?;
        state::lease_row(&mut tx, lease).await?;
        sqlx::query("UPDATE reconcile_intents SET state=$3,reason_code=$4,available_at_ms=$5,lease_owner=NULL,lease_until_ms=NULL,event_sequence=$6 WHERE organization=$1 AND step_id=$2")
            .bind(org).bind(&lease.task.step_id).bind(status).bind(reason.map(ReconcileReason::as_str)).bind(available).bind(event).execute(&mut *tx).await?;
        state::operation_state(&mut tx, org, &lease.task.operation_id).await?;
        let response = state::current(&mut tx, org, &lease.task.step_id).await?;
        sqlx::query("INSERT INTO reconciliation_results (organization,step_id,lease_epoch,input_digest,response,receipt) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(org).bind(&lease.task.step_id).bind(lease.epoch).bind(hash).bind(serde_json::to_value(&response).map_err(|_|Error::InvalidStoredData)?).bind(receipt).execute(&mut *tx).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        state::check_deadline(&mut tx, row.try_get("lease_until_ms")?).await?;
        tx.commit().await?;
        Ok(response)
    }
}
