use crate::support::*;
use agent_computer_core::identity::Revision;
use agent_computer_store::{Error, Precondition, RecordDeclaration};

#[tokio::test]
async fn immutable_history_and_retry_survive_wal_recovery() {
    let mut db = Database::new().await;
    let first = record(
        &db.store,
        "acme",
        "alice",
        "first",
        "research",
        Precondition::Create,
    )
    .await
    .unwrap();
    assert_eq!((first.revision, first.event_sequence), (1, 1));
    let original = db
        .store
        .version(&org("acme"), "research", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        original.canonical,
        definition("research", None).canonical_bytes()
    );
    let second = record(
        &db.store,
        "acme",
        "alice",
        "second",
        "research",
        Precondition::Match(Revision::INITIAL),
    )
    .await
    .unwrap();
    assert_eq!((second.revision, second.event_sequence), (2, 2));
    let settings: (String, String, String) = sqlx::query_as("SELECT current_setting('fsync'),current_setting('synchronous_commit'),current_setting('full_page_writes')").fetch_one(&db.pool).await.unwrap();
    assert_eq!(settings, ("on".into(), "on".into(), "on".into()));
    assert!(
        sqlx::query("UPDATE declaration_versions SET canonical='changed' WHERE revision=1")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM declaration_versions WHERE revision=1")
            .execute(&db.pool)
            .await
            .is_err()
    );
    db.crash_and_restart().await;
    db.store.migrate().await.unwrap();
    let retry = record(
        &db.store,
        "acme",
        "alice",
        "first",
        "research",
        Precondition::Create,
    )
    .await
    .unwrap();
    assert_eq!(retry, first); // The response can be recovered even after a later version.
    assert_eq!(
        db.store
            .version(&org("acme"), "research", 1)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    let snapshot = db.store.snapshot(&org("acme")).await.unwrap();
    assert_eq!(
        (snapshot.watermark, snapshot.declarations[0].revision),
        (2, 2)
    );
    assert_eq!(
        db.store
            .pending_outbox(&org("acme"), 100)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn idempotency_covers_intent_scope_and_retired_keys() {
    let db = Database::new().await;
    record(
        &db.store,
        "acme",
        "alice",
        "key",
        "research",
        Precondition::Create,
    )
    .await
    .unwrap();
    for (name, condition) in [
        ("other", Precondition::Create),
        ("research", Precondition::Match(Revision::INITIAL)),
    ] {
        assert!(matches!(
            record(&db.store, "acme", "alice", "key", name, condition).await,
            Err(Error::IdempotencyConflict)
        ));
    }
    let changed = definition("research", Some(1));
    let result = db
        .store
        .record(RecordDeclaration {
            organization: &org("acme"),
            principal: &principal("alice"),
            key: &key("key"),
            precondition: Precondition::Create,
            definition: &changed,
        })
        .await;
    assert!(matches!(result, Err(Error::IdempotencyConflict)));
    // Same key can be used by a different principal or organization.
    record(
        &db.store,
        "acme",
        "bob",
        "key",
        "other",
        Precondition::Create,
    )
    .await
    .unwrap();
    record(
        &db.store,
        "other",
        "alice",
        "key",
        "research",
        Precondition::Create,
    )
    .await
    .unwrap();
    assert!(
        db.store
            .retire_key(&org("acme"), &principal("alice"), &key("key"))
            .await
            .unwrap()
    );
    assert!(matches!(
        record(
            &db.store,
            "acme",
            "alice",
            "key",
            "research",
            Precondition::Create
        )
        .await,
        Err(Error::IdempotencyGone)
    ));
    assert!(matches!(
        record(
            &db.store,
            "acme",
            "alice",
            "key",
            "changed",
            Precondition::Create
        )
        .await,
        Err(Error::IdempotencyGone)
    ));
    assert!(
        db.store
            .snapshot(&org("absent"))
            .await
            .unwrap()
            .declarations
            .is_empty()
    );
    assert!(
        db.store
            .version(&org("other"), "other", 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.store
            .replay(&org("other"), 0, 100)
            .await
            .unwrap()
            .events
            .len(),
        1
    );
    for invalid in [0, u64::MAX] {
        assert!(matches!(
            record(
                &db.store,
                "acme",
                "alice",
                "invalid",
                "research",
                Precondition::Match(Revision::from_u64(invalid))
            )
            .await,
            Err(Error::InvalidPrecondition)
        ));
    }
}

#[tokio::test]
async fn migration_checksum_mismatch_is_rejected() {
    let db = Database::new().await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=1")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(db.store.migrate().await, Err(Error::Migration(_))));
}
