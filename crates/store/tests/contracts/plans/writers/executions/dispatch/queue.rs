use super::*;
use agent_computer_store::runtime::preparation::PreparationTarget;

async fn target(db: &Database) -> PreparationTarget {
    let value: serde_json::Value =
        sqlx::query_scalar("SELECT binding->'storage_target' FROM execution_requests LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn queue_claim_requires_the_exact_organization_and_complete_storage_target() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    submit(&db, &token, &computer, &submission(&db, &lease).await).await;
    let target = target(&db).await;
    let before = count(&db, "events").await;
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
        let mut changed = serde_json::to_value(&target).unwrap();
        changed[field] = if field.starts_with("writer_") {
            json!(12345)
        } else {
            json!("different")
        };
        let changed = serde_json::from_value(changed).unwrap();
        assert!(
            matches!(
                db.store
                    .claim_queued_candidate_execution(&org("acme"), &changed)
                    .await
                    .unwrap(),
                QueuedDispatch::Idle
            ),
            "{field}"
        );
    }
    assert!(matches!(
        db.store
            .claim_queued_candidate_execution(&org("other"), &target)
            .await
            .unwrap(),
        QueuedDispatch::Idle
    ));
    let mut invalid = target.clone();
    invalid.pvc_uid.clear();
    assert!(matches!(
        db.store
            .claim_queued_candidate_execution(&org("acme"), &invalid)
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
    assert!(matches!(
        db.store
            .claim_queued_candidate_execution(&org("acme"), &target)
            .await
            .unwrap(),
        QueuedDispatch::Claimed(_)
    ));
}

#[tokio::test]
async fn concurrent_queue_claims_have_one_winner_and_restart_never_replays() {
    let (mut db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let queued = submit(&db, &token, &computer, &submission(&db, &lease).await).await;
    let target = target(&db).await;
    let organization = org("acme");
    let (a, b) = tokio::join!(
        db.store
            .claim_queued_candidate_execution(&organization, &target),
        db.store
            .claim_queued_candidate_execution(&organization, &target)
    );
    let attempt = match (a.unwrap(), b.unwrap()) {
        (QueuedDispatch::Claimed(a), QueuedDispatch::Idle)
        | (QueuedDispatch::Idle, QueuedDispatch::Claimed(a)) => a,
        other => panic!("{other:?}"),
    };
    assert_eq!(attempt.intent().execution.execution_id, queued.execution_id);
    assert_eq!(attempt.intent().deadline_at_ms, queued.queue_deadline_at_ms);
    assert!(attempt.remaining_budget_ms().unwrap() <= 30_000);
    drop(attempt);
    let before = count(&db, "events").await;
    db.crash_and_restart().await;
    assert!(matches!(
        db.store
            .claim_queued_candidate_execution(&organization, &target)
            .await
            .unwrap(),
        QueuedDispatch::Idle
    ));
    assert_eq!(count(&db, "events").await, before);
    assert!(matches!(
        db.store
            .begin_candidate_execution_dispatch(&organization, &queued.execution_id, 1)
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
    db.store
        .mark_candidate_execution_unknown(&organization, &queued.execution_id, 2)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .claim_queued_candidate_execution(&organization, &target)
            .await
            .unwrap(),
        QueuedDispatch::Idle
    ));
    assert_eq!(count(&db, "execution_dispatch_intents").await, 1);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 1);
}

#[tokio::test]
async fn queue_poll_cancels_revoked_or_closed_requests_without_dispatch() {
    for sql in [
        "UPDATE service_credentials SET revoked=true WHERE 'runtime.modify'=ANY(scopes)",
        "DELETE FROM runtime_grants WHERE kind='computer' AND permission='modify'",
        "DELETE FROM runtime_grants WHERE kind='workspace' AND permission='read'",
        "UPDATE connection_sessions SET expires_at_ms=0",
    ] {
        let (db, token, computer, input) = setup().await;
        let lease = acquire(&db, &token, &computer, &input).await;
        let queued = submit(&db, &token, &computer, &submission(&db, &lease).await).await;
        let target = target(&db).await;
        // Connection closure goes through its journal API; all other mutations
        // model authority changes in the authoritative fixture database.
        if sql.starts_with("UPDATE connection_sessions") {
            db.store
                .close_connection_session(&token, &lease.connection_session_id)
                .await
                .unwrap();
        } else {
            sqlx::query(sql).execute(&db.pool).await.unwrap();
        }
        let result = db
            .store
            .claim_queued_candidate_execution(&org("acme"), &target)
            .await
            .unwrap();
        match result {
            QueuedDispatch::Cancelled(cancelled) => {
                assert_eq!(cancelled.execution_id, queued.execution_id)
            }
            QueuedDispatch::Idle if sql.starts_with("UPDATE connection_sessions") => {}
            other => panic!("{sql}: {other:?}"),
        }
        assert_eq!(
            db.store
                .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
                .await
                .unwrap()
                .state,
            ExecutionState::Cancelled
        );
        assert!(matches!(
            db.store
                .claim_queued_candidate_execution(&org("acme"), &target)
                .await
                .unwrap(),
            QueuedDispatch::Idle
        ));
        assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
        assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    }
}

#[tokio::test]
async fn failed_queue_outbox_rolls_back_claim_and_expiry_never_gets_a_new_budget() {
    let (db, token, computer, mut input) = setup().await;
    input.duration_seconds = 2;
    let lease = acquire(&db, &token, &computer, &input).await;
    let queued = submit(&db, &token, &computer, &submission(&db, &lease).await).await;
    let target = target(&db).await;
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_queue_claim() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END; $$; CREATE TRIGGER reject_queue_claim BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_queue_claim();").execute(&db.pool).await.unwrap();
    assert!(
        db.store
            .claim_queued_candidate_execution(&org("acme"), &target)
            .await
            .is_err()
    );
    assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    assert_eq!(count(&db, "events").await, before);
    let state: String = sqlx::query_scalar("SELECT state FROM execution_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "Queued");
    sqlx::query("DROP TRIGGER reject_queue_claim ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    match db
        .store
        .claim_queued_candidate_execution(&org("acme"), &target)
        .await
        .unwrap()
    {
        QueuedDispatch::Cancelled(cancelled) => {
            assert_eq!(cancelled.state, ExecutionState::Cancelled);
            assert_eq!(cancelled.queue_deadline_at_ms, queued.queue_deadline_at_ms);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
}

#[tokio::test]
async fn migration_twenty_four_preserves_queue_and_index_excludes_historical_rows() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let queued = submit(&db, &token, &computer, &submission(&db, &lease).await).await;
    db.remove_execution_queue_poll().await;
    db.store.migrate().await.unwrap();
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap(),
        queued
    );
    let index: String = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE indexname='execution_queue_poll'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(index.contains("WHERE (state = 'Queued'::text)"), "{index}");
    assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
}
