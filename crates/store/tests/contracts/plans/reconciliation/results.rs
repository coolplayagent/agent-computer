use super::*;

#[tokio::test]
async fn uncertain_effects_survive_block_resume_and_cannot_be_abandoned() {
    let (db, token, document) = fixture().await;
    let (_, operation) = publish(&db, &token, "first", &document).await;
    let lease = claim(&db, "one", 30).await;
    db.store
        .begin_reconciliation_dispatch(&lease)
        .await
        .unwrap();
    db.store
        .finish_reconciliation(
            &lease,
            ReconcileOutcome::Blocked {
                reason: ReconcileReason::RuntimeUnknown,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .abandon_reconciliation(&org("acme"), &operation.operation_id)
            .await,
        Err(Error::InvalidReconcileResult)
    ));
    db.store
        .resume_reconciliation(&org("acme"), &operation.operation_id)
        .await
        .unwrap();
    let resumed = claim(&db, "two", 30).await;
    assert_eq!(resumed.mode(), ClaimMode::Observe);
    assert!(matches!(
        db.store.begin_reconciliation_dispatch(&resumed).await,
        Err(Error::DispatchAlreadyStarted)
    ));
}

#[tokio::test]
async fn completion_final_write_failure_rolls_back_progress_events_and_outbox() {
    let (db, token, document) = fixture().await;
    let (_, operation) = publish(&db, &token, "first", &document).await;
    let lease = claim(&db, "one", 30).await;
    db.store
        .begin_reconciliation_dispatch(&lease)
        .await
        .unwrap();
    let before = db
        .store
        .definition_operation(&token, &operation.operation_id)
        .await
        .unwrap();
    let events = count(&db, "events").await;
    let outbox = count(&db, "outbox").await;
    sqlx::raw_sql("CREATE FUNCTION fail_result() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$; CREATE TRIGGER fail_result BEFORE INSERT ON reconciliation_results FOR EACH ROW EXECUTE FUNCTION fail_result();").execute(&db.pool).await.unwrap();
    let outcome = ReconcileOutcome::Applied {
        receipt: receipt(&lease),
    };
    assert!(
        db.store
            .finish_reconciliation(&lease, outcome.clone())
            .await
            .is_err()
    );
    let after = db
        .store
        .definition_operation(&token, &operation.operation_id)
        .await
        .unwrap();
    assert_eq!(before.progress, after.progress);
    assert_eq!(count(&db, "events").await, events);
    assert_eq!(count(&db, "outbox").await, outbox);
    assert_eq!(count(&db, "reconciliation_results").await, 0);
    sqlx::query("DROP TRIGGER fail_result ON reconciliation_results")
        .execute(&db.pool)
        .await
        .unwrap();
    let result = db
        .store
        .finish_reconciliation(&lease, outcome.clone())
        .await
        .unwrap();
    assert_eq!(
        db.store
            .finish_reconciliation(&lease, outcome)
            .await
            .unwrap(),
        result
    );
}
