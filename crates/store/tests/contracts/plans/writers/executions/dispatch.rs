mod inputs;
mod lease_policy;
mod pods;
mod queue;
mod startup;
mod watchdogs;
use super::*;

async fn begin(db: &Database, queued: &ExecutionRequest) -> ExecutionDispatchAttempt {
    db.store
        .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, queued.revision)
        .await
        .unwrap()
}

#[tokio::test]
async fn execution_dispatch_has_one_winner_and_wal_recovery_never_reissues_attempt() {
    let (mut db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let organization = org("acme");
    assert!(matches!(
        db.store
            .begin_candidate_execution_dispatch(&organization, &queued.execution_id, 2)
            .await,
        Err(Error::RuntimeConflict)
    ));
    let (a, b) = tokio::join!(
        db.store
            .begin_candidate_execution_dispatch(&organization, &queued.execution_id, 1),
        db.store
            .begin_candidate_execution_dispatch(&organization, &queued.execution_id, 1)
    );
    let attempt = match (a, b) {
        (Ok(a), Err(Error::DispatchAlreadyStarted))
        | (Err(Error::DispatchAlreadyStarted), Ok(a)) => a,
        other => panic!("{other:?}"),
    };
    assert!(attempt.remaining_budget_ms().unwrap() <= 30000);
    let intent = attempt.intent();
    assert_eq!(intent.deadline_at_ms, queued.queue_deadline_at_ms);
    assert_eq!(intent.execution.state, ExecutionState::Dispatching);
    assert!(intent.execution.dispatch_started);
    assert_eq!(intent.execution.revision, 2);
    assert_eq!(intent.input.command.argv, input.command.argv);
    assert_eq!(intent.binding["prepared"]["data_inode"], 123);
    let fixed = serde_json::to_value(intent).unwrap();
    drop(attempt);
    let before = count(&db, "events").await;
    db.crash_and_restart().await;
    assert_eq!(
        serde_json::to_value(
            db.store
                .candidate_execution_dispatch(&organization, &queued.execution_id)
                .await
                .unwrap()
        )
        .unwrap(),
        fixed
    );
    assert!(matches!(
        db.store
            .begin_candidate_execution_dispatch(&organization, &queued.execution_id, 1)
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "execution_dispatch_intents").await, 1);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 1);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert!(
        !db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .ready
    );
    let events: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT payload FROM events WHERE kind='execution.status_changed'")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert!(
        events
            .iter()
            .all(|e| !e.to_string().contains("literal-private-command"))
    );
    assert_eq!(
        submit(&db, &token, &computer, &input).await.state,
        ExecutionState::Dispatching
    );
    for sql in [
        "UPDATE execution_dispatch_intents SET deadline_at_ms=deadline_at_ms+1",
        "DELETE FROM execution_dispatch_intents",
        "UPDATE execution_requests SET state='Cancelled',reason='user_requested',revision=revision+1",
        "UPDATE execution_requests SET state='Queued',reason='awaiting_runtime_dispatch',revision=revision+1",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err(), "{sql}");
    }
}

#[tokio::test]
async fn execution_dispatch_and_queued_cancel_are_serialized() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let organization = org("acme");
    let cancel_key = key("cancel");
    let cancel_input = CancelExecution {
        expected_revision: 1,
    };
    let (dispatch, cancel) = tokio::join!(
        db.store
            .begin_candidate_execution_dispatch(&organization, &queued.execution_id, 1),
        db.store.cancel_candidate_execution(
            &token,
            &cancel_key,
            &queued.execution_id,
            &cancel_input
        )
    );
    match (dispatch, cancel) {
        (Ok(_), Err(Error::RuntimeConflict)) => {
            assert_eq!(count(&db, "candidate_writer_dispatches").await, 1)
        }
        (Err(Error::WriterLeaseInactive), Ok(cancelled)) => {
            assert_eq!(cancelled.state, ExecutionState::Cancelled);
            assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn dispatched_cancel_and_unknown_never_claim_stopped_or_allow_handoff() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let cancel_input = CancelExecution {
        expected_revision: 2,
    };
    let cancelled = db
        .store
        .cancel_candidate_execution(&token, &key("cancel"), &queued.execution_id, &cancel_input)
        .await
        .unwrap();
    assert_eq!(cancelled.state, ExecutionState::CancelRequested);
    assert!(cancelled.dispatch_started);
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap(),
        cancelled
    );
    assert_eq!(
        db.store
            .cancel_candidate_execution(&token, &key("cancel"), &queued.execution_id, &cancel_input)
            .await
            .unwrap(),
        cancelled
    );
    let writer = db
        .store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(writer.state, WriterLeaseState::Draining);
    assert!(writer.release_proof.is_none());
    assert!(matches!(
        db.store
            .acquire_candidate_writer(&token, &key("replacement"), &computer, &acquire_input)
            .await,
        Err(Error::WriterLeaseBusy)
    ));
    assert!(sqlx::query("INSERT INTO candidate_writer_completions (organization,dispatch_id,lease_id,epoch,prepared_digest,input_digest,observed,accepted) SELECT l.organization,d.dispatch_id,l.lease_id,l.epoch,l.prepared_digest,d.input_digest,'{\"drain_confirmed\":true}'::jsonb,'{}'::jsonb FROM candidate_writer_leases l JOIN candidate_writer_dispatches d USING(organization,lease_id,epoch)").execute(&db.pool).await.is_err());
    assert!(sqlx::query("INSERT INTO candidate_writer_drains (organization,lease_id,epoch,proof) SELECT organization,lease_id,epoch,'bounded_file_drained' FROM candidate_writer_leases").execute(&db.pool).await.is_err());
    let unknown = db
        .store
        .mark_candidate_execution_unknown(&org("acme"), &queued.execution_id, cancelled.revision)
        .await
        .unwrap();
    assert_eq!(unknown.state, ExecutionState::Unknown);
    assert_eq!(unknown.reason, "dispatch_unconfirmed");
    let before = count(&db, "events").await;
    assert_eq!(
        db.store
            .mark_candidate_execution_unknown(
                &org("acme"),
                &queued.execution_id,
                cancelled.revision
            )
            .await
            .unwrap(),
        unknown
    );
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert_eq!(
        db.store
            .candidate_execution_dispatch(&org("acme"), &queued.execution_id)
            .await
            .unwrap()
            .intent_digest,
        attempt.intent().intent_digest
    );
    assert!(matches!(
        db.store
            .begin_candidate_execution_dispatch(
                &org("acme"),
                &queued.execution_id,
                unknown.revision
            )
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
}

#[tokio::test]
async fn execution_dispatch_rechecks_original_credential_grants_catalog_and_connection() {
    for sql in [
        "UPDATE service_credentials SET revoked=true WHERE 'runtime.modify'=ANY(scopes)",
        "UPDATE principals SET enabled=false WHERE principal='alice'",
        "UPDATE service_credentials SET scopes=array_remove(scopes,'runtime.modify')",
        "DELETE FROM runtime_grants WHERE kind='workspace' AND permission='read'",
        "DELETE FROM runtime_grants WHERE kind='computer' AND permission='modify'",
        "UPDATE catalog_references SET enabled=false WHERE kind='storage_class'",
    ] {
        let (db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let input = submission(&db, &lease).await;
        let queued = submit(&db, &token, &computer, &input).await;
        sqlx::query(sql).execute(&db.pool).await.unwrap();
        assert!(
            matches!(
                db.store
                    .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
                    .await,
                Err(Error::WriterLeaseInactive)
            ),
            "{sql}"
        );
        let current = db
            .store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap();
        assert_eq!(current.state, ExecutionState::Cancelled, "{sql}");
        assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
        assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    }
}

#[tokio::test]
async fn execution_dispatch_revocation_and_connection_close_make_outcome_unknown() {
    for revoke in [true, false] {
        let (db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let input = submission(&db, &lease).await;
        let queued = submit(&db, &token, &computer, &input).await;
        begin(&db, &queued).await;
        if revoke {
            sqlx::query(
                "UPDATE service_credentials SET revoked=true WHERE 'runtime.modify'=ANY(scopes)",
            )
            .execute(&db.pool)
            .await
            .unwrap();
        } else {
            db.store
                .close_connection_session(&token, &lease.connection_session_id)
                .await
                .unwrap();
        }
        let unknown = db
            .store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap();
        assert_eq!(unknown.state, ExecutionState::Unknown);
        assert_eq!(unknown.reason, "writer_unavailable");
        assert!(unknown.dispatch_started);
        assert_eq!(
            db.store
                .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
                .await
                .unwrap()
                .state,
            WriterLeaseState::Draining
        );
        assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    }
}

#[tokio::test]
async fn execution_dispatch_outbox_failure_and_late_expiry_roll_back_all_journals() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let before = count(&db, "events").await;
    let revision = db
        .store
        .candidate_writer(&token, &lease.lease_id)
        .await
        .unwrap()
        .revision;
    sqlx::raw_sql("CREATE FUNCTION reject_dispatch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END; $$; CREATE TRIGGER reject_dispatch BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_dispatch();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
            .await,
        Err(Error::Database(_))
    ));
    sqlx::raw_sql("DROP TRIGGER reject_dispatch ON outbox; CREATE FUNCTION expire_dispatch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.modify'=ANY(scopes); RETURN NEW; END; $$; CREATE TRIGGER expire_dispatch AFTER INSERT ON execution_dispatch_intents FOR EACH ROW EXECUTE FUNCTION expire_dispatch();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .revision,
        revision
    );
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap(),
        queued
    );
    sqlx::query("DROP TRIGGER expire_dispatch ON execution_dispatch_intents")
        .execute(&db.pool)
        .await
        .unwrap();
    begin(&db, &queued).await;
}

#[tokio::test]
async fn execution_dispatch_deadline_and_attempt_budget_cannot_renew() {
    let (db, token, computer, mut acquire_input) = setup().await;
    acquire_input.duration_seconds = 1;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let writer = db
        .store
        .candidate_writer(&token, &lease.lease_id)
        .await
        .unwrap();
    db.store
        .renew_candidate_writer(
            &token,
            &key("renew"),
            &lease.lease_id,
            &RenewWriterLease {
                lease: command(&writer),
                duration_seconds: 30,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(matches!(
        attempt.remaining_budget_ms(),
        Err(Error::WriterLeaseInactive)
    ));
    let unknown = db
        .store
        .candidate_execution(&token, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(unknown.state, ExecutionState::Unknown);
    assert_eq!(unknown.queue_deadline_at_ms, queued.queue_deadline_at_ms);
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn incomplete_execution_dispatch_intent_cannot_commit() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    submit(&db, &token, &computer, &input).await;
    for write_dispatch in [false, true] {
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO execution_dispatch_intents (organization,execution_id,lease_id,epoch,intent_digest,started_at_ms,deadline_at_ms) SELECT organization,execution_id,lease_id,epoch,$1,floor(extract(epoch from clock_timestamp())*1000),queue_deadline_at_ms FROM execution_requests")
            .bind(format!("sha256:{}","a".repeat(64))).execute(&mut *tx).await.unwrap();
        if write_dispatch {
            sqlx::query("INSERT INTO candidate_writer_dispatches (organization,dispatch_id,lease_id,epoch,input_digest) SELECT organization,execution_id,lease_id,epoch,intent_digest FROM execution_dispatch_intents").execute(&mut *tx).await.unwrap();
        }
        assert!(tx.commit().await.is_err());
        assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
        assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    }
}

#[tokio::test]
async fn migration_thirteen_preserves_queued_identity_and_does_not_dispatch() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    db.remove_execution_dispatch().await;
    assert!(db.store.ready().await.is_err());
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(submit(&db, &token, &computer, &input).await, queued);
    assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
    begin(&db, &queued).await;
}

#[tokio::test]
async fn execution_dispatch_uses_connection_owner_not_original_start_credential() {
    let (db, starter, computer, mut acquire_input) = setup().await;
    let owner = runtime_token(&db, "acme", "bob", &ServiceScope::ALL).await;
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    for (kind, id, permissions) in [
        (
            RuntimeKind::Computer,
            &computer,
            vec![
                RuntimePermission::Connect,
                RuntimePermission::Read,
                RuntimePermission::Modify,
            ],
        ),
        (
            RuntimeKind::Workspace,
            &workspace,
            vec![RuntimePermission::Read, RuntimePermission::Modify],
        ),
    ] {
        for permission in permissions {
            db.store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org("acme"),
                        principal: &principal("bob"),
                        kind,
                        resource_id: id,
                        permission,
                        max_runtime_seconds: None,
                    },
                    true,
                )
                .await
                .unwrap();
        }
    }
    let session = connection(&db, &owner, &computer, "bob", 900).await;
    acquire_input.connection_session_id = session.session_id;
    let lease = acquire(&db, &owner, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &owner, &computer, &input).await;
    sqlx::query("UPDATE service_credentials SET revoked=true WHERE principal='alice'")
        .execute(&db.pool)
        .await
        .unwrap();
    let attempt = begin(&db, &queued).await;
    assert_eq!(
        attempt.intent().input.lease.connection_session_id,
        lease.connection_session_id
    );
    assert!(
        db.store
            .candidate_execution(&starter, &queued.execution_id)
            .await
            .is_err()
    );
    assert_eq!(
        db.store
            .candidate_execution(&owner, &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Dispatching
    );
}

#[tokio::test]
async fn execution_cancellation_and_unknown_events_roll_back_writer_authority_together() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_execution_stop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='writer.draining' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_execution_stop BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION reject_execution_stop();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .cancel_candidate_execution(
                &token,
                &key("cancel"),
                &queued.execution_id,
                &CancelExecution {
                    expected_revision: 2
                }
            )
            .await,
        Err(Error::Database(_))
    ));
    assert!(matches!(
        db.store
            .mark_candidate_execution_unknown(&org("acme"), &queued.execution_id, 2)
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap(),
        attempt.intent().execution
    );
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Held
    );
    sqlx::query("DROP TRIGGER reject_execution_stop ON events")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .cancel_candidate_execution(
                &token,
                &key("cancel"),
                &queued.execution_id,
                &CancelExecution {
                    expected_revision: 2
                }
            )
            .await
            .unwrap()
            .state,
        ExecutionState::CancelRequested
    );
}
