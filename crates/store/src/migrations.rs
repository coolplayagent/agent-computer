use sqlx::migrate::{MigrateError, Migration, MigrationSource, MigrationType, Migrator};
use std::{borrow::Cow, future::Future, pin::Pin};

#[derive(Debug)]
struct Embedded;

type Resolved<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Migration>, sqlx::error::BoxDynError>> + Send + 'a>>;

impl<'s> MigrationSource<'s> for Embedded {
    fn resolve(self) -> Resolved<'s> {
        Box::pin(async {
            Ok(vec![Migration::new(
                1,
                Cow::Borrowed("declaration registry"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0001_declaration_registry.sql")),
                false,
            )])
        })
    }
}

pub(crate) async fn run(pool: &sqlx::PgPool) -> Result<(), MigrateError> {
    // SQLx verifies migration checksums and holds a PostgreSQL advisory lock.
    // Embedded SQL is identical under Cargo and Bazel, with no runtime file lookup.
    Migrator::new(Embedded).await?.run(pool).await
}
