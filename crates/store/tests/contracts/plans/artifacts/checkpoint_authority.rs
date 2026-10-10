use super::checkpoint::{request, start};
use super::*;
use agent_computer_store::runtime::connections::*;

pub(super) async fn session(
    db: &Database,
    computer: &str,
    actor: &str,
) -> (String, ConnectionSession) {
    let token = super::super::runtime::runtime_token(db, "acme", actor, &ServiceScope::ALL).await;
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal(actor),
                kind: RuntimeKind::Computer,
                resource_id: computer,
                permission: RuntimePermission::Connect,
                max_runtime_seconds: None,
            },
            true,
        )
        .await
        .unwrap();
    let session = db
        .store
        .create_connection_session(
            &token,
            &key(actor),
            computer,
            &ConnectRequest {
                requested_capabilities: vec![RuntimePermission::Connect],
                lifetime_seconds: 300,
            },
        )
        .await
        .unwrap();
    (token, session)
}

#[tokio::test]
async fn checkpoint_stop_rejects_apps_and_other_active_connections() {
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
        let other = if apps {
            None
        } else {
            Some(session(&db, &computer, "bob").await)
        };
        assert!(matches!(
            db.store
                .checkpoint_stop_computer(
                    &token,
                    &key("checkpoint-stop"),
                    &computer,
                    &request(&input)
                )
                .await,
            Err(error) if matches!((&error,apps),(&Error::RuntimeStopBlocked,true)|(&Error::RuntimeActiveUse,false))
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
        if let Some((token_b, connection)) = other {
            db.store
                .close_connection_session(&token_b, &connection.session_id)
                .await
                .unwrap();
            assert!(
                start(&db, &token, &computer, &input)
                    .await
                    .stop_after_commit
            );
        }
    }
}

#[tokio::test]
async fn newly_active_use_blocks_finalization_without_losing_sealed_work() {
    let (db, token, computer, _, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    let (other, connection) = session(&db, &computer, "bob").await;
    assert!(matches!(
        db.store.finish_artifact(&lease, &verified).await,
        Err(Error::RuntimeActiveUse)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
    db.store
        .close_connection_session(&other, &connection.session_id)
        .await
        .unwrap();
    assert!(
        db.store
            .finish_artifact(&lease, &verified)
            .await
            .unwrap()
            .stop_receipt
            .is_some()
    );
}

#[tokio::test]
async fn checkpoint_stop_requires_current_manage_authority_and_replacement_credentials() {
    let (db, token, computer, _, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
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
        db.store.finish_artifact(&lease, &verified).await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert!(db.store.renew_artifact(&lease).await.is_err());
    assert_eq!(count(&db, "runtime_stops").await, 0);
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Manage,
        None,
    )
    .await;
    let fresh =
        super::super::runtime::runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    db.store
        .checkpoint_stop_computer(&fresh, &key("checkpoint-stop"), &computer, &request(&input))
        .await
        .unwrap();
    assert!(matches!(
        db.store.finish_artifact(&lease, &verified).await,
        Err(Error::StaleReconcileLease)
    ));
    let replacement = db
        .store
        .claim_artifact(
            &org("acme"),
            &first.commit_id,
            &WorkerId::new("replacement").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replacement.capture(), Some(&bundle));
    assert!(
        db.store
            .finish_artifact(&replacement, &verified)
            .await
            .unwrap()
            .stop_receipt
            .is_some()
    );
}

#[tokio::test]
async fn own_idle_connection_is_allowed_but_human_activity_blocks_both_boundaries() {
    let (db, token, computer, _, input) = setup().await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Manage,
        None,
    )
    .await;
    let (owner, mut connection) = session(&db, &computer, "alice").await;
    for (n, activity) in [ConnectionActivity::Active, ConnectionActivity::Idle]
        .into_iter()
        .enumerate()
    {
        connection = db
            .store
            .heartbeat_connection_session(
                &owner,
                &key(&format!("activity-{n}")),
                &connection.session_id,
                &ConnectionHeartbeat {
                    expected_revision: connection.revision,
                    activity,
                    visibility: ConnectionVisibility::Visible,
                },
            )
            .await
            .unwrap();
        if activity == ConnectionActivity::Active {
            assert!(matches!(
                db.store
                    .checkpoint_stop_computer(
                        &token,
                        &key("checkpoint-stop"),
                        &computer,
                        &request(&input)
                    )
                    .await,
                Err(Error::RuntimeActiveUse)
            ));
        }
    }
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    for (n, activity) in [ConnectionActivity::Active, ConnectionActivity::Idle]
        .into_iter()
        .enumerate()
    {
        connection = db
            .store
            .heartbeat_connection_session(
                &owner,
                &key(&format!("final-activity-{n}")),
                &connection.session_id,
                &ConnectionHeartbeat {
                    expected_revision: connection.revision,
                    activity,
                    visibility: ConnectionVisibility::Visible,
                },
            )
            .await
            .unwrap();
        if activity == ConnectionActivity::Active {
            assert!(matches!(
                db.store.finish_artifact(&lease, &verified).await,
                Err(Error::RuntimeActiveUse)
            ));
        }
    }
    assert!(
        db.store
            .finish_artifact(&lease, &verified)
            .await
            .unwrap()
            .stop_receipt
            .is_some()
    );
}

#[tokio::test]
async fn checkpoint_stop_lease_expiring_during_stop_outbox_rolls_back_both_results() {
    let (db, token, computer, _, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    sqlx::raw_sql("UPDATE artifact_commits SET lease_until_ms=floor(extract(epoch from clock_timestamp())*1000)::bigint+1500; CREATE FUNCTION delay_stop_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM events WHERE organization=NEW.organization AND sequence=NEW.sequence AND kind='computer.stopped') THEN PERFORM pg_sleep(1.8); END IF; RETURN NEW; END $$; CREATE TRIGGER delay_stop_outbox BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION delay_stop_outbox();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store.finish_artifact(&lease, &verified).await,
        Err(Error::StaleReconcileLease)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
    let next = db
        .store
        .claim_artifact(
            &org("acme"),
            &first.commit_id,
            &WorkerId::new("retry").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        db.store
            .finish_artifact(&next, &verified)
            .await
            .unwrap()
            .stop_receipt
            .is_some()
    );
}

#[tokio::test]
async fn checkpoint_stop_deferred_constraint_keeps_an_expired_commit_atomic() {
    let (db, token, computer, _, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    sqlx::raw_sql("UPDATE artifact_commits SET lease_until_ms=floor(extract(epoch from clock_timestamp())*1000)::bigint+1500; CREATE FUNCTION delay_checkpoint_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state<>'Capturing' THEN PERFORM pg_sleep(1.8); END IF; RETURN NULL; END $$; CREATE CONSTRAINT TRIGGER aaa_delay_checkpoint_commit AFTER UPDATE ON artifact_commits DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION delay_checkpoint_commit();").execute(&db.pool).await.unwrap();
    let error = db
        .store
        .finish_artifact(&lease, &verified)
        .await
        .unwrap_err();
    assert!(
        matches!(error,Error::Database(ref e) if e.as_database_error().is_some_and(|e|e.message().contains("checkpoint publication and stop")))
    );
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
}
