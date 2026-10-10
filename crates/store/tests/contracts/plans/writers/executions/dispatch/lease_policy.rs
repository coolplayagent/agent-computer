use super::*;

#[tokio::test]
async fn new_admission_pins_policy_without_changing_omitted_input_or_queue_budget() {
    for renewable in [None, Some(true), Some(false)] {
        let (mut db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let mut input = submission(&db, &lease).await;
        input.renewable = renewable;
        let encoded = serde_json::to_value(&input).unwrap();
        assert_eq!(encoded.get("renewable").is_some(), renewable.is_some());
        let queued = submit(&db, &token, &computer, &input).await;
        assert_eq!(queued.renewable, renewable.unwrap_or(true));
        assert_eq!(queued.queue_deadline_at_ms, lease.expires_at_ms);
        let attempt = begin(&db, &queued).await;
        assert_eq!(attempt.intent().deadline_at_ms, queued.queue_deadline_at_ms);
        let bootstrap = attempt.intent().bootstrap().unwrap();
        if queued.renewable {
            assert_eq!(bootstrap.version, 2);
            assert_eq!(
                attempt.intent().binding["execution_lease"],
                json!({"version":1,"max_budget_ms":40000})
            );
            assert_eq!(
                attempt.intent().hard_deadline_at_ms,
                Some(attempt.intent().started_at_ms + 40000)
            );
        } else {
            assert_eq!(bootstrap.version, 1);
            assert!(bootstrap.hard_budget_ms.is_none());
            assert!(attempt.intent().hard_deadline_at_ms.is_none());
        }
        let digest = attempt.intent().intent_digest.clone();
        db.crash_and_restart().await;
        assert_eq!(
            submit(&db, &token, &computer, &input).await.input_digest,
            queued.input_digest
        );
        assert_eq!(
            db.store
                .candidate_execution_dispatch(&org("acme"), &queued.execution_id)
                .await
                .unwrap()
                .intent_digest,
            digest
        );
        input.renewable = if renewable.is_none() {
            Some(true)
        } else {
            None
        };
        assert!(matches!(
            db.store
                .submit_candidate_execution(&token, &key("submit"), &computer, &input)
                .await,
            Err(Error::IdempotencyConflict)
        ));
    }
}

#[tokio::test]
async fn renewable_hard_limit_is_capped_by_original_credential_and_connection() {
    for credential in [true, false] {
        let (db, token, computer, mut acquire_input) = setup().await;
        let until: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch from clock_timestamp())*1000)::bigint+35000",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if credential {
            sqlx::query("UPDATE service_credentials SET expires_at=to_timestamp($1::double precision/1000) WHERE 'runtime.modify'=ANY(scopes)").bind(until).execute(&db.pool).await.unwrap();
        } else {
            let session = connection(&db, &token, &computer, "short", 35).await;
            acquire_input.connection_session_id = session.session_id;
        }
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let mut input = submission(&db, &lease).await;
        input.renewable = None;
        let queued = submit(&db, &token, &computer, &input).await;
        let attempt = begin(&db, &queued).await;
        let hard = attempt.intent().hard_deadline_at_ms.unwrap();
        if credential {
            assert_eq!(hard, until);
        } else {
            let expiry: i64 = sqlx::query_scalar(
                "SELECT expires_at_ms FROM connection_sessions WHERE session_id=$1",
            )
            .bind(&acquire_input.connection_session_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(hard, expiry);
        }
        assert!(hard < attempt.intent().started_at_ms + 40000);
    }
}

#[tokio::test]
async fn migration_twenty_nine_preserves_fixed_dispatch_bytes_and_cannot_promote_history() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let before = serde_json::to_value(attempt.intent()).unwrap();
    db.remove_execution_renewal().await;
    assert!(db.store.ready().await.is_err());
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let after = db
        .store
        .candidate_execution_dispatch(&org("acme"), &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&after).unwrap(), before);
    assert!(!after.execution.renewable);
    assert_eq!(after.bootstrap().unwrap().version, 1);
    for sql in [
        "UPDATE execution_requests SET binding=binding||'{\"execution_lease\":{\"version\":1,\"max_budget_ms\":40000}}'::jsonb",
        "UPDATE execution_dispatch_intents SET hard_deadline_at_ms=deadline_at_ms+10000",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
    assert_eq!(count(&db, "execution_renewal_grants").await, 0);
    assert_eq!(count(&db, "execution_renewal_acks").await, 0);
}
