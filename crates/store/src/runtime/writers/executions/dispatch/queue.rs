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
    /// Read-only recovery hints for the original node and exact Volume. The
    /// cursor advances past missing local receipts so they cannot starve later
    /// work. Recovery still validates the immutable arm and deadline in its
    /// publication transaction; discovery itself grants no execution authority.
    pub async fn candidate_execution_completion_queue(
        &self,
        org: &OrganizationId,
        target: &PreparationTarget,
        node: &agent_computer_kubernetes::NodeIdentity,
        after: Option<&str>,
    ) -> Result<Vec<String>> {
        target.validate()?;
        if let Some(id) = after {
            valid_id(id)?;
        }
        let binding = serde_json::to_value(target).map_err(|_| Error::InvalidRuntimeRequest)?;
        Ok(sqlx::query_scalar("SELECT r.execution_id FROM execution_requests r JOIN execution_dispatch_intents d USING(organization,execution_id) JOIN execution_watchdog_arms w USING(organization,execution_id) LEFT JOIN execution_completions c USING(organization,execution_id) WHERE r.organization=$1 AND r.binding->'storage_target'=$2 AND w.node_uid=$3 AND w.evidence#>>'{runtime,identity,node,name}'=$4 AND r.state IN ('Dispatching','CancelRequested','Unknown') AND c.execution_id IS NULL AND ($5::text IS NULL OR r.execution_id>$5) AND GREATEST(d.deadline_at_ms,COALESCE((SELECT MAX(g.deadline_at_ms) FROM execution_renewal_grants g WHERE g.organization=r.organization AND g.execution_id=r.execution_id),0))<=floor(extract(epoch from clock_timestamp())*1000)::bigint ORDER BY r.execution_id LIMIT 64")
            .bind(org.as_str()).bind(binding).bind(&node.uid).bind(&node.name).bind(after).fetch_all(&self.pool).await?)
    }

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
