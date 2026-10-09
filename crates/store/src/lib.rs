//! PostgreSQL declaration registry. This internal API assumes an authorized caller;
//! recording a declaration does not apply resources or dispatch runtime work.
#![forbid(unsafe_code)]

mod migrations;
mod reads;
mod retention;
mod types;
mod writes;

use sqlx::{PgPool, Postgres, Row, Transaction};
pub use types::*;

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Pool creation, TLS, credentials and timeouts belong to the trusted service.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn migrate(&self) -> Result<()> {
        migrations::run(&self.pool).await?;
        Ok(())
    }

    async fn lock_stream(tx: &mut Transaction<'_, Postgres>, org: &str) -> Result<i64> {
        sqlx::query(
            "INSERT INTO organization_streams (organization) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(org)
        .execute(&mut **tx)
        .await?;
        Ok(sqlx::query(
            "SELECT last_sequence FROM organization_streams WHERE organization = $1 FOR UPDATE",
        )
        .bind(org)
        .fetch_one(&mut **tx)
        .await?
        .try_get("last_sequence")?)
    }

    async fn read_transaction(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
}
