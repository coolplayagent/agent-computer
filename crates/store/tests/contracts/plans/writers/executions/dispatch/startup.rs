use super::*;
use agent_computer_sandbox::StartupChallenge;

fn challenge(attempt: &ExecutionDispatchAttempt) -> StartupChallenge {
    let bootstrap = attempt.intent().bootstrap().unwrap();
    StartupChallenge {
        version: 1,
        execution_id: bootstrap.request.execution_id.clone(),
        generation: bootstrap.request.generation,
        bootstrap_digest: bootstrap.digest().unwrap(),
        nonce: "a".repeat(64),
    }
}
async fn grant(db: &Database, id: &str, challenge: &StartupChallenge) -> ExecutionStartupAttempt {
    db.store
        .authorize_candidate_execution_startup(&org("acme"), id, 2, "pod-one", challenge)
        .await
        .unwrap()
}

#[tokio::test]
async fn startup_grant_is_unique_bound_and_durable_without_reporting_process_started() {
    let (mut db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let challenge = challenge(&attempt);
    assert!(
        db.store
            .candidate_execution_startup(&org("acme"), &queued.execution_id)
            .await
            .unwrap()
            .is_none()
    );
    let organization = org("acme");
    let (one, two) = tokio::join!(
        db.store.authorize_candidate_execution_startup(
            &organization,
            &queued.execution_id,
            2,
            "pod-one",
            &challenge
        ),
        db.store.authorize_candidate_execution_startup(
            &organization,
            &queued.execution_id,
            2,
            "pod-two",
            &challenge
        )
    );
    let issued = match (one, two) {
        (Ok(v), Err(Error::DispatchAlreadyStarted))
        | (Err(Error::DispatchAlreadyStarted), Ok(v)) => v,
        other => panic!("{other:?}"),
    };
    assert!(issued.remaining_budget_ms().unwrap() <= issued.grant().grant.lease_budget_ms);
    assert_eq!(
        issued.grant().grant.challenge_digest,
        challenge.digest().unwrap()
    );
    assert_eq!(
        i64::from(issued.grant().grant.lease_budget_ms),
        queued.queue_deadline_at_ms - issued.grant().granted_at_ms
    );
    let fixed = serde_json::to_value(issued.grant()).unwrap();
    let before = count(&db, "events").await;
    db.crash_and_restart().await;
    assert_eq!(
        serde_json::to_value(
            db.store
                .candidate_execution_startup(&organization, &queued.execution_id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        fixed
    );
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &organization,
                &queued.execution_id,
                2,
                "pod-one",
                &challenge
            )
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
    let mut changed = challenge.clone();
    changed.nonce = "b".repeat(64);
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &organization,
                &queued.execution_id,
                2,
                "replacement",
                &changed
            )
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
    assert_eq!(count(&db, "execution_startup_grants").await, 1);
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Dispatching
    );
    let event: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM events WHERE kind='execution.startup_authorized'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(event["process_started_confirmed"], false);
    assert!(!event.to_string().contains(&challenge.nonce));
    assert!(!event.to_string().contains("literal-private-command"));
    for sql in [
        "UPDATE execution_startup_grants SET pod_uid='replacement'",
        "DELETE FROM execution_startup_grants",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
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
async fn startup_rejects_transplanted_bootstrap_generation_and_stale_revision() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let good = challenge(&attempt);
    let mut changed = good.clone();
    changed.generation += 1;
    let mut foreign = good.clone();
    foreign.execution_id = "other".into();
    let mut bootstrap = good.clone();
    bootstrap.bootstrap_digest = format!("sha256:{}", "c".repeat(64));
    let mut invalid = good.clone();
    invalid.nonce = "invalid".into();
    for challenge in [changed, foreign, bootstrap, invalid] {
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
            Err(Error::InvalidRuntimeRequest)
        ));
    }
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &org("acme"),
                &queued.execution_id,
                1,
                "pod-one",
                &good
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    grant(&db, &queued.execution_id, &good).await;
}

#[tokio::test]
async fn startup_rechecks_revocation_and_cancel_after_the_original_dispatch() {
    for revoke in [true, false] {
        let (db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let input = submission(&db, &lease).await;
        let queued = submit(&db, &token, &computer, &input).await;
        let attempt = begin(&db, &queued).await;
        let challenge = challenge(&attempt);
        let revision = if revoke {
            sqlx::query(
                "UPDATE service_credentials SET revoked=true WHERE 'runtime.modify'=ANY(scopes)",
            )
            .execute(&db.pool)
            .await
            .unwrap();
            2
        } else {
            db.store
                .cancel_candidate_execution(
                    &token,
                    &key("cancel"),
                    &queued.execution_id,
                    &CancelExecution {
                        expected_revision: 2,
                    },
                )
                .await
                .unwrap()
                .revision
        };
        assert!(matches!(
            db.store
                .authorize_candidate_execution_startup(
                    &org("acme"),
                    &queued.execution_id,
                    revision,
                    "pod-one",
                    &challenge
                )
                .await,
            Err(Error::WriterLeaseInactive)
        ));
        assert_eq!(count(&db, "execution_startup_grants").await, 0);
        let current = db
            .store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap();
        assert_eq!(
            current.state,
            if revoke {
                ExecutionState::Unknown
            } else {
                ExecutionState::CancelRequested
            }
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
}

#[tokio::test]
async fn startup_fixed_deadline_survives_delayed_challenge_and_writer_renewal() {
    let (db, token, computer, mut acquire_input) = setup().await;
    acquire_input.duration_seconds = 1;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let challenge = challenge(&attempt);
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
        db.store
            .authorize_candidate_execution_startup(
                &org("acme"),
                &queued.execution_id,
                2,
                "pod-one",
                &challenge
            )
            .await,
        Err(Error::WriterLeaseInactive)
    ));
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    assert_eq!(
        db.store
            .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Unknown
    );
}

#[tokio::test]
async fn startup_grant_and_outbox_roll_back_after_failure_or_late_credential_expiry() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let challenge = challenge(&attempt);
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_startup() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END; $$; CREATE TRIGGER reject_startup BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_startup();").execute(&db.pool).await.unwrap();
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
        Err(Error::Database(_))
    ));
    sqlx::raw_sql("DROP TRIGGER reject_startup ON outbox; CREATE FUNCTION expire_startup() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.modify'=ANY(scopes); RETURN NEW; END; $$; CREATE TRIGGER expire_startup AFTER INSERT ON execution_startup_grants FOR EACH ROW EXECUTE FUNCTION expire_startup();").execute(&db.pool).await.unwrap();
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
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    assert_eq!(
        db.store
            .candidate_execution(&token, &queued.execution_id)
            .await
            .unwrap()
            .state,
        ExecutionState::Dispatching
    );
    sqlx::query("DROP TRIGGER expire_startup ON execution_startup_grants")
        .execute(&db.pool)
        .await
        .unwrap();
    grant(&db, &queued.execution_id, &challenge).await;
}

#[tokio::test]
async fn migration_fourteen_never_invents_a_startup_grant_for_an_existing_dispatch() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    db.remove_execution_startup().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 1);
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    grant(&db, &queued.execution_id, &challenge(&attempt)).await;
}
