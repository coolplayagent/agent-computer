use crate::support::*;
use agent_computer_store::{Error, Precondition};

#[tokio::test]
async fn failure_at_final_write_rolls_back_every_effect_and_key_reservation() {
    let db = Database::new().await;
    sqlx::raw_sql("CREATE FUNCTION fail_request() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected last-write failure'; END; $$; CREATE TRIGGER fail_request BEFORE INSERT ON request_records FOR EACH ROW EXECUTE FUNCTION fail_request();").execute(&db.pool).await.unwrap();
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
        Err(Error::Database(_))
    ));
    // Read with a new connection after the failed transaction has rolled back.
    for table in [
        "organization_streams",
        "declaration_heads",
        "declaration_versions",
        "events",
        "outbox",
        "request_records",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "partial commit in {table}");
    }
    sqlx::query("DROP TRIGGER fail_request ON request_records")
        .execute(&db.pool)
        .await
        .unwrap();
    let receipt = record(
        &db.store,
        "acme",
        "alice",
        "key",
        "research",
        Precondition::Create,
    )
    .await
    .unwrap();
    assert_eq!((receipt.revision, receipt.event_sequence), (1, 1));
}
