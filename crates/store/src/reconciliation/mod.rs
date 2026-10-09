//! Durable coordination for trusted runtime workers. No external backend is
//! invoked here, and a coordinator lease never proves physical fencing.
mod claims;
mod completion;
mod objects;
mod state;
mod types;
pub(crate) use state::progress;
pub use types::*;

impl crate::Store {
    /// Trusted local diagnostics. HTTP uses the separately authorized operation
    /// query and never exposes this database-administrator surface.
    pub async fn inspect_reconciliation(
        &self,
        org: &agent_computer_core::identity::OrganizationId,
        operation: &str,
    ) -> crate::Result<ReconciliationStatus> {
        let mut tx = self.read_transaction().await?;
        let status: Option<String> = sqlx::query_scalar(
            "SELECT state FROM operations WHERE organization=$1 AND operation_id=$2",
        )
        .bind(org.as_str())
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await?;
        let state = status.ok_or(crate::Error::PlanNotFound)?;
        let watermark = sqlx::query_scalar(
            "SELECT last_sequence FROM organization_streams WHERE organization=$1",
        )
        .bind(org.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let progress = state::progress(&mut tx, org.as_str(), operation).await?;
        tx.commit().await?;
        Ok(ReconciliationStatus {
            operation_id: operation.into(),
            state,
            watermark,
            progress,
        })
    }
}
