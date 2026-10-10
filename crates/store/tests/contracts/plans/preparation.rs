use super::*;
use super::{runtime::runtime_fixture_with_quota, starts::grants};
use agent_computer_storage::{Manifest, Prepared};
use agent_computer_store::{
    Error,
    reconciliation::*,
    runtime::{preparation::*, *},
};
use serde_json::json;

pub(super) async fn setup() -> (Database, String, String, StartReceipt, PreparationTarget) {
    setup_with_quota(10 * 1024 * 1024 * 1024).await
}
pub(super) async fn setup_with_quota(
    quota: i64,
) -> (Database, String, String, StartReceipt, PreparationTarget) {
    let (db, _, token, computer, _) = runtime_fixture_with_quota(quota).await;
    grants(&db).await;
    let ClaimOutcome::Claimed(volume) = db
        .store
        .claim_reconciliation_kind(
            &org("acme"),
            &WorkerId::new("volume-worker").unwrap(),
            Duration::from_secs(180),
            DefinitionKind::Volume,
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let target = PreparationTarget {
        volume_id: volume.task().resource_id.clone(),
        namespace_uid: "namespace-uid".into(),
        pvc_uid: "pvc-uid".into(),
        pv_uid: "pv-uid".into(),
        filesystem_uuid: "filesystem-uuid".into(),
        volume_path: "volume-path".into(),
        writer_uid: 1000,
        writer_gid: 1000,
    };
    db.store
        .begin_reconciliation_dispatch(&volume)
        .await
        .unwrap();
    for (role, uid) in [("pvc", &target.pvc_uid), ("pv", &target.pv_uid)] {
        db.store
            .record_reconciliation_object(
                &volume,
                role,
                &ReconcileObject {
                    backend: "kubernetes_juicefs".into(),
                    name: format!("{role}-name"),
                    uid: uid.clone(),
                    scope_uid: target.namespace_uid.clone(),
                },
            )
            .await
            .unwrap();
    }
    db.store
        .finish_reconciliation(
            &volume,
            ReconcileOutcome::Applied {
                receipt: EffectReceipt {
                    step_id: volume.task().step_id.clone(),
                    resource_id: target.volume_id.clone(),
                    revision: volume.task().revision,
                    spec_digest: volume.task().spec_digest.clone(),
                    backend: "kubernetes_juicefs".into(),
                    object_uid: target.pvc_uid.clone(),
                    evidence_id: target.pv_uid.clone(),
                },
            },
        )
        .await
        .unwrap();
    let receipt = db
        .store
        .admit_computer_start(
            &token,
            &key("start"),
            &computer,
            &StartRequest {
                expected_revision: 1,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
            },
        )
        .await
        .unwrap();
    (db, token, computer, receipt, target)
}
pub(super) async fn claim(
    db: &Database,
    start: &StartReceipt,
    target: &PreparationTarget,
    owner: &str,
) -> PreparationLease {
    match db
        .store
        .claim_candidate_preparation(
            &org("acme"),
            &start.request_id,
            &WorkerId::new(owner).unwrap(),
            target,
        )
        .await
        .unwrap()
    {
        PreparationClaim::Claimed(lease) => *lease,
        other => panic!("{other:?}"),
    }
}
pub(super) fn evidence(lease: &PreparationLease) -> Prepared {
    // Synthetic adapter evidence tests only database binding; not real storage.
    let target = lease.target();
    let req = lease.request();
    Prepared {
        version: 1,
        request_digest: req
            .binding_digest(&target.volume_path, target.writer_uid, target.writer_gid)
            .unwrap(),
        filesystem_uuid: target.filesystem_uuid.clone(),
        volume_uid: target.pvc_uid.clone(),
        path_ref: req.path_ref(),
        data_inode: 123,
        manifest_digest: req.manifest_digest.clone(),
        quota_bytes: req.quota_bytes,
    }
}
async fn expire(db: &Database) {
    sqlx::query("UPDATE candidate_preparations SET lease_until_ms=1")
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn input_and_volume_are_bound_before_dispatch_and_completion_survives_restart() {
    let (mut db, token, computer, start, target) = setup().await;
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    assert_eq!(count(&db, "runtime_start_inputs").await, 1);
    assert_eq!(start.input_revision, Some(1));
    assert_eq!(
        start.input_manifest_digest,
        Some(Manifest::default().digest().unwrap())
    );
    let lease = claim(&db, &start, &target, "one").await;
    assert_eq!(lease.request().manifest, Manifest::default());
    assert_eq!(lease.request().candidate, start.candidate_id);
    assert_eq!(lease.request().generation, start.generation as u64);
    assert_eq!(lease.mode(), ClaimMode::Execute);
    let worker = WorkerId::new("two").unwrap();
    assert!(matches!(
        db.store
            .claim_candidate_preparation(&org("acme"), &start.request_id, &worker, &target)
            .await
            .unwrap(),
        PreparationClaim::Busy
    ));
    let prepared = evidence(&lease);
    assert!(matches!(
        db.store
            .finish_candidate_preparation(&lease, &prepared)
            .await,
        Err(Error::InvalidReconcileResult)
    ));
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    assert!(matches!(
        db.store.begin_candidate_preparation(&lease).await,
        Err(Error::DispatchAlreadyStarted)
    ));
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Preparing)
    );
    db.store
        .finish_candidate_preparation(&lease, &prepared)
        .await
        .unwrap();
    db.crash_and_restart().await;
    let before = count(&db, "events").await;
    db.store
        .finish_candidate_preparation(&lease, &prepared)
        .await
        .unwrap();
    assert_eq!(count(&db, "events").await, before);
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(
        (current.revision, current.generation, current.ready),
        (4, 1, false)
    );
    assert_eq!(current.start_state, Some(StartState::Prepared));
    assert!(
        matches!(db.store.claim_candidate_preparation(&org("acme"),&start.request_id,&worker,&target).await.unwrap(),PreparationClaim::Prepared(r) if r==prepared)
    );
    assert!(
        sqlx::query("UPDATE workspace_input_versions SET manifest='{}'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE runtime_start_inputs SET revision=2")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE candidate_preparations SET binding='{}'")
            .execute(&db.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn stale_preparation_can_only_observe_after_dispatch_and_cannot_replace_identity() {
    let (db, _, _, start, target) = setup().await;
    let first = claim(&db, &start, &target, "one").await;
    expire(&db).await;
    let second = claim(&db, &start, &target, "two").await;
    assert_eq!(second.mode(), ClaimMode::Execute);
    assert!(matches!(
        db.store.begin_candidate_preparation(&first).await,
        Err(Error::StaleReconcileLease)
    ));
    db.store.begin_candidate_preparation(&second).await.unwrap();
    expire(&db).await;
    let third = claim(&db, &start, &target, "three").await;
    assert_eq!(third.mode(), ClaimMode::Observe);
    assert!(matches!(
        db.store.begin_candidate_preparation(&third).await,
        Err(Error::DispatchAlreadyStarted)
    ));
    assert!(matches!(
        db.store
            .finish_candidate_preparation(&second, &evidence(&second))
            .await,
        Err(Error::StaleReconcileLease)
    ));
    db.store.defer_candidate_preparation(&third).await.unwrap();
    let fourth = claim(&db, &start, &target, "four").await;
    assert_eq!(fourth.mode(), ClaimMode::Observe);
    assert_eq!(fourth.request(), first.request());
    assert!(matches!(
        db.store
            .finish_candidate_preparation(&third, &evidence(&third))
            .await,
        Err(Error::StaleReconcileLease)
    ));
    let changed = PreparationTarget {
        volume_path: "other-path".into(),
        ..target.clone()
    };
    assert!(matches!(
        db.store
            .claim_candidate_preparation(
                &org("acme"),
                &start.request_id,
                &WorkerId::new("bad").unwrap(),
                &changed
            )
            .await,
        Err(Error::IdempotencyConflict)
    ));
}

#[tokio::test]
async fn completion_rejects_all_changed_receipt_bindings_and_retains_capacity() {
    let (db, token, computer, start, target) = setup().await;
    let lease = claim(&db, &start, &target, "one").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    let correct = serde_json::to_value(evidence(&lease)).unwrap();
    for (field, value) in [
        ("version", json!(2)),
        ("request_digest", json!("wrong")),
        ("filesystem_uuid", json!("other")),
        ("volume_uid", json!("other")),
        ("path_ref", json!("other")),
        ("data_inode", json!(0)),
        ("manifest_digest", json!("wrong")),
        ("quota_bytes", json!(1)),
    ] {
        let mut changed = correct.clone();
        changed[field] = value;
        assert!(matches!(
            db.store
                .finish_candidate_preparation(&lease, &serde_json::from_value(changed).unwrap())
                .await,
            Err(Error::InvalidReconcileResult)
        ));
    }
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(current.start_state, Some(StartState::Preparing));
    assert!(matches!(
        db.store
            .cancel_queued_computer_start(
                &token,
                &key("cancel"),
                &computer,
                &CancelQueuedStart {
                    expected_revision: current.revision,
                    request_id: start.request_id
                }
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    assert!(!current.ready);
}

#[tokio::test]
async fn preparation_rechecks_grants_and_original_credential_at_each_boundary() {
    let (db, _, _, start, target) = setup().await;
    let lease = claim(&db, &start, &target, "one").await;
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Workspace,
                resource_id: &lease.request().workspace,
                permission: RuntimePermission::Modify,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store.begin_candidate_preparation(&lease).await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    super::runtime::allow(
        &db,
        &lease.request().workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Modify,
        None,
    )
    .await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    let credential: String = sqlx::query_scalar("SELECT credential_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    db.store
        .revoke_credential(&org("acme"), &credential)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .finish_candidate_preparation(&lease, &evidence(&lease))
            .await,
        Err(Error::Unauthenticated)
    ));
    assert!(matches!(
        db.store.defer_candidate_preparation(&lease).await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM candidate_preparations WHERE receipt IS NOT NULL"
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn queued_cancellation_wins_over_a_claim_and_wrong_volume_evidence_is_rejected() {
    let (db, token, computer, start, target) = setup().await;
    for changed in [
        PreparationTarget {
            pvc_uid: "replacement".into(),
            ..target.clone()
        },
        PreparationTarget {
            pv_uid: "replacement".into(),
            ..target.clone()
        },
        PreparationTarget {
            namespace_uid: "replacement".into(),
            ..target.clone()
        },
    ] {
        assert!(matches!(
            db.store
                .claim_candidate_preparation(
                    &org("acme"),
                    &start.request_id,
                    &WorkerId::new("one").unwrap(),
                    &changed
                )
                .await,
            Err(Error::ReferenceUnavailable)
        ));
    }
    assert_eq!(count(&db, "candidate_preparations").await, 0);
    let lease = claim(&db, &start, &target, "one").await;
    db.store
        .cancel_queued_computer_start(
            &token,
            &key("cancel"),
            &computer,
            &CancelQueuedStart {
                expected_revision: 2,
                request_id: start.request_id,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store.begin_candidate_preparation(&lease).await,
        Err(Error::RuntimeConflict)
    ));
}

#[tokio::test]
async fn failed_completion_or_late_expiry_rolls_back_receipt_state_and_outbox() {
    let (db, token, computer, start, target) = setup().await;
    let lease = claim(&db, &start, &target, "one").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_prepared() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.receipt IS NOT NULL THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_prepared BEFORE UPDATE ON candidate_preparations FOR EACH ROW EXECUTE FUNCTION reject_prepared();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .finish_candidate_preparation(&lease, &evidence(&lease))
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Preparing)
    );
    sqlx::raw_sql("DROP TRIGGER reject_prepared ON candidate_preparations; CREATE FUNCTION expire_prepared() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.receipt IS NOT NULL THEN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.activate'=ANY(scopes); PERFORM pg_sleep(0.01); END IF; RETURN NEW; END $$; CREATE TRIGGER expire_prepared BEFORE UPDATE ON candidate_preparations FOR EACH ROW EXECUTE FUNCTION expire_prepared();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .finish_candidate_preparation(&lease, &evidence(&lease))
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "events").await, before);
    sqlx::query("DROP TRIGGER expire_prepared ON candidate_preparations")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .finish_candidate_preparation(&lease, &evidence(&lease))
        .await
        .unwrap();
}

#[tokio::test]
async fn upgrade_never_infers_empty_inputs_for_legacy_workspaces_or_queued_requests() {
    let (db, token, computer, start, target) = setup().await;
    db.remove_execution_admission().await;
    sqlx::raw_sql("DROP TABLE candidate_writer_completions; DROP FUNCTION guard_writer_completion(); DROP TABLE candidate_writer_drains,candidate_writer_dispatches,candidate_writer_epochs,candidate_writer_leases; DROP FUNCTION guard_writer_record_insert(); DROP FUNCTION guard_writer_lease_mutation(); DROP TABLE connection_sessions; DROP FUNCTION guard_connection_session_mutation(); DROP TABLE candidate_preparations,runtime_start_inputs,workspace_input_heads,workspace_input_versions; DROP FUNCTION guard_candidate_preparation(); DELETE FROM _sqlx_migrations WHERE version>=8;").execute(&db.pool).await.unwrap();
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "workspace_input_versions").await, 0);
    assert_eq!(count(&db, "runtime_start_inputs").await, 0);
    assert!(matches!(
        db.store
            .claim_candidate_preparation(
                &org("acme"),
                &start.request_id,
                &WorkerId::new("one").unwrap(),
                &target
            )
            .await,
        Err(Error::WorkspaceInputUnavailable)
    ));
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    db.store
        .initialize_empty_workspace(&org("acme"), &workspace)
        .await
        .unwrap();
    let events = count(&db, "events").await;
    db.store
        .initialize_empty_workspace(&org("acme"), &workspace)
        .await
        .unwrap();
    assert_eq!(count(&db, "events").await, events);
    assert!(matches!(
        db.store
            .claim_candidate_preparation(
                &org("acme"),
                &start.request_id,
                &WorkerId::new("one").unwrap(),
                &target
            )
            .await,
        Err(Error::WorkspaceInputUnavailable)
    ));
    db.store
        .cancel_queued_computer_start(
            &token,
            &key("cancel"),
            &computer,
            &CancelQueuedStart {
                expected_revision: 2,
                request_id: start.request_id,
            },
        )
        .await
        .unwrap();
    db.store
        .admit_computer_start(
            &token,
            &key("new"),
            &computer,
            &StartRequest {
                expected_revision: 3,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
            },
        )
        .await
        .unwrap();
    assert_eq!(count(&db, "runtime_start_inputs").await, 1);
}
