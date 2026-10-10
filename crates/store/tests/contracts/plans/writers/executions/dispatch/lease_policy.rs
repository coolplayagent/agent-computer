use super::*;

#[tokio::test]
async fn migration_thirty_preserves_genuine_omitted_stream_input_and_legacy_retry_policy() {
    use sha2::{Digest, Sha256};
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let mut input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    db.remove_execution_output_chunks().await;
    // This disposable fixture models the pre-v30 input and retry hash, which
    // had no stream_output field. Restore all guards before testing the upgrade.
    input.stream_output = None;
    let credential: String =
        sqlx::query_scalar("SELECT credential_id FROM connection_sessions WHERE session_id=$1")
            .bind(&input.lease.connection_session_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let old_input = serde_json::to_value(&input).unwrap();
    assert!(old_input.get("stream_output").is_none());
    let mut h = Sha256::new();
    h.update(b"agent-computer/execution-submit-v1\0");
    h.update(serde_json::to_vec(&json!([computer, old_input, credential])).unwrap());
    let hash = format!("sha256:{:x}", h.finalize());
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE execution_requests DISABLE TRIGGER check_execution_request")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE execution_requests SET input=$1,input_digest=$2 WHERE execution_id=$3")
        .bind(&old_input)
        .bind(&hash)
        .bind(&queued.execution_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE request_records SET input_digest=$1 WHERE operation='runtime.execution-submit.v1' AND request_key='submit'").bind(Sha256::digest(&hash).to_vec()).execute(&mut *tx).await.unwrap();
    sqlx::query("ALTER TABLE execution_requests ENABLE TRIGGER check_execution_request")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(r) FROM execution_requests r")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(before["binding"].get("output_stream").is_none());
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(r) FROM execution_requests r")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    let retry = submit(&db, &token, &computer, &input).await;
    assert!(!retry.stream_output);
    assert_eq!(retry.input_digest, hash);
    let attempt = begin(&db, &retry).await;
    assert_eq!(attempt.intent().bootstrap().unwrap().version, 1);
    assert!(attempt.output_capture().unwrap().is_none());
    assert_eq!(count(&db, "execution_output_chunk_intents").await, 0);
    assert!(sqlx::query("UPDATE execution_requests SET binding=binding||'{\"output_stream\":{\"version\":1}}'::jsonb").execute(&db.pool).await.is_err());
}

#[tokio::test]
async fn new_output_stream_policy_is_pinned_independently_of_renewal_and_retry() {
    for renewable in [Some(false), None] {
        for stream in [None, Some(true), Some(false)] {
            let (db, token, computer, acquire_input) = setup().await;
            let lease = acquire(&db, &token, &computer, &acquire_input).await;
            let mut input = submission(&db, &lease).await;
            input.renewable = renewable;
            input.stream_output = stream;
            assert_eq!(
                serde_json::to_value(&input)
                    .unwrap()
                    .get("stream_output")
                    .is_some(),
                stream.is_some()
            );
            let queued = submit(&db, &token, &computer, &input).await;
            assert_eq!(queued.stream_output, stream.unwrap_or(true));
            let attempt = begin(&db, &queued).await;
            let d = attempt.intent();
            assert_eq!(
                d.binding.get("output_stream").is_some(),
                queued.stream_output
            );
            assert_eq!(
                d.bootstrap().unwrap().version,
                if queued.stream_output {
                    3
                } else if queued.renewable {
                    2
                } else {
                    1
                }
            );
            assert_eq!(
                attempt.output_capture().unwrap().is_some(),
                queued.stream_output
            );
            let before = serde_json::to_value(d).unwrap();
            submit(&db, &token, &computer, &input).await;
            assert_eq!(
                serde_json::to_value(
                    db.store
                        .candidate_execution_dispatch(&org("acme"), &queued.execution_id)
                        .await
                        .unwrap()
                )
                .unwrap(),
                before
            );
            input.stream_output = if stream.is_none() { Some(true) } else { None };
            assert!(matches!(
                db.store
                    .submit_candidate_execution(&token, &key("submit"), &computer, &input)
                    .await,
                Err(Error::IdempotencyConflict)
            ));
        }
    }
}

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
