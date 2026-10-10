use super::*;

async fn listed(db: &Database, target: &PreparationTarget) -> Vec<String> {
    db.store
        .candidate_preparation_queue(&org("acme"), target)
        .await
        .unwrap()
}

#[tokio::test]
async fn preparation_discovery_is_read_only_and_claims_still_have_one_winner() {
    let (mut db, _, _, start, target) = setup().await;
    let before = count(&db, "events").await;
    for _ in 0..2 {
        assert_eq!(
            listed(&db, &target).await,
            std::slice::from_ref(&start.request_id)
        );
    }
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "candidate_preparations").await, 0);
    let organization = org("acme");
    let (a, b) = (WorkerId::new("a").unwrap(), WorkerId::new("b").unwrap());
    let (a, b) = tokio::join!(
        db.store
            .claim_candidate_preparation(&organization, &start.request_id, &a, &target),
        db.store
            .claim_candidate_preparation(&organization, &start.request_id, &b, &target)
    );
    let lease = match (a.unwrap(), b.unwrap()) {
        (PreparationClaim::Claimed(l), PreparationClaim::Busy)
        | (PreparationClaim::Busy, PreparationClaim::Claimed(l)) => l,
        other => panic!("{other:?}"),
    };
    assert!(listed(&db, &target).await.is_empty());
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    db.crash_and_restart().await;
    assert!(listed(&db, &target).await.is_empty());
    expire(&db).await;
    assert_eq!(
        listed(&db, &target).await,
        std::slice::from_ref(&start.request_id)
    );
    let recovery = claim(&db, &start, &target, "recovery").await;
    assert_eq!(recovery.mode(), ClaimMode::Observe);
    assert!(matches!(
        db.store.begin_candidate_preparation(&recovery).await,
        Err(Error::DispatchAlreadyStarted)
    ));
    db.store
        .finish_candidate_preparation(&recovery, &evidence(&recovery))
        .await
        .unwrap();
    assert!(listed(&db, &target).await.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM events WHERE kind='candidate.preparation_dispatched'"
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn preparation_discovery_filters_existing_binding_and_cancelled_or_completed_starts() {
    let (db, token, computer, start, target) = setup().await;
    assert!(
        db.store
            .candidate_preparation_queue(&org("foreign"), &target)
            .await
            .unwrap()
            .is_empty()
    );
    let mut wrong = target.clone();
    wrong.volume_id = "foreign".into();
    assert!(listed(&db, &wrong).await.is_empty());
    claim(&db, &start, &target, "claim").await;
    expire(&db).await;
    for field in [
        "volume_id",
        "namespace_uid",
        "pvc_uid",
        "pv_uid",
        "filesystem_uuid",
        "volume_path",
        "writer_uid",
        "writer_gid",
    ] {
        let mut wrong = serde_json::to_value(&target).unwrap();
        wrong[field] = if field.starts_with("writer_") {
            json!(12345)
        } else {
            json!("other")
        };
        assert!(
            listed(&db, &serde_json::from_value(wrong).unwrap())
                .await
                .is_empty(),
            "{field}"
        );
    }
    db.store
        .cancel_queued_computer_start(
            &token,
            &key("cancel"),
            &computer,
            &CancelQueuedStart {
                expected_revision: start.control_revision,
                request_id: start.request_id,
            },
        )
        .await
        .unwrap();
    assert!(listed(&db, &target).await.is_empty());
    assert_eq!(count(&db, "candidate_preparations").await, 1);
}

#[tokio::test]
async fn revoked_discovered_request_cannot_claim_and_does_not_hide_other_requests() {
    let (db, token, computer, first, target) = setup_many(20 * 1024 * 1024 * 1024, false, 2).await;
    let other: String = sqlx::query_scalar(
        "SELECT resource_id FROM resource_definitions WHERE kind='computer' AND resource_id<>$1",
    )
    .bind(&computer)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let second = db
        .store
        .admit_computer_start(
            &token,
            &key("second"),
            &other,
            &StartRequest {
                expected_revision: 1,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
                input_artifact_id: None,
            },
        )
        .await
        .unwrap();
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Computer,
                resource_id: &computer,
                permission: RuntimePermission::Activate,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    let found = listed(&db, &target).await;
    assert_eq!(found.len(), 2);
    assert!(found.contains(&first.request_id) && found.contains(&second.request_id));
    assert!(matches!(
        db.store
            .claim_candidate_preparation(
                &org("acme"),
                &first.request_id,
                &WorkerId::new("worker").unwrap(),
                &target
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let lease = claim(&db, &second, &target, "worker").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    db.store
        .finish_candidate_preparation(&lease, &evidence(&lease))
        .await
        .unwrap();
    assert_eq!(listed(&db, &target).await, [first.request_id]);
}

#[tokio::test]
async fn migration_twenty_five_retains_preparation_history_and_readiness() {
    let (db, token, computer, start, target) = setup().await;
    let lease = claim(&db, &start, &target, "worker").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    db.store.defer_candidate_preparation(&lease).await.unwrap();
    let before = count(&db, "events").await;
    db.remove_preparation_queue_poll().await;
    db.store.migrate().await.unwrap();
    assert_eq!(listed(&db, &target).await, [start.request_id]);
    assert_eq!(count(&db, "events").await, before);
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(current.start_state, Some(StartState::Preparing));
    assert!(!current.ready);
    let index: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE indexname='candidate_preparation_queue_poll'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        index.contains("Queued") && index.contains("Preparing"),
        "{index}"
    );
}
