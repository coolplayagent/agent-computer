//! Synthetic adapter inputs: these contracts exercise PostgreSQL, not Kubernetes.
use super::*;
use agent_computer_sandbox::StartupChallenge;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(super) fn manifest(attempt: &ExecutionDispatchAttempt) -> Value {
    let dispatch = attempt.intent();
    let execution = &dispatch.execution;
    let key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(&dispatch.organization, &execution.execution_id)).unwrap()
        )
    );
    let bootstrap = dispatch.bootstrap().unwrap();
    let binding = json!({"version":2,"namespace":"runtime", "bootstrap":bootstrap,
        "identity":{"organization":dispatch.organization,"computer":execution.computer_id,"sandbox":execution.sandbox_id,
            "instance":execution.execution_id,"generation":execution.generation,"spec_revision":execution.sandbox_revision},
        "workspace":{"kind":"prepared_candidate","version":1,"organization":dispatch.organization,"computer":execution.computer_id,
            "candidate":execution.candidate_id,"generation":execution.generation,"namespace_uid":"namespace-uid",
            "pv_uid":"pv-uid","volume_path":"volume-path","prepared":dispatch.binding["prepared"]}});
    json!({"kind":"Pod","apiVersion":"v1","metadata":{"name":format!("ac-{}",&key[..52]),"namespace":"runtime",
        "annotations":{"agent-computer.io/binding":binding.to_string()}},
        "spec":{"containers":[{"command":["/bin/agent-computer-sandbox","--attach-startup-json",serde_json::to_string(&bootstrap).unwrap()]}]}})
}
pub(super) fn challenge(attempt: &ExecutionDispatchAttempt) -> StartupChallenge {
    let bootstrap = attempt.intent().bootstrap().unwrap();
    StartupChallenge {
        version: bootstrap.version,
        execution_id: bootstrap.request.execution_id.clone(),
        generation: bootstrap.request.generation,
        bootstrap_digest: bootstrap.digest().unwrap(),
        nonce: "a".repeat(64),
    }
}
pub(super) async fn register(
    db: &Database,
    attempt: &ExecutionDispatchAttempt,
) -> ExecutionPodAttempt {
    db.store
        .register_candidate_execution_pod(attempt, "namespace-uid", &manifest(attempt))
        .await
        .unwrap()
}

#[tokio::test]
async fn pod_plan_has_one_creation_attempt_and_survives_lost_response_and_wal_recovery() {
    let (mut db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let manifest = manifest(&attempt);
    let (a, b) = tokio::join!(
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest),
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest)
    );
    let issued = match (a, b) {
        (Ok(a), Err(Error::DispatchAlreadyStarted))
        | (Err(Error::DispatchAlreadyStarted), Ok(a)) => a,
        other => panic!("{other:?}"),
    };
    assert!(issued.remaining_budget_ms().unwrap() <= 30000);
    assert_eq!(issued.plan().manifest, manifest);
    assert!(issued.plan().pod_uid.is_none());
    let saved = serde_json::to_value(issued.plan()).unwrap();
    drop(issued);
    let before = count(&db, "events").await;
    db.crash_and_restart().await;
    assert_eq!(
        serde_json::to_value(
            db.store
                .candidate_execution_pod(&org("acme"), &queued.execution_id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        saved
    );
    assert!(matches!(
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest)
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
    assert!(matches!(
        db.store
            .candidate_execution_pod(&org("other"), &queued.execution_id)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "execution_pod_plans").await, 1);
    assert_eq!(count(&db, "execution_pod_observations").await, 0);
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    let event: Value =
        sqlx::query_scalar("SELECT payload FROM events WHERE kind='execution.pod_planned'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(event["pod_creation_confirmed"], false);
    assert!(!event.to_string().contains("literal-private-command"));
    assert!(!event.to_string().contains("volume-path"));
    for sql in [
        "UPDATE execution_pod_plans SET namespace='replacement'",
        "DELETE FROM execution_pod_plans",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
}

#[tokio::test]
async fn pod_uid_observation_is_unique_durable_and_required_for_registered_startup() {
    let (mut db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let plan = register(&db, &attempt).await;
    let challenge = challenge(&attempt);
    let rejected = sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) SELECT organization,execution_id,'pod-one',$1,'{\"version\":1,\"lease_budget_ms\":1}'::jsonb,$2,floor(extract(epoch from clock_timestamp())*1000) FROM execution_requests")
        .bind(serde_json::to_value(&challenge).unwrap()).bind(format!("sha256:{}","a".repeat(64))).execute(&db.pool).await.unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("execution Pod UID is not recorded")
    );
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &org("acme"),
                &queued.execution_id,
                2,
                "pod-one",
                &challenge
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    assert!(matches!(
        db.store
            .record_candidate_execution_pod(
                &org("acme"),
                &queued.execution_id,
                &format!("sha256:{}", "0".repeat(64)),
                "pod-one"
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    let organization = org("acme");
    let (a, b) = tokio::join!(
        db.store.record_candidate_execution_pod(
            &organization,
            &queued.execution_id,
            &plan.plan().plan_digest,
            "pod-one"
        ),
        db.store.record_candidate_execution_pod(
            &organization,
            &queued.execution_id,
            &plan.plan().plan_digest,
            "pod-two"
        )
    );
    let observed = match (a, b) {
        (Ok(a), Err(Error::RuntimeConflict)) | (Err(Error::RuntimeConflict), Ok(a)) => a,
        other => panic!("{other:?}"),
    };
    let uid = observed.pod_uid.as_deref().unwrap();
    let before = count(&db, "events").await;
    db.crash_and_restart().await;
    let again = db
        .store
        .record_candidate_execution_pod(
            &organization,
            &queued.execution_id,
            &observed.plan_digest,
            uid,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&again).unwrap(),
        serde_json::to_value(&observed).unwrap()
    );
    assert_eq!(count(&db, "events").await, before);
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &organization,
                &queued.execution_id,
                2,
                "replacement",
                &challenge
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    let rejected = db
        .store
        .authorize_candidate_execution_startup(
            &organization,
            &queued.execution_id,
            2,
            uid,
            &challenge,
        )
        .await
        .unwrap_err();
    assert!(matches!(rejected, Error::RuntimeAccessUnavailable));
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    let rejected = sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) SELECT organization,execution_id,$1,$2,'{\"version\":1,\"lease_budget_ms\":1}'::jsonb,$3,floor(extract(epoch from clock_timestamp())*1000) FROM execution_requests")
        .bind(uid).bind(serde_json::to_value(&challenge).unwrap()).bind(format!("sha256:{}","a".repeat(64))).execute(&db.pool).await.unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("execution watchdog is not armed")
    );
    assert!(
        sqlx::query("UPDATE execution_pod_observations SET pod_uid='replacement'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM execution_pod_observations")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&organization, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Held
    );
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn pod_plan_rejects_transplanted_identity_bootstrap_namespace_and_prepared_receipt() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let good = manifest(&attempt);
    for (pointer, value) in [
        ("/metadata/name", json!(format!("ac-{}", "0".repeat(52)))),
        ("/metadata/namespace", json!("runtime-")),
        ("/kind", Value::Null),
        ("/spec/containers/0/command/2", json!("{}")),
    ] {
        let mut changed = good.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(
            matches!(
                db.store
                    .register_candidate_execution_pod(&attempt, "namespace-uid", &changed)
                    .await,
                Err(Error::InvalidRuntimeRequest)
            ),
            "{pointer}"
        );
    }
    for (pointer, value) in [
        ("/identity/instance", json!("foreign")),
        ("/identity/generation", json!(2)),
        ("/bootstrap/request/argv", json!(["/bin/false"])),
        ("/workspace/prepared/data_inode", json!(999)),
        ("/workspace/namespace_uid", json!("replacement")),
        ("/workspace/candidate", json!("foreign")),
    ] {
        let mut changed = good.clone();
        let mut binding: Value = serde_json::from_str(
            changed["metadata"]["annotations"]["agent-computer.io/binding"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        *binding.pointer_mut(pointer).unwrap() = value;
        changed["metadata"]["annotations"]["agent-computer.io/binding"] =
            json!(binding.to_string());
        assert!(
            matches!(
                db.store
                    .register_candidate_execution_pod(&attempt, "namespace-uid", &changed)
                    .await,
                Err(Error::InvalidRuntimeRequest)
            ),
            "{pointer}"
        );
    }
    assert!(matches!(
        db.store
            .register_candidate_execution_pod(&attempt, "replacement", &good)
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
    let mut oversized = good.clone();
    oversized["padding"] = json!("a".repeat(262_144));
    assert!(matches!(
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &oversized)
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
    assert_eq!(count(&db, "execution_pod_plans").await, 0);
    let mut missing_kind = good.clone();
    missing_kind.as_object_mut().unwrap().remove("kind");
    let rejected=sqlx::query("INSERT INTO execution_pod_plans (organization,execution_id,namespace_uid,namespace,pod_name,plan_digest,manifest) SELECT organization,execution_id,'namespace-uid','runtime',$1,$2,$3 FROM execution_requests")
        .bind(good["metadata"]["name"].as_str().unwrap()).bind(format!("sha256:{}","a".repeat(64))).bind(missing_kind).execute(&db.pool).await.unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("execution Pod plan is not admitted")
    );
    register(&db, &attempt).await;
}

#[tokio::test]
async fn pod_journal_and_observation_outbox_failures_roll_back_atomically() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_pod_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END; $$; CREATE TRIGGER reject_pod_event BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_pod_event();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest(&attempt))
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "execution_pod_plans").await, 0);
    assert_eq!(count(&db, "events").await, before);
    sqlx::query("DROP TRIGGER reject_pod_event ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    let plan = register(&db, &attempt).await;
    sqlx::query("CREATE TRIGGER reject_pod_event BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_pod_event()").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .record_candidate_execution_pod(
                &org("acme"),
                &queued.execution_id,
                &plan.plan().plan_digest,
                "pod-one"
            )
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "execution_pod_observations").await, 0);
    assert_eq!(count(&db, "events").await, before + 1);
    sqlx::query("DROP TRIGGER reject_pod_event ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .record_candidate_execution_pod(
            &org("acme"),
            &queued.execution_id,
            &plan.plan().plan_digest,
            "pod-one",
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn pod_plan_rechecks_revoked_authority_and_expiry_after_journal_writes() {
    for sql in [
        "UPDATE service_credentials SET revoked=true WHERE 'runtime.modify'=ANY(scopes)",
        "UPDATE catalog_references SET enabled=false WHERE kind='storage_class'",
        "DELETE FROM runtime_grants WHERE kind='workspace' AND permission='modify'",
    ] {
        let (db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let input = submission(&db, &lease).await;
        let queued = submit(&db, &token, &computer, &input).await;
        let attempt = begin(&db, &queued).await;
        sqlx::query(sql).execute(&db.pool).await.unwrap();
        assert!(
            matches!(
                db.store
                    .register_candidate_execution_pod(
                        &attempt,
                        "namespace-uid",
                        &manifest(&attempt)
                    )
                    .await,
                Err(Error::WriterLeaseInactive)
            ),
            "{sql}"
        );
        assert_eq!(count(&db, "execution_pod_plans").await, 0);
        assert_eq!(
            db.store
                .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
                .await
                .unwrap()
                .state,
            ExecutionState::Unknown
        );
        assert_eq!(
            db.store
                .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
                .await
                .unwrap()
                .state,
            WriterLeaseState::Draining
        );
    }
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION expire_pod_plan() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.modify'=ANY(scopes); RETURN NEW; END; $$; CREATE TRIGGER expire_pod_plan AFTER INSERT ON execution_pod_plans FOR EACH ROW EXECUTE FUNCTION expire_pod_plan();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest(&attempt))
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "execution_pod_plans").await, 0);
    assert_eq!(count(&db, "events").await, before);
}

#[tokio::test]
async fn pod_observation_after_expiry_does_not_renew_budget_or_release_writer() {
    let (db, token, computer, mut acquire_input) = setup().await;
    acquire_input.duration_seconds = 1;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let plan = register(&db, &attempt).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(matches!(
        plan.remaining_budget_ms(),
        Err(Error::WriterLeaseInactive)
    ));
    let unknown = db
        .store
        .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(unknown.state, ExecutionState::Unknown);
    db.store
        .record_candidate_execution_pod(
            &org("acme"),
            &queued.execution_id,
            &plan.plan().plan_digest,
            "pod-one",
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &org("acme"),
                &queued.execution_id,
                unknown.revision,
                "pod-one",
                &challenge(&attempt)
            )
            .await,
        Err(Error::WriterLeaseInactive)
    ));
    assert!(matches!(
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest(&attempt))
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
}

#[tokio::test]
async fn migration_fifteen_preserves_prior_grants_and_never_invents_pod_identity() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let grant = db
        .store
        .authorize_candidate_execution_startup(
            &org("acme"),
            &queued.execution_id,
            2,
            "pod-one",
            &challenge(&attempt),
        )
        .await
        .unwrap();
    db.remove_execution_pods().await;
    assert!(db.store.ready().await.is_err());
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "execution_pod_plans").await, 0);
    assert_eq!(count(&db, "execution_pod_observations").await, 0);
    assert_eq!(
        db.store
            .candidate_execution_startup(&org("acme"), &queued.execution_id)
            .await
            .unwrap()
            .unwrap()
            .grant_digest,
        grant.grant().grant_digest
    );
    assert!(matches!(
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest(&attempt))
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
}
