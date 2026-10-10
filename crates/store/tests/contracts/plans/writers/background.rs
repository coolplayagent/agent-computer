use super::executions::{submission, submit};
use super::*;
use serde_json::json;

async fn background(db: &Database, lease: &WriterLease) -> SubmitExecution {
    let mut input = serde_json::to_value(submission(db, lease).await).unwrap();
    input.as_object_mut().unwrap().remove("lifetime");
    serde_json::from_value(input).unwrap()
}

#[tokio::test]
async fn background_queue_survives_disconnect_and_wal_without_new_writer_authority() {
    let (mut db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = background(&db, &lease).await;
    assert_eq!(input.lifetime, ExecutionLifetime::Background);
    let queued = submit(&db, &token, &computer, &input).await;
    assert_eq!(queued.lifetime, ExecutionLifetime::Background);
    let closed = db
        .store
        .close_connection_session(&token, &lease.connection_session_id)
        .await
        .unwrap();
    assert_eq!(closed.state, ConnectionState::Closed);
    assert!(closed.capabilities.is_empty());
    let held = db
        .store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(held.state, WriterLeaseState::Held);
    assert_eq!(held.expires_at_ms, lease.expires_at_ms);
    assert!(
        db.store
            .renew_candidate_writer(
                &token,
                &key("renew-closed"),
                &lease.lease_id,
                &RenewWriterLease {
                    lease: command(&held),
                    duration_seconds: 30
                }
            )
            .await
            .is_err()
    );
    assert!(
        db.store
            .begin_candidate_writer_dispatch(
                &token,
                &held.lease_id,
                &command(&held),
                WriterDispatch {
                    dispatch_id: "closed-file",
                    input_digest: &format!("sha256:{}", "a".repeat(64))
                }
            )
            .await
            .is_err()
    );
    assert!(
        db.store
            .submit_candidate_execution(&token, &key("closed-new"), &computer, &input)
            .await
            .is_err()
    );
    let other = connection(&db, &token, &computer, "reconnect", 900).await;
    assert!(matches!(
        db.store
            .acquire_candidate_writer(
                &token,
                &key("reconnect"),
                &computer,
                &AcquireWriterLease {
                    connection_session_id: other.session_id,
                    ..acquire_input
                }
            )
            .await,
        Err(Error::WriterLeaseBusy)
    ));
    let events = count(&db, "events").await;
    db.crash_and_restart().await;
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap(),
        queued
    );
    assert_eq!(submit(&db, &token, &computer, &input).await, queued);
    assert_eq!(count(&db, "events").await, events);
    let changed = SubmitExecution {
        lifetime: ExecutionLifetime::Connection,
        ..input
    };
    assert!(matches!(
        db.store
            .submit_candidate_execution(&token, &key("submit"), &computer, &changed)
            .await,
        Err(Error::IdempotencyConflict)
    ));
    let attempt = db
        .store
        .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, queued.revision)
        .await
        .unwrap();
    assert_eq!(attempt.intent().deadline_at_ms, lease.expires_at_ms);
    assert!(attempt.remaining_budget_ms().unwrap() <= 30_000);
    assert_eq!(
        attempt.intent().execution.lifetime,
        ExecutionLifetime::Background
    );
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn execution_disconnect_follows_the_immutable_submitted_lifetime() {
    for lifetime in [ExecutionLifetime::Connection, ExecutionLifetime::Background] {
        let (db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let input = SubmitExecution {
            lifetime,
            ..submission(&db, &lease).await
        };
        let queued = submit(&db, &token, &computer, &input).await;
        let attempt = db
            .store
            .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
            .await
            .unwrap();
        db.store
            .close_connection_session(&token, &lease.connection_session_id)
            .await
            .unwrap();
        let current = db
            .store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap();
        assert_eq!(
            current.state,
            if lifetime == ExecutionLifetime::Background {
                ExecutionState::Dispatching
            } else {
                ExecutionState::Unknown
            }
        );
        assert_eq!(current.queue_deadline_at_ms, queued.queue_deadline_at_ms);
        assert!(sqlx::query("UPDATE execution_requests SET input=jsonb_set(input,'{lifetime}','\"connection\"'::jsonb),revision=revision+1").execute(&db.pool).await.is_err());
        if lifetime == ExecutionLifetime::Background {
            let cancel = db
                .store
                .cancel_candidate_execution(
                    &token,
                    &key("cancel-closed-background"),
                    &queued.execution_id,
                    &CancelExecution {
                        expected_revision: current.revision,
                    },
                )
                .await
                .unwrap();
            assert_eq!(cancel.state, ExecutionState::CancelRequested);
            assert_eq!(
                db.store
                    .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
                    .await
                    .unwrap()
                    .state,
                ExecutionState::CancelRequested
            );
        }
        assert_eq!(count(&db, "candidate_writer_drains").await, 0);
        assert!(
            db.store
                .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
                .await
                .is_err()
        );
        drop(attempt);
    }
}

#[tokio::test]
async fn background_disconnect_never_bypasses_revocation_or_catalog_authority() {
    for revoke in ["credential", "principal", "grant", "catalog"] {
        let (db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let queued = submit(&db, &token, &computer, &background(&db, &lease).await).await;
        let attempt = db
            .store
            .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
            .await
            .unwrap();
        db.store
            .close_connection_session(&token, &lease.connection_session_id)
            .await
            .unwrap();
        match revoke {
            "credential" => {
                sqlx::query("UPDATE service_credentials SET revoked=true")
                    .execute(&db.pool)
                    .await
                    .unwrap();
            }
            "principal" => {
                sqlx::query("UPDATE principals SET enabled=false")
                    .execute(&db.pool)
                    .await
                    .unwrap();
            }
            "grant" => {
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
            }
            _ => {
                sqlx::query("UPDATE catalog_references SET enabled=false")
                    .execute(&db.pool)
                    .await
                    .unwrap();
            }
        }
        assert_eq!(
            db.store
                .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
                .await
                .unwrap()
                .state,
            ExecutionState::Unknown,
            "{revoke}"
        );
        assert_eq!(
            db.store
                .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
                .await
                .unwrap()
                .state,
            WriterLeaseState::Draining
        );
        assert_eq!(count(&db, "candidate_writer_drains").await, 0);
        drop(attempt);
    }
}

#[tokio::test]
async fn cancelled_background_reservation_does_not_preserve_a_closed_writer() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let queued = submit(&db, &token, &computer, &background(&db, &lease).await).await;
    db.store
        .cancel_candidate_execution(
            &token,
            &key("cancel"),
            &queued.execution_id,
            &CancelExecution {
                expected_revision: 1,
            },
        )
        .await
        .unwrap();
    db.store
        .close_connection_session(&token, &lease.connection_session_id)
        .await
        .unwrap();
    let released = db
        .store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(released.state, WriterLeaseState::Released);
    assert_eq!(released.release_proof.as_deref(), Some("no_dispatch"));
    let new = connection(&db, &token, &computer, "new", 900).await;
    let next = db
        .store
        .acquire_candidate_writer(
            &token,
            &key("next"),
            &computer,
            &AcquireWriterLease {
                connection_session_id: new.session_id.clone(),
                ..acquire_input
            },
        )
        .await
        .unwrap();
    assert_eq!(next.epoch, lease.epoch + 1);
    db.store
        .close_connection_session(&token, &new.session_id)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &next.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Released
    );
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
}

#[tokio::test]
async fn background_disconnect_is_atomic_with_outbox_and_does_not_renew_any_deadline() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let queued = submit(&db, &token, &computer, &background(&db, &lease).await).await;
    let events = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_background_close() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected close failure'; END $$; CREATE TRIGGER reject_background_close BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_background_close();").execute(&db.pool).await.unwrap();
    assert!(
        db.store
            .close_connection_session(&token, &lease.connection_session_id)
            .await
            .is_err()
    );
    assert_eq!(count(&db, "events").await, events);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM connection_sessions")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        "Active"
    );
    sqlx::query("DROP TRIGGER reject_background_close ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .close_connection_session(&token, &lease.connection_session_id)
        .await
        .unwrap();
    let current = db
        .store
        .candidate_execution(&token, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(current, queued);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .expires_at_ms,
        lease.expires_at_ms
    );
}

#[tokio::test]
async fn background_execution_expiry_cannot_be_renewed_by_reconnect_or_retry() {
    for dispatch in [false, true] {
        let (db, token, computer, mut acquire_input) = setup().await;
        acquire_input.duration_seconds = 2;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let input = background(&db, &lease).await;
        let queued = submit(&db, &token, &computer, &input).await;
        let attempt = if dispatch {
            Some(
                db.store
                    .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        db.store
            .close_connection_session(&token, &lease.connection_session_id)
            .await
            .unwrap();
        connection(&db, &token, &computer, "new", 900).await;
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        let expired = db
            .store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap();
        assert_eq!(
            expired.state,
            if dispatch {
                ExecutionState::Unknown
            } else {
                ExecutionState::Cancelled
            }
        );
        assert_eq!(submit(&db, &token, &computer, &input).await, expired);
        assert_eq!(expired.queue_deadline_at_ms, lease.expires_at_ms);
        let writer = db
            .store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap();
        assert_eq!(
            writer.state,
            if dispatch {
                WriterLeaseState::Draining
            } else {
                WriterLeaseState::Released
            }
        );
        drop(attempt);
    }
}

#[tokio::test]
async fn migration_twenty_eight_preserves_explicit_connection_inputs_and_metadata() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    db.remove_background_execution().await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(e) FROM execution_requests e")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(
        before,
        sqlx::query_scalar::<_, serde_json::Value>("SELECT to_jsonb(e) FROM execution_requests e")
            .fetch_one(&db.pool)
            .await
            .unwrap()
    );
    assert_eq!(submit(&db, &token, &computer, &input).await, queued);
    let mut historical = serde_json::to_value(&queued).unwrap();
    historical.as_object_mut().unwrap().remove("lifetime");
    assert_eq!(
        serde_json::from_value::<ExecutionRequest>(historical)
            .unwrap()
            .lifetime,
        ExecutionLifetime::Connection
    );
    assert!(serde_json::from_value::<SubmitExecution>(json!({"lifetime":"unbounded"})).is_err());
}

#[tokio::test]
async fn checkpoint_stop_cancels_detached_background_but_still_waits_for_physical_proof() {
    let (db, token, computer, acquire_input) = setup_with_apps(false).await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let queued = submit(&db, &token, &computer, &background(&db, &lease).await).await;
    let attempt = db
        .store
        .begin_candidate_execution_dispatch(&org("acme"), &queued.execution_id, 1)
        .await
        .unwrap();
    db.store
        .close_connection_session(&token, &lease.connection_session_id)
        .await
        .unwrap();
    let runtime = db.store.computer_runtime(&token, &computer).await.unwrap();
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Manage,
        None,
    )
    .await;
    allow(
        &db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Publish,
        None,
    )
    .await;
    let pending = db
        .store
        .checkpoint_stop_computer(
            &token,
            &key("stop-background"),
            &computer,
            &agent_computer_store::runtime::artifacts::CheckpointStop {
                request_id: runtime.active_request.unwrap(),
                expected_revision: runtime.revision,
                publish_current: true,
                cancel_running: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        pending.state,
        agent_computer_store::runtime::artifacts::ArtifactState::Draining
    );
    assert_eq!(
        db.store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::CancelRequested
    );
    assert!(
        db.store
            .claim_artifact(
                &org("acme"),
                &pending.commit_id,
                &agent_computer_store::reconciliation::WorkerId::new("background-stop").unwrap()
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert_eq!(count(&db, "runtime_stops").await, 0);
    drop(attempt);
}
