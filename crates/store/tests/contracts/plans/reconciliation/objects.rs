use super::*;

fn object() -> ReconcileObject {
    ReconcileObject {
        backend: "kubernetes_juicefs".into(),
        name: "acv-volume".into(),
        uid: "pvc-uid".into(),
        scope_uid: "namespace-uid".into(),
    }
}

#[tokio::test]
async fn object_identity_is_immutable_and_survives_worker_retry() {
    let (db, token, doc) = fixture().await;
    publish(&db, &token, "objects", &doc).await;
    let lease = claim(&db, "one", 30).await;
    assert!(matches!(
        db.store
            .record_reconciliation_object(&lease, "pvc", &object())
            .await,
        Err(Error::InvalidReconcileResult)
    ));
    db.store
        .begin_reconciliation_dispatch(&lease)
        .await
        .unwrap();
    let before = count(&db, "events").await;
    db.store
        .record_reconciliation_object(&lease, "pvc", &object())
        .await
        .unwrap();
    db.store
        .record_reconciliation_object(&lease, "pvc", &object())
        .await
        .unwrap();
    assert_eq!(count(&db, "events").await, before + 1);
    let mut replacement = object();
    replacement.uid = "new-uid".into();
    assert!(matches!(
        db.store
            .record_reconciliation_object(&lease, "pvc", &replacement)
            .await,
        Err(Error::IdempotencyConflict)
    ));
    db.store
        .finish_reconciliation(
            &lease,
            ReconcileOutcome::Retry {
                reason: ReconcileReason::BackendTransient,
                delay_seconds: 1,
            },
        )
        .await
        .unwrap();
    sqlx::query("UPDATE reconcile_intents SET available_at_ms=0 WHERE step_id=$1")
        .bind(&lease.task().step_id)
        .execute(&db.pool)
        .await
        .unwrap();
    let next = claim(&db, "two", 30).await;
    assert_eq!(next.mode(), ClaimMode::Observe);
    assert_eq!(
        db.store.reconciliation_object(&next, "pvc").await.unwrap(),
        Some(object())
    );
    assert!(
        db.store
            .record_reconciliation_object(&lease, "pv", &object())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE reconciliation_objects SET binding='{}'")
            .execute(&db.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn object_identity_write_rollback_and_authority_checks_leave_no_partial_binding() {
    let (db, token, doc) = fixture().await;
    publish(&db, &token, "objects", &doc).await;
    let lease = claim(&db, "one", 30).await;
    db.store
        .begin_reconciliation_dispatch(&lease)
        .await
        .unwrap();
    let before = count(&db, "events").await;
    let outbox = count(&db, "outbox").await;
    sqlx::raw_sql("CREATE FUNCTION reject_object() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$; CREATE TRIGGER reject_object BEFORE INSERT ON reconciliation_objects FOR EACH ROW EXECUTE FUNCTION reject_object();").execute(&db.pool).await.unwrap();
    assert!(
        db.store
            .record_reconciliation_object(&lease, "pvc", &object())
            .await
            .is_err()
    );
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "outbox").await, outbox);
    assert_eq!(count(&db, "reconciliation_objects").await, 0);
    sqlx::query("DROP TRIGGER reject_object ON reconciliation_objects")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .disable_principal(&org("acme"), &principal("alice"))
        .await
        .unwrap();
    assert!(
        db.store
            .record_reconciliation_object(&lease, "pvc", &object())
            .await
            .is_err()
    );
    assert!(db.store.reconciliation_object(&lease, "pvc").await.is_err());
    assert_eq!(count(&db, "reconciliation_objects").await, 0);
}

#[tokio::test]
async fn filtered_workers_do_not_claim_other_kinds_or_skip_dependencies() {
    let (db, token, doc) = fixture().await;
    let (_, operation) = publish(&db, &token, "initial", &doc).await;
    let worker = WorkerId::new("volume-only").unwrap();
    assert!(matches!(
        db.store
            .claim_reconciliation_kind(
                &org("acme"),
                &worker,
                Duration::from_secs(30),
                DefinitionKind::Workspace
            )
            .await
            .unwrap(),
        ClaimOutcome::Idle
    ));
    let lease = match db
        .store
        .claim_reconciliation_kind(
            &org("acme"),
            &worker,
            Duration::from_secs(30),
            DefinitionKind::Volume,
        )
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed(l) => l,
        other => panic!("{other:?}"),
    };
    assert_eq!(lease.task().kind, DefinitionKind::Volume);
    succeed(&db, &lease).await;
    assert!(matches!(
        db.store
            .claim_reconciliation_kind(
                &org("acme"),
                &worker,
                Duration::from_secs(30),
                DefinitionKind::Volume
            )
            .await
            .unwrap(),
        ClaimOutcome::Idle
    ));
    let next = claim(&db, "workspace", 30).await;
    assert_eq!(next.task().kind, DefinitionKind::Workspace);
    assert_eq!(next.task().operation_id, operation.operation_id);
}
