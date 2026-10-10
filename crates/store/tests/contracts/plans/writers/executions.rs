mod dispatch;
use super::*;
use serde_json::json;

pub(super) async fn submission(db: &Database, lease: &WriterLease) -> SubmitExecution {
    let sandbox: String = sqlx::query_scalar(
        "SELECT resource_id FROM resource_definitions WHERE kind='sandbox' AND name='exec-env'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    SubmitExecution {
        lease_id: lease.lease_id.clone(),
        lease: command(lease),
        sandbox_id: sandbox,
        lifetime: ExecutionLifetime::Connection,
        command: ExecutionCommand {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf literal-private-command".into(),
            ],
            cwd: String::new(),
            timeout_seconds: 10,
            term_grace_ms: 100,
            output_limit_bytes: 4096,
        },
    }
}
pub(super) async fn submit(
    db: &Database,
    token: &str,
    computer: &str,
    input: &SubmitExecution,
) -> ExecutionRequest {
    db.store
        .submit_candidate_execution(token, &key("submit"), computer, input)
        .await
        .unwrap()
}

#[tokio::test]
async fn execution_queue_is_durable_bound_and_retry_never_extends_or_dispatches() {
    let (mut db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let first = submit(&db, &token, &computer, &input).await;
    assert_eq!(first.state, ExecutionState::Queued);
    assert!(!first.dispatch_started);
    assert_eq!(first.queue_deadline_at_ms, lease.expires_at_ms);
    assert_eq!(first.sandbox_revision, 1);
    let binding: serde_json::Value = sqlx::query_scalar("SELECT binding FROM execution_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        binding["sandbox"]["spec"]["image"],
        format!("registry.example.invalid/tools@sha256:{}", "b".repeat(64))
    );
    assert_eq!(binding["prepared"]["data_inode"], 123);
    assert_eq!(binding["storage_target"]["pvc_uid"], "pvc-uid");
    assert_eq!(binding["input_revision"], 1);
    let event: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM events WHERE kind='execution.status_changed'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!event.to_string().contains("literal-private-command"));
    let before = count(&db, "events").await;
    db.crash_and_restart().await;
    assert_eq!(first, submit(&db, &token, &computer, &input).await);
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    assert!(
        !db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .ready
    );
    let mut changed = input.clone();
    changed.command.argv.push("different".into());
    assert!(matches!(
        db.store
            .submit_candidate_execution(&token, &key("submit"), &computer, &changed)
            .await,
        Err(Error::IdempotencyConflict)
    ));
    for sql in [
        "UPDATE execution_requests SET input='{}'::jsonb",
        "UPDATE execution_requests SET state='Cancelled',revision=revision+1",
        "DELETE FROM execution_requests",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
}

#[tokio::test]
async fn queue_reserves_the_writer_slot_and_cancel_restores_file_admission() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let current = db
        .store
        .candidate_writer(&token, &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(current.revision, lease.revision + 1);
    assert!(!current.dispatch_recorded);
    assert!(matches!(
        db.store
            .begin_candidate_writer_dispatch(
                &token,
                &lease.lease_id,
                &command(&current),
                WriterDispatch {
                    dispatch_id: "file",
                    input_digest: &format!("sha256:{}", "a".repeat(64))
                }
            )
            .await,
        Err(Error::WriterLeaseBusy)
    ));
    assert!(sqlx::query("INSERT INTO candidate_writer_dispatches (organization,dispatch_id,lease_id,epoch,input_digest) SELECT organization,'forged',lease_id,epoch,$1 FROM candidate_writer_leases").bind(format!("sha256:{}","a".repeat(64))).execute(&db.pool).await.is_err());
    let cancel = CancelExecution {
        expected_revision: queued.revision,
    };
    let cancelled = db
        .store
        .cancel_candidate_execution(&token, &key("cancel"), &queued.execution_id, &cancel)
        .await
        .unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    assert_eq!(cancelled.reason, "user_requested");
    assert_eq!(
        cancelled,
        db.store
            .cancel_candidate_execution(&token, &key("cancel"), &queued.execution_id, &cancel)
            .await
            .unwrap()
    );
    assert!(matches!(
        db.store
            .cancel_candidate_execution(&token, &key("stale"), &queued.execution_id, &cancel)
            .await,
        Err(Error::RuntimeConflict)
    ));
    let current = db
        .store
        .candidate_writer(&token, &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(current.state, WriterLeaseState::Held);
    dispatch(&db, &token, &current).await;
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 1);
    assert_eq!(cancelled, submit(&db, &token, &computer, &input).await);
}

#[tokio::test]
async fn release_cancels_queue_atomically_before_zero_dispatch_handoff() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let current = db
        .store
        .candidate_writer(&token, &lease.lease_id)
        .await
        .unwrap();
    let released = db
        .store
        .release_candidate_writer(&token, &key("release"), &lease.lease_id, &command(&current))
        .await
        .unwrap();
    assert_eq!(released.state, WriterLeaseState::Released);
    assert_eq!(released.release_proof.as_deref(), Some("no_dispatch"));
    let cancelled = db
        .store
        .candidate_execution(&token, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    let next = db
        .store
        .acquire_candidate_writer(&token, &key("next"), &computer, &acquire_input)
        .await
        .unwrap();
    assert_eq!(next.epoch, 2);
    assert_eq!(cancelled, submit(&db, &token, &computer, &input).await);
    let next_input = submission(&db, &next).await;
    assert_eq!(
        db.store
            .submit_candidate_execution(&token, &key("next-submit"), &computer, &next_input)
            .await
            .unwrap()
            .epoch,
        2
    );
}

#[tokio::test]
async fn execution_admission_rejects_stale_lease_foreign_sandbox_and_browser_reuse() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let browser: String = sqlx::query_scalar(
        "SELECT resource_id FROM resource_definitions WHERE kind='sandbox' AND name='browser-env'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    for sandbox in ["foreign".into(), browser] {
        let mut changed = input.clone();
        changed.sandbox_id = sandbox;
        assert!(matches!(
            db.store
                .submit_candidate_execution(&token, &key("invalid"), &computer, &changed)
                .await,
            Err(Error::ReferenceUnavailable)
        ));
    }
    let mut stale = input.clone();
    stale.lease.generation += 1;
    assert!(matches!(
        db.store
            .submit_candidate_execution(&token, &key("invalid"), &computer, &stale)
            .await,
        Err(Error::WriterLeaseConflict)
    ));
    let mut oversized = input.clone();
    oversized.command.timeout_seconds = 301;
    assert!(matches!(
        db.store
            .submit_candidate_execution(&token, &key("invalid"), &computer, &oversized)
            .await,
        Err(Error::RuntimeBudgetExceeded)
    ));
    let mut escaped = input.clone();
    escaped.command.cwd = "../escape".into();
    assert!(matches!(
        db.store
            .submit_candidate_execution(&token, &key("invalid"), &computer, &escaped)
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
    assert_eq!(count(&db, "execution_requests").await, 0);
    assert_eq!(
        submit(&db, &token, &computer, &input).await.state,
        ExecutionState::Queued
    );
}

#[tokio::test]
async fn execution_queue_binds_original_credential_and_revocation_lowers_authority() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let other = runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    assert!(matches!(
        db.store
            .submit_candidate_execution(&other, &key("other"), &computer, &input)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let queued = submit(&db, &token, &computer, &input).await;
    assert!(matches!(
        db.store
            .candidate_execution(&other, &queued.execution_id)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert!(matches!(
        db.store
            .cancel_candidate_execution(
                &other,
                &key("other"),
                &queued.execution_id,
                &CancelExecution {
                    expected_revision: 1
                }
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Computer,
                resource_id: &computer,
                permission: RuntimePermission::Modify,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    let cancelled = db
        .store
        .candidate_execution(&token, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    assert_eq!(cancelled.reason, "writer_unavailable");
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Modify,
        None,
    )
    .await;
    assert_eq!(cancelled, submit(&db, &token, &computer, &input).await);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
}

#[tokio::test]
async fn file_dispatch_and_execution_reservation_have_one_winner() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let submit_key = key("raced");
    let command = command(&lease);
    let hash = format!("sha256:{}", "c".repeat(64));
    let (queued, file) = tokio::join!(
        db.store
            .submit_candidate_execution(&token, &submit_key, &computer, &input),
        db.store.begin_candidate_writer_dispatch(
            &token,
            &lease.lease_id,
            &command,
            WriterDispatch {
                dispatch_id: "file",
                input_digest: &hash
            }
        )
    );
    assert_eq!(usize::from(queued.is_ok()) + usize::from(file.is_ok()), 1);
    assert!(matches!(
        queued,
        Ok(_) | Err(Error::WriterLeaseConflict | Error::DispatchAlreadyStarted)
    ));
    assert!(matches!(file, Ok(_) | Err(Error::WriterLeaseBusy)));
    assert_eq!(
        count(&db, "execution_requests").await + count(&db, "candidate_writer_dispatches").await,
        1
    );
}

#[tokio::test]
async fn execution_outbox_failure_and_late_expiry_roll_back_reservation_and_revision() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_execution() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END; $$; CREATE TRIGGER reject_execution BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_execution();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .submit_candidate_execution(&token, &key("submit"), &computer, &input)
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "execution_requests").await, 0);
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .revision,
        lease.revision
    );
    sqlx::raw_sql("DROP TRIGGER reject_execution ON outbox; CREATE FUNCTION expire_execution() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.modify'=ANY(scopes); RETURN NEW; END; $$; CREATE TRIGGER expire_execution AFTER INSERT ON execution_requests FOR EACH ROW EXECUTE FUNCTION expire_execution();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .submit_candidate_execution(&token, &key("submit"), &computer, &input)
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "execution_requests").await, 0);
    assert_eq!(count(&db, "events").await, before);
    sqlx::query("DROP TRIGGER expire_execution ON execution_requests")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        submit(&db, &token, &computer, &input).await.state,
        ExecutionState::Queued
    );
}

#[tokio::test]
async fn execution_queue_deadline_does_not_extend_with_writer_renewal() {
    let (db, token, computer, mut acquire_input) = setup().await;
    acquire_input.duration_seconds = 1;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let current = db
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
                lease: command(&current),
                duration_seconds: 30,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let cancelled = db
        .store
        .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    assert_eq!(cancelled.queue_deadline_at_ms, queued.queue_deadline_at_ms);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Held
    );
}

#[tokio::test]
async fn migration_twelve_does_not_invent_execution_for_existing_dispatch() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    dispatch(&db, &token, &lease).await;
    db.remove_execution_admission().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "execution_requests").await, 0);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 1);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert!(serde_json::from_value::<SubmitExecution>(json!({"lifetime":"background"})).is_err());
}

#[tokio::test]
async fn failed_queue_cancellation_rolls_back_writer_release_and_drain_proof() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let current = db
        .store
        .candidate_writer(&token, &lease.lease_id)
        .await
        .unwrap();
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_cancel() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.payload->>'state'='Cancelled' THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_cancel BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION reject_cancel();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .release_candidate_writer(&token, &key("release"), &lease.lease_id, &command(&current))
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Held
    );
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Queued
    );
    sqlx::query("DROP TRIGGER reject_cancel ON events")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .release_candidate_writer(&token, &key("release"), &lease.lease_id, &command(&current))
        .await
        .unwrap();
    assert_eq!(count(&db, "events").await, before + 3);
}

#[tokio::test]
async fn closing_connection_cancels_queued_execution_without_dispatch_or_resurrection() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    db.store
        .close_connection_session(&token, &lease.connection_session_id)
        .await
        .unwrap();
    let cancelled = db
        .store
        .candidate_execution(&token, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    assert_eq!(cancelled.reason, "writer_unavailable");
    assert_eq!(cancelled, submit(&db, &token, &computer, &input).await);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    let released = db
        .store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(released.state, WriterLeaseState::Released);
    assert_eq!(released.release_proof.as_deref(), Some("no_dispatch"));
}
