use super::*;

#[tokio::test]
async fn revoked_admission_credentials_and_grants_block_workers_without_dispatch() {
    let (db, token, _) = fixture().await;
    let (_, operation) = publish(&db, &token, "first", &agent_document("first", "external")).await;
    let credential_id = token.split('_').nth(1).unwrap();
    db.store
        .revoke_credential(&org("acme"), credential_id)
        .await
        .unwrap();
    let blocked = db
        .store
        .claim_reconciliation(
            &org("acme"),
            &WorkerId::new("worker").unwrap(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let ClaimOutcome::Blocked(progress) = blocked else {
        panic!("expected blocked")
    };
    assert_eq!(progress.reason, Some(ReconcileReason::AuthorizationRevoked));
    assert!(!progress.dispatch_started);
    assert_eq!(progress.attempts, 0);
    assert!(matches!(
        db.store
            .resume_reconciliation(&org("acme"), &operation.operation_id)
            .await,
        Err(Error::Unauthenticated)
    ));
    // A trusted operator can abandon undispatched work after permanent revocation.
    db.store
        .abandon_reconciliation(&org("acme"), &operation.operation_id)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM operations WHERE operation_id=$1")
            .bind(&operation.operation_id)
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        "Failed"
    );
    let fresh = credential(&db, "acme", "alice").await;
    publish(&db, &fresh, "other", &agent_document("other", "other")).await;
    let lease = claim(&db, "fresh", 30).await;
    grant(
        &db,
        "acme",
        "alice",
        DefinitionKind::Agent,
        "*",
        DefinitionPermission::Create,
        false,
    )
    .await;
    assert!(matches!(
        db.store.begin_reconciliation_dispatch(&lease).await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        db.store
            .renew_reconciliation(&lease, Duration::from_secs(30))
            .await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        db.store
            .finish_reconciliation(
                &lease,
                ReconcileOutcome::Blocked {
                    reason: ReconcileReason::BackendUnavailable
                }
            )
            .await,
        Err(Error::Forbidden)
    ));
}

#[tokio::test]
async fn legacy_unbound_operations_and_disabled_references_fail_closed() {
    let (db, token, document) = fixture().await;
    let (_, operation) = publish(&db, &token, "first", &document).await;
    sqlx::query("UPDATE operations SET credential_id=NULL WHERE operation_id=$1")
        .bind(&operation.operation_id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .claim_reconciliation(
                &org("acme"),
                &WorkerId::new("one").unwrap(),
                Duration::from_secs(30)
            )
            .await
            .unwrap(),
        ClaimOutcome::Blocked(_)
    ));
    db.store
        .abandon_reconciliation(&org("acme"), &operation.operation_id)
        .await
        .unwrap();
    let (db, token, document) = fixture().await;
    publish(&db, &token, "first", &document).await;
    let lease = claim(&db, "two", 30).await;
    let catalog = db
        .store
        .register_catalog_reference(
            &org("acme"),
            DefinitionKind::StorageClass,
            "juicefs-workspace",
        )
        .await
        .unwrap();
    db.store
        .disable_catalog_reference(&org("acme"), &catalog)
        .await
        .unwrap();
    assert!(matches!(
        db.store.begin_reconciliation_dispatch(&lease).await,
        Err(Error::ReferenceUnavailable)
    ));
    assert!(
        db.store
            .claim_reconciliation(
                &org("acme"),
                &WorkerId::new("bad-ttl").unwrap(),
                Duration::from_secs(301)
            )
            .await
            .is_err()
    );
    assert!(WorkerId::new("../escape").is_err());
}

#[tokio::test]
async fn credential_revocation_winning_dispatch_admission_never_sets_the_marker() {
    let (db, token, document) = fixture().await;
    let (_, operation) = publish(&db, &token, "first", &document).await;
    let lease = claim(&db, "worker", 30).await;
    let before = db
        .store
        .inspect_reconciliation(&org("acme"), &operation.operation_id)
        .await
        .unwrap();
    let mut revoke = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE service_credentials SET revoked=TRUE WHERE credential_id=$1")
        .bind(token.split('_').nth(1).unwrap())
        .execute(&mut *revoke)
        .await
        .unwrap();
    let store = db.store.clone();
    let dispatch = tokio::spawn(async move { store.begin_reconciliation_dispatch(&lease).await });
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name='agent_computer_store_tests' AND wait_event_type='Lock'").fetch_one(&db.pool).await.unwrap();
            if waiting>0 {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    revoke.commit().await.unwrap();
    assert!(matches!(
        dispatch.await.unwrap(),
        Err(Error::Unauthenticated)
    ));
    let after = db
        .store
        .inspect_reconciliation(&org("acme"), &operation.operation_id)
        .await
        .unwrap();
    assert_eq!(before.progress, after.progress);
    assert_eq!(before.watermark, after.watermark);
    assert!(!after.progress[0].dispatch_started);
}
