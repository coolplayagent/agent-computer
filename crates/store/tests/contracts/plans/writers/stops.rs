use super::*;
use serde_json::json;

async fn request(db: &Database, token: &str, computer: &str) -> StopPreparedComputer {
    let current = db.store.computer_runtime(token, computer).await.unwrap();
    StopPreparedComputer {
        expected_revision: current.revision,
        request_id: current.active_request.unwrap(),
    }
}
async fn finish(
    db: &Database,
    start: &StartReceipt,
    target: &agent_computer_store::runtime::preparation::PreparationTarget,
) {
    let lease = super::super::preparation::claim(db, start, target, "stop-preparer").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    db.store
        .finish_candidate_preparation(&lease, &super::super::preparation::evidence(&lease))
        .await
        .unwrap();
}

#[tokio::test]
async fn stop_survives_wal_restart_and_new_generation_retains_old_storage() {
    let (mut db, token, computer, first, target) =
        super::super::preparation::setup_with_quota(20 * 1024 * 1024 * 1024).await;
    finish(&db, &first, &target).await;
    let input = request(&db, &token, &computer).await;
    let stopped = db
        .store
        .stop_prepared_computer(&token, &key("stop"), &computer, &input)
        .await
        .unwrap();
    assert_eq!(stopped.control_revision, 5);
    assert_eq!(stopped.proof, "no_user_dispatch");
    assert_eq!(
        stopped.event_sequence,
        sqlx::query_scalar::<_, i64>("SELECT sequence FROM events WHERE kind='computer.stopped'")
            .fetch_one(&db.pool)
            .await
            .unwrap()
    );
    assert_eq!(
        stopped.input_manifest_digest,
        first.input_manifest_digest.unwrap()
    );
    assert_eq!(stopped.retained_storage_bytes, first.storage_bytes);
    db.crash_and_restart().await;
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert!(current.active_request.is_none());
    assert_eq!(current.stop_receipt, Some(stopped.clone()));
    let before = count(&db, "events").await;
    assert_eq!(
        stopped,
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &input)
            .await
            .unwrap()
    );
    assert_eq!(count(&db, "events").await, before);
    let next = db
        .store
        .admit_computer_start(
            &token,
            &key("restart"),
            &computer,
            &StartRequest {
                expected_revision: stopped.control_revision,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
                input_artifact_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!((next.control_revision, next.generation), (6, 2));
    assert_ne!(next.candidate_id, first.candidate_id);
    assert!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .stop_receipt
            .is_none()
    );
    // Retrying an old stop returns its historical receipt and cannot stop generation 2.
    assert_eq!(
        stopped,
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &input)
            .await
            .unwrap()
    );
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stale"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
    finish(&db, &next, &target).await;
    let second = request(&db, &token, &computer).await;
    db.store
        .stop_prepared_computer(&token, &key("stop-two"), &computer, &second)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("third"),
                &computer,
                &StartRequest {
                    expected_revision: second.expected_revision + 1,
                    expected_spec_revision: 1,
                    max_runtime_seconds: 300,
                    input_artifact_id: None,
                }
            )
            .await,
        Err(Error::RuntimeCapacityUnavailable)
    ));
    assert_eq!(count(&db, "candidate_preparations").await, 2);
    assert_eq!(count(&db, "runtime_stops").await, 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT sum(storage_bytes)::bigint FROM runtime_start_requests WHERE state='Stopped'"
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        20 * 1024 * 1024 * 1024
    );
}

#[tokio::test]
async fn unreleased_writer_blocks_stop_and_released_epoch_cannot_restart() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let stop = request(&db, &token, &computer).await;
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    // A closed connection only lowers admission; the writer must still be drained.
    db.store
        .close_connection_session(&token, &input.connection_session_id)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Released
    );
    db.store
        .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
        .await
        .unwrap();
    assert!(sqlx::query("UPDATE candidate_writer_leases SET state='Held',epoch=epoch+1,revision=revision+1,expires_at_ms=floor(extract(epoch from clock_timestamp())*1000)+30000").execute(&db.pool).await.is_err());
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 1);
}

#[tokio::test]
async fn dispatch_history_and_queued_execution_block_stop() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let stop = request(&db, &token, &computer).await;
    let submission = super::executions::submission(&db, &lease).await;
    let queued = super::executions::submit(&db, &token, &computer, &submission).await;
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    db.store
        .cancel_candidate_execution(
            &token,
            &key("cancel"),
            &queued.execution_id,
            &CancelExecution {
                expected_revision: queued.revision,
            },
        )
        .await
        .unwrap();
    let current = db
        .store
        .candidate_writer(&token, &lease.lease_id)
        .await
        .unwrap();
    dispatch(&db, &token, &current).await;
    db.store
        .close_connection_session(&token, &input.connection_session_id)
        .await
        .unwrap();
    db.store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
}

#[tokio::test]
async fn active_human_input_blocks_stop_until_idle_even_without_a_writer() {
    let (db, token, computer, input) = setup().await;
    let stop = request(&db, &token, &computer).await;
    let session = db
        .store
        .heartbeat_connection_session(
            &token,
            &key("active"),
            &input.connection_session_id,
            &ConnectionHeartbeat {
                expected_revision: 1,
                activity: ConnectionActivity::Active,
                visibility: ConnectionVisibility::Visible,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    db.store
        .heartbeat_connection_session(
            &token,
            &key("idle"),
            &input.connection_session_id,
            &ConnectionHeartbeat {
                expected_revision: session.revision,
                activity: ConnectionActivity::Idle,
                visibility: ConnectionVisibility::Visible,
            },
        )
        .await
        .unwrap();
    db.store
        .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .acquire_candidate_writer(&token, &key("old-candidate"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
}

#[tokio::test]
async fn acquiring_writer_and_stopping_share_one_authority_boundary() {
    let (db, token, computer, input) = setup().await;
    let stop = request(&db, &token, &computer).await;
    let acquire_key = key("acquire");
    let stop_key = key("stop");
    let (acquired, stopped) = tokio::join!(
        db.store
            .acquire_candidate_writer(&token, &acquire_key, &computer, &input),
        db.store
            .stop_prepared_computer(&token, &stop_key, &computer, &stop)
    );
    match (acquired, stopped) {
        (Ok(lease), Err(Error::RuntimeStopBlocked)) => {
            assert_eq!(lease.state, WriterLeaseState::Held);
            assert_eq!(count(&db, "runtime_stops").await, 0);
        }
        (Err(Error::RuntimeConflict), Ok(_)) => {
            assert_eq!(count(&db, "candidate_writer_leases").await, 0);
            assert_eq!(count(&db, "runtime_stops").await, 1);
        }
        result => panic!("{result:?}"),
    }
}

#[tokio::test]
async fn stop_rolls_back_on_outbox_failure_and_late_credential_expiry() {
    let (db, token, computer, _) = setup().await;
    let input = request(&db, &token, &computer).await;
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_stop_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END $$; CREATE TRIGGER reject_stop_event BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_stop_event();").execute(&db.pool).await.unwrap();
    assert!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &input)
            .await
            .is_err()
    );
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Prepared)
    );
    sqlx::raw_sql("DROP TRIGGER reject_stop_event ON outbox; CREATE FUNCTION expire_stop_credential() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.manage'=ANY(scopes); RETURN NEW; END $$; CREATE TRIGGER expire_stop_credential BEFORE INSERT ON request_records FOR EACH ROW EXECUTE FUNCTION expire_stop_credential();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &input)
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
    sqlx::query("DROP TRIGGER expire_stop_credential ON request_records")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .stop_prepared_computer(&token, &key("stop"), &computer, &input)
        .await
        .unwrap();
    for sql in [
        "UPDATE runtime_stops SET receipt='{}'::jsonb",
        "DELETE FROM runtime_stops",
        "UPDATE runtime_start_requests SET state='Prepared'",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
}

#[tokio::test]
async fn stop_requires_manage_again_on_retry_and_rejects_forged_proofs() {
    let (db, token, computer, _) = setup().await;
    let input = request(&db, &token, &computer).await;
    let narrow = runtime_token(&db, "acme", "alice", &[ServiceScope::RuntimeRead]).await;
    assert!(matches!(
        db.store
            .stop_prepared_computer(&narrow, &key("stop"), &computer, &input)
            .await,
        Err(Error::Forbidden)
    ));
    assert!(
        sqlx::query("UPDATE runtime_start_requests SET state='Stopped'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO runtime_stops SELECT organization,request_id,$1 FROM runtime_start_requests").bind(json!({"proof":"no_user_dispatch"})).execute(&db.pool).await.is_err());
    let stopped = db
        .store
        .stop_prepared_computer(&token, &key("stop"), &computer, &input)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(
                &token,
                &key("stop"),
                &computer,
                &StopPreparedComputer {
                    expected_revision: input.expected_revision + 1,
                    ..input.clone()
                }
            )
            .await,
        Err(Error::IdempotencyConflict)
    ));
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Computer,
                resource_id: &computer,
                permission: RuntimePermission::Manage,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &input)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(stopped.proof, "no_user_dispatch");
}

#[tokio::test]
async fn queued_and_preparing_are_not_assumed_to_be_stopped() {
    let (db, token, computer, start, target) = super::super::preparation::setup().await;
    let input = request(&db, &token, &computer).await;
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
    let lease = super::super::preparation::claim(&db, &start, &target, "in-flight").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
}

#[tokio::test]
async fn migration_twenty_preserves_old_dispatch_and_does_not_invent_stop_proof() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    dispatch(&db, &token, &lease).await;
    db.remove_undispatched_stop().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let stop = request(&db, &token, &computer).await;
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 1);
}

#[tokio::test]
async fn completed_file_in_an_older_epoch_still_requires_a_checkpoint() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let permit = dispatch(&db, &token, &lease).await;
    // Synthetic historical adapter report exercises only the SQL authority
    // boundary. Real bounded-file IO has separate storage/worker tests.
    let report = json!({"state":"Applied","version":{"sha256":format!("sha256:{}","b".repeat(64)),"size":4,"executable":false},"drain_confirmed":true});
    sqlx::query("INSERT INTO candidate_writer_completions (organization,dispatch_id,lease_id,epoch,prepared_digest,input_digest,observed,accepted) SELECT l.organization,d.dispatch_id,l.lease_id,l.epoch,l.prepared_digest,d.input_digest,$1,$1 FROM candidate_writer_leases l JOIN candidate_writer_dispatches d USING(organization,lease_id,epoch)").bind(report).execute(&db.pool).await.unwrap();
    db.store
        .release_candidate_writer(
            &token,
            &key("release-one"),
            &lease.lease_id,
            &command(permit.lease()),
        )
        .await
        .unwrap();
    let second = db
        .store
        .acquire_candidate_writer(&token, &key("second"), &computer, &input)
        .await
        .unwrap();
    assert_eq!(second.epoch, 2);
    assert!(!second.dispatch_recorded);
    let released = db
        .store
        .release_candidate_writer(
            &token,
            &key("release-two"),
            &lease.lease_id,
            &command(&second),
        )
        .await
        .unwrap();
    assert_eq!(released.release_proof.as_deref(), Some("no_dispatch"));
    let stop = request(&db, &token, &computer).await;
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
}
