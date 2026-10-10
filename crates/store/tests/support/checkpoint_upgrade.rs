use super::Database;

impl Database {
    pub async fn remove_background_execution(&self) {
        sqlx::raw_sql("DROP FUNCTION writer_has_background_execution(TEXT,TEXT,BIGINT); ALTER TABLE execution_requests DROP CONSTRAINT execution_lifetime; DELETE FROM _sqlx_migrations WHERE version=28;")
            .execute(&self.pool).await.unwrap();
    }
    /// Restore the exact migration-26 functions and checks for upgrade fixtures.
    pub async fn remove_checkpoint_stop_drain(&self) {
        self.remove_background_execution().await;
        sqlx::raw_sql("DROP TRIGGER checkpoint_drain_consistency ON artifact_commits; DROP FUNCTION verify_checkpoint_drain(); DROP FUNCTION checkpoint_stop_eligible(TEXT,TEXT,TEXT); DROP INDEX artifact_worker_queue; ALTER TABLE artifact_commits DROP CONSTRAINT artifact_pending_timestamp, DROP CONSTRAINT artifact_pending_object, DROP CONSTRAINT artifact_pending_input, DROP CONSTRAINT artifact_drain_pending, DROP CONSTRAINT artifact_commits_state_check, DROP COLUMN cancel_running; ALTER TABLE artifact_commits ADD CHECK (state IN ('Capturing','Committed','Conflict')), ADD CHECK ((state='Capturing')=(published_at_ms IS NULL)), ADD CHECK ((state='Capturing')=(object_ref IS NULL)), ADD CHECK ((state='Capturing')=(input_revision IS NULL)); CREATE INDEX artifact_worker_queue ON artifact_commits (organization,commit_id) WHERE state='Capturing'; ALTER TABLE runtime_start_requests DROP CONSTRAINT runtime_start_requests_state_check; ALTER TABLE runtime_start_requests ADD CHECK (state IN ('Queued','Preparing','Prepared','Sealing','Sealed','Cancelled','Stopped')); DELETE FROM _sqlx_migrations WHERE version=27;")
            .execute(&self.pool).await.unwrap();
        for (source, name) in [
            (
                include_str!("../../migrations/0021_workspace_artifacts.sql"),
                "guard_artifact_commit",
            ),
            (
                include_str!("../../migrations/0021_workspace_artifacts.sql"),
                "guard_runtime_start_mutation",
            ),
            (
                include_str!("../../migrations/0026_checkpoint_stop_worker.sql"),
                "guard_checkpoint_stop_admission",
            ),
            (
                include_str!("../../migrations/0026_checkpoint_stop_worker.sql"),
                "verify_checkpoint_stop_completion",
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
}
