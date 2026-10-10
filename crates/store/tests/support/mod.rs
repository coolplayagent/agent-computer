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
    pub async fn remove_execution_queue_poll(&self) {
        sqlx::raw_sql(
            "DROP INDEX execution_queue_poll; DELETE FROM _sqlx_migrations WHERE version=24;",
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }
    pub async fn remove_execution_completions(&self) {
        self.remove_execution_queue_poll().await;
        sqlx::raw_sql("DROP TABLE execution_completions; DROP FUNCTION guard_execution_completion(); DROP FUNCTION complete_execution_completion(); DELETE FROM _sqlx_migrations WHERE version=23; ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_state_check; ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_reason_check; ALTER TABLE execution_requests ADD CHECK (state IN ('Queued','Cancelled','Dispatching','CancelRequested','Unknown')); ALTER TABLE execution_requests ADD CHECK (reason IN ('awaiting_runtime_dispatch','user_requested','writer_unavailable','dispatch_committed','dispatch_unconfirmed')); ALTER TABLE candidate_writer_drains DROP CONSTRAINT candidate_writer_drains_proof_check; ALTER TABLE candidate_writer_drains ADD CHECK (proof IN ('no_dispatch','bounded_file_drained'));").execute(&self.pool).await.unwrap();
        for (source, name) in [
            (
                include_str!("../../migrations/0011_candidate_file_completions.sql"),
                "guard_writer_record_insert",
            ),
            (
                include_str!("../../migrations/0013_execution_dispatch.sql"),
                "guard_execution_request",
            ),
            (
                include_str!("../../migrations/0013_execution_dispatch.sql"),
                "guard_execution_writer_slot",
            ),
            (
                include_str!("../../migrations/0021_workspace_artifacts.sql"),
                "artifact_candidate_drained",
            ),
        ] {
            let body = source
                .split(&format!("FUNCTION {name}"))
                .nth(1)
                .unwrap()
                .split("$$;")
                .next()
                .unwrap();
            sqlx::raw_sql(&format!("CREATE OR REPLACE FUNCTION {name}{body}$$;"))
                .execute(&self.pool)
                .await
                .unwrap();
        }
    }
    pub async fn remove_artifact_continuation(&self) {
        self.remove_execution_completions().await;
        sqlx::raw_sql("DROP TRIGGER check_runtime_start_input ON runtime_start_inputs; DROP FUNCTION guard_runtime_start_input(); DROP INDEX runtime_workspace_candidates; CREATE UNIQUE INDEX runtime_one_active_workspace ON runtime_start_requests (organization,workspace_id) WHERE state NOT IN ('Cancelled','Stopped'); DELETE FROM _sqlx_migrations WHERE version=22;").execute(&self.pool).await.unwrap();
        let previous = include_str!("../../migrations/0021_workspace_artifacts.sql")
            .split("CREATE FUNCTION runtime_stop_checkpoint")
            .nth(1)
            .unwrap()
            .split("$$;")
            .next()
            .unwrap();
        sqlx::raw_sql(&format!(
            "CREATE OR REPLACE FUNCTION runtime_stop_checkpoint{previous}$$;"
        ))
        .execute(&self.pool)
        .await
        .unwrap();
    }
    pub async fn remove_workspace_artifacts(&self) {
        self.remove_artifact_continuation().await;
        sqlx::raw_sql("DROP FUNCTION runtime_stop_checkpoint(TEXT,TEXT); ALTER TABLE workspace_input_versions DROP COLUMN artifact_commit_id; DROP TABLE artifact_commits; DROP FUNCTION guard_artifact_commit(); DROP FUNCTION artifact_candidate_drained(TEXT,TEXT); DELETE FROM _sqlx_migrations WHERE version=21;").execute(&self.pool).await.unwrap();
    }
    pub async fn remove_undispatched_stop(&self) {
        self.remove_workspace_artifacts().await;
        sqlx::raw_sql("DROP TRIGGER check_writer_start ON candidate_writer_leases; DROP FUNCTION guard_writer_start_authority(); DROP TABLE runtime_stops; DROP FUNCTION runtime_stop_is_undispatched(TEXT,TEXT); DROP FUNCTION guard_runtime_stop_insert(); DELETE FROM _sqlx_migrations WHERE version=20;").execute(&self.pool).await.unwrap();
    }
    pub async fn remove_execution_outputs(&self) {
        self.remove_undispatched_stop().await;
        sqlx::raw_sql("DROP TABLE execution_outputs; DROP TABLE execution_output_intents; DROP FUNCTION guard_execution_output(); DELETE FROM _sqlx_migrations WHERE version=19;").execute(&self.pool).await.unwrap();
    }
    pub async fn remove_reaper_admission(&self) {
        self.remove_execution_outputs().await;
        sqlx::raw_sql("DROP TRIGGER check_reaper_admission ON execution_watchdog_arms; DROP FUNCTION guard_reaper_admission(); DROP TRIGGER check_reaper_startup ON execution_startup_grants; DROP FUNCTION guard_reaper_startup(); DROP INDEX execution_reaper_challenges; DELETE FROM _sqlx_migrations WHERE version=18;").execute(&self.pool).await.unwrap();
    }
    pub async fn remove_redundant_watchdogs(&self) {
        self.remove_reaper_admission().await;
        sqlx::raw_sql("DROP TRIGGER check_redundant_watchdog ON execution_watchdog_arms; DROP FUNCTION guard_redundant_watchdog(); DROP TRIGGER check_redundant_startup ON execution_startup_grants; DROP FUNCTION guard_redundant_startup(); DELETE FROM _sqlx_migrations WHERE version=17;").execute(&self.pool).await.unwrap();
    }
    pub async fn remove_execution_watchdogs(&self) {
        self.remove_redundant_watchdogs().await;
        sqlx::raw_sql("DROP TRIGGER check_execution_startup_watchdog ON execution_startup_grants; DROP FUNCTION guard_execution_startup_watchdog(); DROP TABLE execution_watchdog_arms; DROP FUNCTION guard_execution_watchdog(); DELETE FROM _sqlx_migrations WHERE version=16;").execute(&self.pool).await.unwrap();
    }
    pub async fn remove_execution_pods(&self) {
        self.remove_execution_watchdogs().await;
        sqlx::raw_sql("DROP TRIGGER check_execution_startup_pod ON execution_startup_grants; DROP FUNCTION guard_execution_startup_pod(); DROP TABLE execution_pod_observations; DROP TABLE execution_pod_plans; DROP FUNCTION guard_execution_pod_plan(); DELETE FROM _sqlx_migrations WHERE version=15;").execute(&self.pool).await.unwrap();
    }
    pub async fn remove_execution_startup(&self) {
        self.remove_execution_pods().await;
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
