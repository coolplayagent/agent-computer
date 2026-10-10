//! Read-only discovery for trusted preparation workers; IDs confer no authority.
use super::*;

impl Store {
    /// List at most the platform's 64 active starts for one organization/Volume.
    /// Every job must separately obtain the original atomic preparation claim.
    /// Existing bindings must match in full. Expired dispatched work is eligible
    /// for observation; undispatched work never gets a new queue deadline.
    pub async fn candidate_preparation_queue(
        &self,
        org: &OrganizationId,
        target: &PreparationTarget,
    ) -> Result<Vec<String>> {
        target.validate()?;
        let binding = serde_json::to_value(target).map_err(|_| Error::InvalidRuntimeRequest)?;
        Ok(sqlx::query_scalar(
            "SELECT r.request_id FROM runtime_start_requests r
             JOIN runtime_controls c ON c.organization=r.organization AND c.computer_id=r.computer_id AND c.active_request=r.request_id AND c.generation=r.generation
             JOIN runtime_start_inputs i ON i.organization=r.organization AND i.request_id=r.request_id AND i.workspace_id=r.workspace_id
             LEFT JOIN candidate_preparations p ON p.organization=r.organization AND p.request_id=r.request_id
             WHERE r.organization=$1 AND r.volume_id=$2 AND r.state IN ('Queued','Preparing')
               AND p.receipt IS NULL AND (p.binding IS NULL OR p.binding=$3)
               AND (p.lease_until_ms IS NULL OR p.lease_until_ms<=floor(extract(epoch from clock_timestamp())*1000)::bigint)
               AND ((r.state='Queued' AND r.queue_deadline_at_ms>floor(extract(epoch from clock_timestamp())*1000)::bigint)
                 OR (r.state='Preparing' AND p.dispatch_started))
             ORDER BY r.request_id LIMIT 64",
        )
        .bind(org.as_str())
        .bind(&target.volume_id)
        .bind(binding)
        .fetch_all(&self.pool)
        .await?)
    }
}
