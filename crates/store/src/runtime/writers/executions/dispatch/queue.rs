//! One atomic claim from a bounded organization/Volume execution queue.
use super::*;
use crate::runtime::preparation::PreparationTarget;

#[derive(Debug)]
pub enum QueuedDispatch {
    Idle,
    Cancelled(Box<ExecutionRequest>),
    Claimed(Box<ExecutionDispatchAttempt>),
}
impl Store {
    /// Trusted node worker: select the oldest queued request for this exact
    /// storage identity, then spend the original admission in the same transaction.
    /// Concurrent workers share the existing organization stream lock. Claimed,
    /// Unknown and completed executions can never be selected after a restart.
    pub async fn claim_queued_candidate_execution(
        &self,
        org: &OrganizationId,
        target: &PreparationTarget,
    ) -> Result<QueuedDispatch> {
        target.validate()?;
        let local_start = Instant::now();
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let binding = serde_json::to_value(target).map_err(|_| Error::InvalidRuntimeRequest)?;
        let record = sqlx::query("SELECT * FROM execution_requests WHERE organization=$1 AND state='Queued' AND binding->'storage_target'=$2 ORDER BY created_at_ms,execution_id LIMIT 1")
            .bind(org.as_str()).bind(binding).fetch_optional(&mut *tx).await?;
        let Some(record) = record else {
            tx.commit().await?;
            return Ok(QueuedDispatch::Idle);
        };
        let id: String = record.try_get("execution_id")?;
        begin_queued(tx, org, &id, record, seq, local_start).await
    }
}
