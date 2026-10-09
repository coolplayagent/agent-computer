use super::{state, types::*};
use crate::{Error, Result, Store, plans};
use sqlx::Row;

impl Store {
    /// Record a backend identity under a live authorized lease, after dispatch.
    /// Exact retries return the existing binding; replacement identities fail closed.
    pub async fn record_reconciliation_object(
        &self,
        lease: &ReconcileLease,
        role: &str,
        binding: &ReconcileObject,
    ) -> Result<()> {
        if !identifier(role)
            || ![
                &binding.backend,
                &binding.name,
                &binding.uid,
                &binding.scope_uid,
            ]
            .into_iter()
            .all(|s| identifier(s))
        {
            return Err(Error::InvalidReconcileResult);
        }
        let org = &lease.task.organization;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let row = state::lease_row(&mut tx, lease).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        if !row.try_get::<bool, _>("dispatch_started")? {
            return Err(Error::InvalidReconcileResult);
        }
        let value = serde_json::to_value(binding).map_err(|_| Error::InvalidReconcileResult)?;
        let previous:Option<serde_json::Value>=sqlx::query_scalar("SELECT binding FROM reconciliation_objects WHERE organization=$1 AND step_id=$2 AND role=$3")
            .bind(org).bind(&lease.task.step_id).bind(role).fetch_optional(&mut *tx).await?;
        if let Some(previous) = previous {
            if previous != value {
                return Err(Error::IdempotencyConflict);
            }
        } else {
            let event = plans::transactions::emit(
                &mut tx,
                org,
                seq,
                "reconciliation.object_recorded",
                serde_json::json!({"step_id":lease.task.step_id,"role":role,"binding":binding}),
            )
            .await?;
            sqlx::query("INSERT INTO reconciliation_objects (organization,step_id,role,binding,event_sequence) VALUES ($1,$2,$3,$4,$5)")
                .bind(org).bind(&lease.task.step_id).bind(role).bind(value).bind(event).execute(&mut *tx).await?;
        }
        state::lease_row(&mut tx, lease).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        state::check_deadline(&mut tx, row.try_get("lease_until_ms")?).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn reconciliation_object(
        &self,
        lease: &ReconcileLease,
        role: &str,
    ) -> Result<Option<ReconcileObject>> {
        if !identifier(role) {
            return Err(Error::InvalidReconcileResult);
        }
        let org = &lease.task.organization;
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org).await?;
        let row = state::lease_row(&mut tx, lease).await?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        let value:Option<serde_json::Value>=sqlx::query_scalar("SELECT binding FROM reconciliation_objects WHERE organization=$1 AND step_id=$2 AND role=$3")
            .bind(org).bind(&lease.task.step_id).bind(role).fetch_optional(&mut *tx).await?;
        let binding = value
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| Error::InvalidStoredData)?;
        state::authorize(&mut tx, org, &lease.task.operation_id).await?;
        state::check_deadline(&mut tx, row.try_get("lease_until_ms")?).await?;
        tx.commit().await?;
        Ok(binding)
    }
}
