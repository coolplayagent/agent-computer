use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, ValidatedDefinition, validate_bytes};
use agent_computer_store::{Precondition, Receipt, RecordDeclaration, Store};
use agent_computer_test_support::Postgres;
use sqlx::PgPool;

pub struct Database {
    pub store: Store,
    pub pool: PgPool,
    postgres: Postgres,
}
impl Database {
    pub async fn remove_execution_startup(&self) {
        sqlx::raw_sql("DROP TABLE execution_startup_grants; DROP FUNCTION guard_execution_startup(); DELETE FROM _sqlx_migrations WHERE version=14;").execute(&self.pool).await.unwrap();
    }
    /// Restore migration 12 when constructing historical upgrade fixtures.
    pub async fn remove_execution_dispatch(&self) {
        self.remove_execution_startup().await;
        sqlx::raw_sql("DROP TABLE execution_dispatch_intents; DROP FUNCTION guard_execution_dispatch(); DROP FUNCTION complete_execution_dispatch(); DROP TRIGGER check_execution_file_completion ON candidate_writer_completions; DELETE FROM _sqlx_migrations WHERE version=13;").execute(&self.pool).await.unwrap();
        sqlx::raw_sql("ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_state_check; ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_reason_check; ALTER TABLE execution_requests ADD CHECK (state IN ('Queued','Cancelled')); ALTER TABLE execution_requests ADD CHECK (reason IN ('awaiting_runtime_dispatch','user_requested','writer_unavailable'));").execute(&self.pool).await.unwrap();
        for function in include_str!("../../migrations/0012_execution_admission.sql")
            .split("CREATE FUNCTION ")
            .skip(1)
        {
            let body = function.split("$$;").next().unwrap();
            sqlx::raw_sql(&format!("CREATE OR REPLACE FUNCTION {body}$$;"))
                .execute(&self.pool)
                .await
                .unwrap();
        }
    }
    pub async fn remove_execution_admission(&self) {
        self.remove_execution_dispatch().await;
        sqlx::raw_sql("DROP TABLE execution_requests; DROP FUNCTION guard_execution_request(); DROP TRIGGER check_execution_writer_dispatch ON candidate_writer_dispatches; DROP TRIGGER check_execution_writer_drain ON candidate_writer_drains; DROP FUNCTION guard_execution_writer_slot(); DELETE FROM _sqlx_migrations WHERE version=12;").execute(&self.pool).await.unwrap();
    }
    pub async fn new() -> Self {
        let postgres = Postgres::new().await;
        let pool = postgres.pool.clone();
        let store = Store::new(pool.clone());
        let (one, two) = tokio::join!(store.migrate(), store.migrate());
        one.unwrap();
        two.unwrap();
        Self {
            store,
            pool,
            postgres,
        }
    }
    pub async fn crash_and_restart(&mut self) {
        self.postgres.crash_and_restart().await;
        self.pool = self.postgres.pool.clone();
        self.store = Store::new(self.pool.clone());
    }
}

pub fn org(value: &str) -> OrganizationId {
    OrganizationId::new(value).unwrap()
}
pub fn principal(value: &str) -> PrincipalId {
    PrincipalId::new(value).unwrap()
}
pub fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).unwrap()
}

pub fn definition(name: &str, expected: Option<u64>) -> ValidatedDefinition {
    let mut document = serde_json::json!({"apiVersion":"agent-computer/v1alpha1", "kind":"ComputerSet", "metadata":{"name":name}, "spec":{"agents":[{"name":"external", "mode":"external", "adapter":"tools-api", "capabilities":[]}]}});
    if let Some(expected) = expected {
        document["metadata"]["expectedRevision"] = expected.into();
    }
    validate_bytes(&serde_json::to_vec(&document).unwrap(), Format::Json).unwrap()
}

pub async fn record(
    store: &Store,
    organization: &str,
    actor: &str,
    request_key: &str,
    name: &str,
    precondition: Precondition,
) -> agent_computer_store::Result<Receipt> {
    store
        .record(RecordDeclaration {
            organization: &org(organization),
            principal: &principal(actor),
            key: &key(request_key),
            precondition,
            definition: &definition(name, None),
        })
        .await
}
