use sqlx::migrate::{MigrateError, Migration, MigrationSource, MigrationType, Migrator};
use std::{borrow::Cow, future::Future, pin::Pin};

#[derive(Debug)]
struct Embedded;

type Resolved<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Migration>, sqlx::error::BoxDynError>> + Send + 'a>>;

impl<'s> MigrationSource<'s> for Embedded {
    fn resolve(self) -> Resolved<'s> {
        Box::pin(async {
            Ok(vec![
                Migration::new(
                    1,
                    Cow::Borrowed("declaration registry"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0001_declaration_registry.sql")),
                    false,
                ),
                Migration::new(
                    2,
                    Cow::Borrowed("service credentials"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0002_service_credentials.sql")),
                    false,
                ),
                Migration::new(
                    3,
                    Cow::Borrowed("definition plans"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0003_definition_plans.sql")),
                    false,
                ),
                Migration::new(
                    4,
                    Cow::Borrowed("reconciliation leases"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0004_reconciliation_leases.sql")),
                    false,
                ),
                Migration::new(
                    5,
                    Cow::Borrowed("reconciliation object identities"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!(
                        "../migrations/0005_reconciliation_objects.sql"
                    )),
                    false,
                ),
                Migration::new(
                    6,
                    Cow::Borrowed("runtime grants"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0006_runtime_grants.sql")),
                    false,
                ),
                Migration::new(
                    7,
                    Cow::Borrowed("runtime start admission"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!(
                        "../migrations/0007_runtime_start_admission.sql"
                    )),
                    false,
                ),
                Migration::new(
                    8,
                    Cow::Borrowed("candidate preparation"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0008_candidate_preparation.sql")),
                    false,
                ),
                Migration::new(
                    9,
                    Cow::Borrowed("connection sessions"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0009_connection_sessions.sql")),
                    false,
                ),
                Migration::new(
                    10,
                    Cow::Borrowed("candidate writer leases"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!(
                        "../migrations/0010_candidate_writer_leases.sql"
                    )),
                    false,
                ),
                Migration::new(
                    11,
                    Cow::Borrowed("candidate file completions"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!(
                        "../migrations/0011_candidate_file_completions.sql"
                    )),
                    false,
                ),
                Migration::new(
                    12,
                    Cow::Borrowed("execution admission"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0012_execution_admission.sql")),
                    false,
                ),
                Migration::new(
                    13,
                    Cow::Borrowed("execution dispatch"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0013_execution_dispatch.sql")),
                    false,
                ),
                Migration::new(
                    14,
                    Cow::Borrowed("execution startup"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0014_execution_startup.sql")),
                    false,
                ),
                Migration::new(
                    15,
                    Cow::Borrowed("execution Pod identities"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0015_execution_pods.sql")),
                    false,
                ),
                Migration::new(
                    16,
                    Cow::Borrowed("execution node watchdogs"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0016_execution_watchdogs.sql")),
                    false,
                ),
                Migration::new(
                    17,
                    Cow::Borrowed("redundant execution watchdogs"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0017_redundant_watchdogs.sql")),
                    false,
                ),
                Migration::new(
                    18,
                    Cow::Borrowed("execution reaper admission"),
                    MigrationType::Simple,
                    Cow::Borrowed(include_str!("../migrations/0018_reaper_admission.sql")),
                    false,
                ),
            ])
        })
    }
}

pub(crate) async fn ready(pool: &sqlx::PgPool) -> crate::Result<()> {
    use sqlx::Row;
    let migrator = Migrator::new(Embedded).await?;
    let applied =
        sqlx::query("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?;
    if applied.len() != migrator.iter().count() {
        return Err(crate::Error::SchemaNotReady);
    }
    for (row, expected) in applied.into_iter().zip(migrator.iter()) {
        if row.try_get::<i64, _>("version")? != expected.version
            || row.try_get::<Vec<u8>, _>("checksum")? != expected.checksum.as_ref()
            || !row.try_get::<bool, _>("success")?
        {
            return Err(crate::Error::SchemaNotReady);
        }
    }
    Ok(())
}

pub(crate) async fn run(pool: &sqlx::PgPool) -> Result<(), MigrateError> {
    // SQLx verifies migration checksums and holds a PostgreSQL advisory lock.
    // Embedded SQL is identical under Cargo and Bazel, with no runtime file lookup.
    Migrator::new(Embedded).await?.run(pool).await
}
