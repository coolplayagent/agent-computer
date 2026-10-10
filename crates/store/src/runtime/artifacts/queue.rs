use super::*;

impl Store {
    /// Read-only hints. Every publication still requires its own current claim.
    pub async fn artifact_queue(
        &self,
        org: &OrganizationId,
        target: &super::super::preparation::PreparationTarget,
    ) -> Result<Vec<String>> {
        target.validate()?;
        let binding = serde_json::to_value(target).map_err(|_| Error::InvalidRuntimeRequest)?;
        Ok(sqlx::query_scalar("SELECT a.commit_id FROM artifact_commits a JOIN candidate_preparations p USING(organization,request_id) JOIN runtime_start_requests r USING(organization,request_id) JOIN runtime_controls c ON c.organization=r.organization AND c.computer_id=r.computer_id AND c.active_request=r.request_id AND c.generation=r.generation WHERE a.organization=$1 AND ((a.state='Capturing' AND r.state='Sealing') OR (a.state='Draining' AND r.state='Draining')) AND p.binding=$2 AND (a.lease_until_ms IS NULL OR a.lease_until_ms<=floor(extract(epoch from clock_timestamp())*1000)::bigint) ORDER BY a.commit_id LIMIT 64")
            .bind(org.as_str()).bind(binding).fetch_all(&self.pool).await?)
    }
}
