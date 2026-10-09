use crate::{Error, Result, Store, writes::OPERATION};
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};

impl Store {
    /// Keep a permanent key tombstone, even when the response is removed.
    pub async fn retire_key(
        &self,
        org: &OrganizationId,
        principal: &PrincipalId,
        key: &IdempotencyKey,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        Self::lock_stream(&mut tx, org.as_str()).await?;
        let rows = sqlx::query("UPDATE request_records SET retired=TRUE,response=NULL WHERE organization=$1 AND principal=$2 AND operation=$3 AND request_key=$4")
            .bind(org.as_str()).bind(principal.as_str()).bind(OPERATION).bind(key.as_str())
            .execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(rows == 1)
    }

    pub async fn acknowledge(&self, org: &OrganizationId, sequence: i64) -> Result<bool> {
        Ok(
            sqlx::query(
                "UPDATE outbox SET acknowledged=TRUE WHERE organization=$1 AND sequence=$2",
            )
            .bind(org.as_str())
            .bind(sequence)
            .execute(&self.pool)
            .await?
            .rows_affected()
                == 1,
        )
    }

    /// Explicit retention boundary, never inferred from a client clock. Refuse to
    /// delete an event while it is still pending external delivery.
    pub async fn prune_events(&self, org: &OrganizationId, through: i64) -> Result<()> {
        if through < 0 {
            return Err(Error::InvalidCursor);
        }
        let mut tx = self.pool.begin().await?;
        let last = Self::lock_stream(&mut tx, org.as_str()).await?;
        let floor: i64 = sqlx::query_scalar(
            "SELECT replay_floor FROM organization_streams WHERE organization=$1",
        )
        .bind(org.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if through < floor || through > last {
            return Err(Error::InvalidCursor);
        }
        let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM outbox WHERE organization=$1 AND sequence<=$2 AND NOT acknowledged)")
            .bind(org.as_str()).bind(through).fetch_one(&mut *tx).await?;
        if pending {
            return Err(Error::UnpublishedEvents);
        }
        sqlx::query("DELETE FROM outbox WHERE organization=$1 AND sequence<=$2")
            .bind(org.as_str())
            .bind(through)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM events WHERE organization=$1 AND sequence<=$2")
            .bind(org.as_str())
            .bind(through)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE organization_streams SET replay_floor=$2 WHERE organization=$1")
            .bind(org.as_str())
            .bind(through)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
