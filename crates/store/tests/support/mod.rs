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
