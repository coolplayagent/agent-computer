use super::*;

#[tokio::test]
async fn lease_expiry_and_database_restart_preserve_unknown_effects_and_reject_old_workers() {
    let (mut db, token, document) = fixture().await;
    publish(&db, &token, "initial", &document).await;
    let old = claim(&db, "old", 1).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(matches!(
        db.store
            .renew_reconciliation(&old, Duration::from_secs(30))
            .await,
        Err(Error::StaleReconcileLease)
    ));
    assert!(matches!(
        db.store.begin_reconciliation_dispatch(&old).await,
        Err(Error::StaleReconcileLease)
    ));
    let active = claim(&db, "active", 1).await;
    assert_eq!(active.epoch(), 2);
    assert_eq!(active.mode(), ClaimMode::Execute);
    assert_eq!(active.task().step_id, old.task().step_id);
    db.store
        .begin_reconciliation_dispatch(&active)
        .await
        .unwrap();
    assert!(matches!(
        db.store.begin_reconciliation_dispatch(&active).await,
        Err(Error::DispatchAlreadyStarted)
    ));
    db.crash_and_restart().await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let recovered = claim(&db, "replacement", 30).await;
    assert_eq!(recovered.mode(), ClaimMode::Observe);
    assert_eq!(recovered.epoch(), 3);
    assert_eq!(recovered.task().step_id, active.task().step_id);
    assert!(matches!(
        db.store.begin_reconciliation_dispatch(&recovered).await,
        Err(Error::DispatchAlreadyStarted)
    ));
    assert!(matches!(
        db.store
            .finish_reconciliation(
                &active,
                ReconcileOutcome::Applied {
                    receipt: receipt(&active)
                }
            )
            .await,
        Err(Error::StaleReconcileLease)
    ));
    db.store
        .finish_reconciliation(
            &recovered,
            ReconcileOutcome::Applied {
                receipt: receipt(&recovered),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn retries_are_durable_and_do_not_erase_dispatch_uncertainty() {
    let (db, token, document) = fixture().await;
    publish(&db, &token, "initial", &document).await;
    let lease = claim(&db, "one", 30).await;
    db.store
        .begin_reconciliation_dispatch(&lease)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .finish_reconciliation(
                &lease,
                ReconcileOutcome::Failed {
                    reason: ReconcileReason::BackendRejected
                }
            )
            .await,
        Err(Error::InvalidReconcileResult)
    ));
    let result = ReconcileOutcome::Retry {
        reason: ReconcileReason::BackendTransient,
        delay_seconds: 1,
    };
    let pending = db
        .store
        .finish_reconciliation(&lease, result.clone())
        .await
        .unwrap();
    assert_eq!(pending.state, IntentState::Pending);
    assert!(pending.dispatch_started);
    idle(&db).await;
    assert_eq!(
        db.store
            .finish_reconciliation(&lease, result.clone())
            .await
            .unwrap(),
        pending
    );
    assert!(matches!(
        db.store
            .finish_reconciliation(
                &lease,
                ReconcileOutcome::Blocked {
                    reason: ReconcileReason::RuntimeUnknown
                }
            )
            .await,
        Err(Error::IdempotencyConflict)
    ));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let next = claim(&db, "two", 30).await;
    assert_eq!(next.mode(), ClaimMode::Observe);
    // An old exact completion receipt is readable but cannot change the new lease.
    assert_eq!(
        db.store
            .finish_reconciliation(&lease, result)
            .await
            .unwrap(),
        pending
    );
    assert_eq!(
        db.store
            .definition_operation(&token, &next.task().operation_id)
            .await
            .unwrap()
            .progress[0]
            .attempts,
        2
    );
    let mut forged = receipt(&next);
    forged.spec_digest = format!("sha256:{}", "0".repeat(64));
    assert!(matches!(
        db.store
            .finish_reconciliation(&next, ReconcileOutcome::Applied { receipt: forged })
            .await,
        Err(Error::InvalidReconcileResult)
    ));
    db.store
        .finish_reconciliation(
            &next,
            ReconcileOutcome::Applied {
                receipt: receipt(&next),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn completion_that_outlives_its_lease_rolls_back_at_commit_boundary() {
    let (db, token, document) = fixture().await;
    let (_, operation) = publish(&db, &token, "first", &document).await;
    let lease = claim(&db, "slow", 1).await;
    db.store
        .begin_reconciliation_dispatch(&lease)
        .await
        .unwrap();
    let before = db
        .store
        .inspect_reconciliation(&org("acme"), &operation.operation_id)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION slow_result() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(1.1); RETURN NEW; END $$; CREATE TRIGGER slow_result BEFORE INSERT ON reconciliation_results FOR EACH ROW EXECUTE FUNCTION slow_result();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .finish_reconciliation(
                &lease,
                ReconcileOutcome::Applied {
                    receipt: receipt(&lease)
                }
            )
            .await,
        Err(Error::StaleReconcileLease)
    ));
    let after = db
        .store
        .inspect_reconciliation(&org("acme"), &operation.operation_id)
        .await
        .unwrap();
    assert_eq!(before.progress, after.progress);
    assert_eq!(before.watermark, after.watermark);
    assert_eq!(count(&db, "reconciliation_results").await, 0);
    let next = claim(&db, "replacement", 30).await;
    assert_eq!(next.mode(), ClaimMode::Observe);
}
