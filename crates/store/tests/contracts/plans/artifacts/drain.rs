use super::*;
use agent_computer_store::runtime::{connections::*, writers::*};

async fn writer(db: &Database, token: &str, computer: &str) -> (AcquireWriterLease, WriterLease) {
    for permission in [RuntimePermission::Manage, RuntimePermission::Connect] {
        allow(db, computer, RuntimeKind::Computer, permission, None).await;
    }
    let connection = db
        .store
        .create_connection_session(
            token,
            &key("drain-session"),
            computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::Modify,
                ],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    let runtime = db.store.computer_runtime(token, computer).await.unwrap();
    let candidate: String = sqlx::query_scalar("SELECT candidate_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let input = AcquireWriterLease {
        scope: WriterScope::Modify,
        connection_session_id: connection.session_id,
        candidate_id: candidate,
        generation: runtime.generation,
        duration_seconds: 30,
    };
    let lease = db
        .store
        .acquire_candidate_writer(token, &key("drain-writer"), computer, &input)
        .await
        .unwrap();
    (input, lease)
}
fn command(lease: &WriterLease) -> WriterLeaseCommand {
    WriterLeaseCommand {
        connection_session_id: lease.connection_session_id.clone(),
        generation: lease.generation,
        epoch: lease.epoch,
        expected_revision: lease.revision,
    }
}
fn request(input: &CommitArtifact) -> CheckpointStop {
    CheckpointStop {
        cancel_running: true,
        ..checkpoint::request(input)
    }
}
async fn start(
    db: &Database,
    token: &str,
    computer: &str,
    input: &CommitArtifact,
) -> ArtifactCommit {
    allow(
        db,
        computer,
        RuntimeKind::Computer,
        RuntimePermission::Manage,
        None,
    )
    .await;
    db.store
        .checkpoint_stop_computer(token, &key("drain"), computer, &request(input))
        .await
        .unwrap()
}
async fn execution(
    db: &Database,
    token: &str,
    computer: &str,
    lease: &WriterLease,
) -> (SubmitExecution, ExecutionRequest) {
    let sandbox: String = sqlx::query_scalar(
        "SELECT resource_id FROM resource_definitions WHERE kind='sandbox' AND name='exec-env'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let input = SubmitExecution {
        renewable: Some(false),
        stream_output: Some(false),
        lease_id: lease.lease_id.clone(),
        lease: command(lease),
        sandbox_id: sandbox,
        lifetime: ExecutionLifetime::Connection,
        command: ExecutionCommand {
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 20".into()],
            cwd: String::new(),
            timeout_seconds: 25,
            term_grace_ms: 100,
            output_limit_bytes: 4096,
        },
    };
    let queued = db
        .store
        .submit_candidate_execution(token, &key("drain-execution"), computer, &input)
        .await
        .unwrap();
    (input, queued)
}
async fn claim(db: &Database, id: &str) -> Option<ArtifactLease> {
    db.store
        .claim_artifact(&org("acme"), id, &WorkerId::new("drain-worker").unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn draining_queued_work_is_atomic_durable_and_never_dispatches() {
    let (mut db, token, computer, _, input) = setup().await;
    let (_, lease) = writer(&db, &token, &computer).await;
    let (_, queued) = execution(&db, &token, &computer, &lease).await;
    assert!(
        db.store
            .checkpoint_stop_computer(
                &token,
                &key("old-stop"),
                &computer,
                &checkpoint::request(&input)
            )
            .await
            .is_err()
    );
    let first = start(&db, &token, &computer, &input).await;
    assert_eq!(first.state, ArtifactState::Draining);
    assert!(first.cancel_running && first.stop_receipt.is_none());
    assert_eq!(first.drain_reason.as_deref(), Some("drain_pending"));
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Draining)
    );
    let cancelled = db
        .store
        .candidate_execution(&token, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(cancelled.state, ExecutionState::Cancelled);
    assert!(!cancelled.dispatch_started);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .release_proof
            .as_deref(),
        Some("no_dispatch")
    );
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert!(
        db.store
            .artifact_manifest(&token, &first.commit_id)
            .await
            .unwrap()
            .is_none()
    );
    let events = count(&db, "events").await;
    db.crash_and_restart().await;
    assert_eq!(start(&db, &token, &computer, &input).await, first);
    assert_eq!(count(&db, "events").await, events);
    assert!(matches!(
        db.store
            .checkpoint_stop_computer(
                &token,
                &key("drain"),
                &computer,
                &checkpoint::request(&input)
            )
            .await,
        Err(Error::IdempotencyConflict)
    ));
    let objects = Objects::new();
    let (worker, bundle) = capture(&db, &first.commit_id, &objects).await;
    let done = db
        .store
        .finish_artifact(
            &worker,
            &verify(&objects.client, &bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        done.stop_receipt.unwrap().control_revision,
        input.expected_revision + 4
    );
    assert!(done.drain_reason.is_none());
    assert_eq!(count(&db, "execution_dispatch_intents").await, 0);
}

#[tokio::test]
async fn draining_dispatch_requires_physical_proof_and_rejects_all_new_authority() {
    let (mut db, token, computer, _, input) = setup().await;
    let (acquire, lease) = writer(&db, &token, &computer).await;
    let (submit, queued) = execution(&db, &token, &computer, &lease).await;
    let attempt = db
        .store
        .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, queued.revision)
        .await
        .unwrap();
    let first = start(&db, &token, &computer, &input).await;
    assert_eq!(
        db.store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::CancelRequested
    );
    assert!(claim(&db, &first.commit_id).await.is_none());
    assert!(
        db.store
            .acquire_candidate_writer(&token, &key("new"), &computer, &acquire)
            .await
            .is_err()
    );
    assert!(
        db.store
            .renew_candidate_writer(
                &token,
                &key("renew"),
                &lease.lease_id,
                &RenewWriterLease {
                    lease: command(&lease),
                    duration_seconds: 30
                }
            )
            .await
            .is_err()
    );
    assert!(
        db.store
            .submit_candidate_execution(&token, &key("new-exec"), &computer, &submit)
            .await
            .is_err()
    );
    assert!(
        db.store
            .begin_candidate_writer_dispatch(
                &token,
                &lease.lease_id,
                &command(&lease),
                WriterDispatch {
                    dispatch_id: "new-file",
                    input_digest: &format!("sha256:{}", "a".repeat(64))
                }
            )
            .await
            .is_err()
    );
    for sql in [
        "UPDATE artifact_commits SET state='Capturing'",
        "UPDATE artifact_commits SET state='Committed',published_at_ms=1,input_revision=1,object_ref='{}'::jsonb",
        "UPDATE artifact_commits SET cancel_running=false",
        "UPDATE runtime_start_requests SET state='Sealing'",
        "UPDATE runtime_start_requests SET state='Prepared'",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err(), "{sql}");
    }
    drop(attempt);
    db.crash_and_restart().await;
    assert!(claim(&db, &first.commit_id).await.is_none());
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::CancelRequested
    );
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert_eq!(count(&db, "execution_dispatch_intents").await, 1);
}

#[tokio::test]
async fn draining_unknown_is_durable_and_new_publisher_credential_cannot_resolve_it() {
    let (db, token, computer, _, input) = setup().await;
    let (_, lease) = writer(&db, &token, &computer).await;
    let (_, queued) = execution(&db, &token, &computer, &lease).await;
    let attempt = db
        .store
        .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, queued.revision)
        .await
        .unwrap();
    let first = start(&db, &token, &computer, &input).await;
    sqlx::query("UPDATE service_credentials SET revoked=true")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Unknown
    );
    let fresh =
        super::super::runtime::runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    let pending = start(&db, &fresh, &computer, &input).await;
    assert_eq!(pending.drain_reason.as_deref(), Some("recovery_blocked"));
    assert!(claim(&db, &first.commit_id).await.is_none());
    assert!(
        db.store
            .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
            .await
            .is_err()
    );
    assert_eq!(count(&db, "runtime_stops").await, 0);
    drop(attempt);
}

#[tokio::test]
async fn draining_outbox_failure_rolls_back_cancellation_and_writer_boundary() {
    let (db, token, computer, _, input) = setup().await;
    let (_, lease) = writer(&db, &token, &computer).await;
    let (_, queued) = execution(&db, &token, &computer, &lease).await;
    let _attempt = db
        .store
        .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, queued.revision)
        .await
        .unwrap();
    let events = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_drain_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM events WHERE organization=NEW.organization AND sequence=NEW.sequence AND kind='writer.draining') THEN RAISE EXCEPTION 'injected drain failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_drain_event BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_drain_event();").execute(&db.pool).await.unwrap();
    assert!(
        db.store
            .checkpoint_stop_computer(&token, &key("drain"), &computer, &request(&input))
            .await
            .is_err()
    );
    assert_eq!(count(&db, "artifact_commits").await, 0);
    assert_eq!(count(&db, "events").await, events);
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Prepared)
    );
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Dispatching
    );
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
async fn draining_migration_preserves_existing_stop_and_hash_without_opt_in() {
    let (db, token, computer, _, input) = setup().await;
    let first = checkpoint::start(&db, &token, &computer, &input).await;
    db.remove_checkpoint_stop_drain().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(
        checkpoint::start(&db, &token, &computer, &input).await,
        first
    );
    assert!(!first.cancel_running);
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    assert!(
        db.store
            .finish_artifact(
                &lease,
                &verify(&objects.client, &bundle, None).await.unwrap()
            )
            .await
            .unwrap()
            .stop_receipt
            .is_some()
    );
}

#[tokio::test]
async fn draining_does_not_bypass_apps_or_other_active_connections() {
    for apps in [true, false] {
        let (db, token, computer, _, input) = setup_with_apps(apps).await;
        allow(
            &db,
            &computer,
            RuntimeKind::Computer,
            RuntimePermission::Manage,
            None,
        )
        .await;
        if !apps {
            checkpoint_authority::session(&db, &computer, "bob").await;
        }
        let error = db
            .store
            .checkpoint_stop_computer(&token, &key("drain"), &computer, &request(&input))
            .await
            .unwrap_err();
        assert!(matches!(
            (apps, error),
            (true, Error::RuntimeStopBlocked) | (false, Error::RuntimeActiveUse)
        ));
        assert_eq!(count(&db, "artifact_commits").await, 0);
        assert_eq!(
            db.store
                .computer_runtime(&token, &computer)
                .await
                .unwrap()
                .start_state,
            Some(StartState::Prepared)
        );
    }
}

#[tokio::test]
async fn draining_promotion_rechecks_authority_and_has_one_claim_winner() {
    let (db, token, computer, workspace, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Workspace,
                resource_id: &workspace,
                permission: RuntimePermission::Publish,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(
        db.store
            .claim_artifact(
                &org("acme"),
                &first.commit_id,
                &WorkerId::new("revoked").unwrap()
            )
            .await
            .is_err()
    );
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Draining
    );
    allow(
        &db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Publish,
        None,
    )
    .await;
    let (other, connection) = checkpoint_authority::session(&db, &computer, "bob").await;
    assert!(matches!(
        db.store
            .claim_artifact(
                &org("acme"),
                &first.commit_id,
                &WorkerId::new("active").unwrap()
            )
            .await,
        Err(Error::RuntimeActiveUse)
    ));
    db.store
        .close_connection_session(&other, &connection.session_id)
        .await
        .unwrap();
    let organization = org("acme");
    let one = WorkerId::new("one").unwrap();
    let two = WorkerId::new("two").unwrap();
    let (one, two) = tokio::join!(
        db.store
            .claim_artifact(&organization, &first.commit_id, &one),
        db.store
            .claim_artifact(&organization, &first.commit_id, &two)
    );
    assert!(matches!(
        (one.unwrap(), two.unwrap()),
        (Some(_), None) | (None, Some(_))
    ));
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .revision,
        input.expected_revision + 2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM events WHERE kind='artifact.sealing'")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        1
    );
}
