use super::{
    runtime::{allow, runtime_fixture, runtime_token},
    *,
};
use agent_computer_store::{
    Error,
    runtime::{connections::*, *},
};
use serde_json::json;

fn request() -> ConnectRequest {
    ConnectRequest {
        requested_capabilities: vec![
            RuntimePermission::Connect,
            RuntimePermission::Read,
            RuntimePermission::Modify,
            RuntimePermission::Activate,
        ],
        lifetime_seconds: 900,
    }
}
fn heartbeat(revision: i64) -> ConnectionHeartbeat {
    ConnectionHeartbeat {
        expected_revision: revision,
        activity: ConnectionActivity::Active,
        visibility: ConnectionVisibility::Visible,
    }
}
async fn setup() -> (Database, String, String) {
    let (db, _, token, computer, _) = runtime_fixture().await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Connect,
        None,
    )
    .await;
    (db, token, computer)
}
async fn connect(db: &Database, token: &str, computer: &str, name: &str) -> ConnectionSession {
    db.store
        .create_connection_session(token, &key(name), computer, &request())
        .await
        .unwrap()
}
async fn revoke(db: &Database, computer: &str, permission: RuntimePermission) {
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Computer,
                resource_id: computer,
                permission,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn human_connection_is_idempotent_durable_and_independent_of_compute_generation() {
    let (mut db, token, computer) = setup().await;
    let before = count(&db, "events").await;
    let a = connect(&db, &token, &computer, "one").await;
    assert_eq!(a.principal_kind, "human");
    assert_eq!(a.capabilities, vec![RuntimePermission::Connect]);
    assert_eq!(
        (a.revision, a.revocation_revision, a.state),
        (1, 0, ConnectionState::Active)
    );
    assert_eq!(count(&db, "runtime_controls").await, 0);
    assert_eq!(count(&db, "runtime_start_requests").await, 0);
    assert_eq!(count(&db, "events").await, before + 1);
    db.crash_and_restart().await;
    let retry = connect(&db, &token, &computer, "one").await;
    assert_eq!(retry.session_id, a.session_id);
    assert_eq!(retry.expires_at_ms, a.expires_at_ms);
    assert_eq!(count(&db, "events").await, before + 1);
    super::starts::grants(&db).await;
    let start = db
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
    assert_eq!(start.generation, 1);
    assert_eq!(
        db.store
            .connection_session(&token, &a.session_id)
            .await
            .unwrap()
            .state,
        ConnectionState::Active
    );
    let closed = db
        .store
        .close_connection_session(&token, &a.session_id)
        .await
        .unwrap();
    assert_eq!(
        (closed.state, closed.revocation_revision),
        (ConnectionState::Closed, 1)
    );
    assert!(closed.capabilities.is_empty());
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .active_request,
        Some(start.request_id)
    );
    let events = count(&db, "events").await;
    assert_eq!(
        connect(&db, &token, &computer, "one").await.state,
        ConnectionState::Closed
    );
    assert_eq!(
        db.store
            .close_connection_session(&token, &a.session_id)
            .await
            .unwrap()
            .revision,
        closed.revision
    );
    assert_eq!(count(&db, "events").await, events);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM events e JOIN outbox o USING(organization,sequence) WHERE e.kind LIKE 'connection.%'").fetch_one(&db.pool).await.unwrap(),2);
}

#[tokio::test]
async fn current_requested_grants_and_scopes_intersect_and_connect_revocation_is_terminal() {
    let (db, _, computer) = setup().await;
    for permission in [
        RuntimePermission::Read,
        RuntimePermission::Modify,
        RuntimePermission::Activate,
    ] {
        allow(
            &db,
            &computer,
            RuntimeKind::Computer,
            permission,
            (permission == RuntimePermission::Activate).then_some(300),
        )
        .await;
    }
    let token = runtime_token(
        &db,
        "acme",
        "alice",
        &[
            ServiceScope::RuntimeConnect,
            ServiceScope::RuntimeRead,
            ServiceScope::RuntimeActivate,
        ],
    )
    .await;
    let session = connect(&db, &token, &computer, "one").await;
    assert_eq!(
        session.capabilities,
        vec![
            RuntimePermission::Connect,
            RuntimePermission::Read,
            RuntimePermission::Activate
        ]
    );
    assert_eq!(session.max_runtime_seconds, Some(300));
    revoke(&db, &computer, RuntimePermission::Read).await;
    assert!(
        !connect(&db, &token, &computer, "one")
            .await
            .capabilities
            .contains(&RuntimePermission::Read)
    );
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Activate,
        Some(60),
    )
    .await;
    assert_eq!(
        db.store
            .connection_session(&token, &session.session_id)
            .await
            .unwrap()
            .max_runtime_seconds,
        Some(60)
    );
    revoke(&db, &computer, RuntimePermission::Connect).await;
    let revoked = db
        .store
        .connection_session(&token, &session.session_id)
        .await
        .unwrap();
    assert_eq!(
        (revoked.state, revoked.revocation_revision),
        (ConnectionState::Revoked, 1)
    );
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Connect,
        None,
    )
    .await;
    assert_eq!(
        connect(&db, &token, &computer, "one").await.state,
        ConnectionState::Revoked
    );
    assert!(matches!(
        db.store
            .heartbeat_connection_session(
                &token,
                &key("hb"),
                &session.session_id,
                &heartbeat(revoked.revision)
            )
            .await,
        Err(Error::ConnectionInactive)
    ));
    assert_eq!(
        connect(&db, &token, &computer, "new").await.state,
        ConnectionState::Active
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT (payload->>'revoked_connection_count')::bigint FROM events WHERE kind='access.revoked' AND payload->>'permission'='connect' AND payload->>'enabled'='false'").fetch_one(&db.pool).await.unwrap(),1);
}

#[tokio::test]
async fn connections_bind_exact_credentials_and_isolate_principals_and_organizations() {
    let (db, token, computer) = setup().await;
    let session = connect(&db, &token, &computer, "same-key").await;
    let (workspace,name):(String,String)=sqlx::query_as("SELECT resource_id,name FROM resource_definitions WHERE organization='acme' AND kind='workspace'").fetch_one(&db.pool).await.unwrap();
    grant(
        &db,
        "acme",
        "alice",
        DefinitionKind::Workspace,
        &name,
        DefinitionPermission::Reference,
        true,
    )
    .await;
    let second = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"second-connection"},"spec":{"computers":[{"name":"second","workspaceRef":format!("id:{workspace}"),"sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"}]}});
    let plan = db
        .store
        .create_definition_plan(&token, &key("second-plan"), &checked(&second))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(
            &token,
            &key("second-apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    let second_computer = &plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Computer)
        .unwrap()
        .resource_id;
    allow(
        &db,
        second_computer,
        RuntimeKind::Computer,
        RuntimePermission::Connect,
        None,
    )
    .await;
    let second_session = connect(&db, &token, second_computer, "second-computer").await;
    assert_ne!(session.session_id, second_session.session_id);
    assert_ne!(session.computer_id, second_session.computer_id);
    assert_eq!(count(&db, "runtime_controls").await, 0);
    for other in [
        runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await,
        runtime_token(&db, "acme", "bob", &ServiceScope::ALL).await,
        runtime_token(&db, "other", "alice", &ServiceScope::ALL).await,
    ] {
        assert!(matches!(
            db.store
                .connection_session(&other, &session.session_id)
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
        assert!(matches!(
            db.store
                .close_connection_session(&other, &session.session_id)
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
    let replacement = runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    assert!(matches!(
        db.store
            .create_connection_session(&replacement, &key("same-key"), &computer, &request())
            .await,
        Err(Error::IdempotencyConflict)
    ));
    let agent = db
        .store
        .issue_credential(IssueCredential {
            organization: &org("acme"),
            principal: &principal("agent"),
            kind: PrincipalKind::Agent,
            scopes: &[ServiceScope::RuntimeConnect],
            lifetime: Duration::from_secs(3600),
        })
        .await
        .unwrap();
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("agent"),
                kind: RuntimeKind::Computer,
                resource_id: &computer,
                permission: RuntimePermission::Connect,
                max_runtime_seconds: None,
            },
            true,
        )
        .await
        .unwrap();
    let agent_session = connect(&db, agent.expose_token(), &computer, "same-key").await;
    assert_eq!(agent_session.principal_kind, "agent");
    assert_ne!(agent_session.session_id, session.session_id);
    let id = token
        .strip_prefix("acsk_")
        .unwrap()
        .split('_')
        .next()
        .unwrap();
    db.store.revoke_credential(&org("acme"), id).await.unwrap();
    assert!(matches!(
        db.store
            .connection_session(&token, &session.session_id)
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(
        db.store
            .connection_session(agent.expose_token(), &agent_session.session_id)
            .await
            .unwrap()
            .state,
        ConnectionState::Active
    );
    db.store
        .disable_principal(&org("acme"), &principal("agent"))
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .close_connection_session(agent.expose_token(), &agent_session.session_id)
            .await,
        Err(Error::Unauthenticated)
    ));
}

#[tokio::test]
async fn concurrent_connect_and_heartbeat_retries_do_not_duplicate_or_extend_sessions() {
    let (db, token, computer) = setup().await;
    let req = request();
    let k = key("same");
    let (a, b) = tokio::join!(
        db.store
            .create_connection_session(&token, &k, &computer, &req),
        db.store
            .create_connection_session(&token, &k, &computer, &req)
    );
    let a = a.unwrap();
    assert_eq!(a.session_id, b.unwrap().session_id);
    assert_eq!(count(&db, "connection_sessions").await, 1);
    let input = heartbeat(1);
    let k = key("heartbeat");
    let (one, two) = tokio::join!(
        db.store
            .heartbeat_connection_session(&token, &k, &a.session_id, &input),
        db.store
            .heartbeat_connection_session(&token, &k, &a.session_id, &input)
    );
    let one = one.unwrap();
    assert_eq!(one.revision, 2);
    assert_eq!(two.unwrap().revision, 2);
    assert_eq!(one.expires_at_ms, a.expires_at_ms);
    assert_eq!(
        (one.activity, one.visibility),
        (ConnectionActivity::Active, ConnectionVisibility::Visible)
    );
    assert!(matches!(
        db.store
            .heartbeat_connection_session(&token, &key("stale"), &a.session_id, &input)
            .await,
        Err(Error::ConnectionRevisionConflict)
    ));
    let changed = ConnectionHeartbeat {
        visibility: ConnectionVisibility::Hidden,
        ..input.clone()
    };
    assert!(matches!(
        db.store
            .heartbeat_connection_session(&token, &k, &a.session_id, &changed)
            .await,
        Err(Error::IdempotencyConflict)
    ));
    let before = count(&db, "events").await;
    db.store
        .close_connection_session(&token, &a.session_id)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .heartbeat_connection_session(&token, &k, &a.session_id, &input)
            .await
            .unwrap()
            .state,
        ConnectionState::Closed
    );
    assert_eq!(count(&db, "events").await, before + 1);
}

#[tokio::test]
async fn expired_connections_cannot_be_revived_and_identity_expiry_caps_lifetime() {
    let (db, token, computer) = setup().await;
    let short = ConnectRequest {
        lifetime_seconds: 1,
        ..request()
    };
    let a = db
        .store
        .create_connection_session(&token, &key("short"), &computer, &short)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let now = db
        .store
        .connection_session(&token, &a.session_id)
        .await
        .unwrap();
    assert_eq!(now.state, ConnectionState::Expired);
    assert!(now.capabilities.is_empty());
    assert_eq!(
        db.store
            .create_connection_session(&token, &key("short"), &computer, &short)
            .await
            .unwrap()
            .state,
        ConnectionState::Expired
    );
    assert!(matches!(
        db.store
            .heartbeat_connection_session(&token, &key("hb"), &a.session_id, &heartbeat(1))
            .await,
        Err(Error::ConnectionInactive)
    ));
    let limited = db
        .store
        .issue_credential(IssueCredential {
            organization: &org("acme"),
            principal: &principal("alice"),
            kind: PrincipalKind::Human,
            scopes: &[ServiceScope::RuntimeConnect],
            lifetime: Duration::from_secs(60),
        })
        .await
        .unwrap();
    let c = connect(&db, limited.expose_token(), &computer, "limited").await;
    let expiry:i64=sqlx::query_scalar("SELECT floor(extract(epoch from expires_at)*1000)::bigint FROM service_credentials WHERE credential_id=$1").bind(limited.id()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(c.expires_at_ms, expiry);
    assert!(c.expires_at_ms - c.created_at_ms <= 60_000);
    let delayed = db
        .store
        .create_connection_session(&token, &key("delayed"), &computer, &short)
        .await
        .unwrap();
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION delay_connection_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(1.1); RETURN NEW; END $$; CREATE TRIGGER delay_connection_event BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION delay_connection_event();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .heartbeat_connection_session(&token, &key("late"), &delayed.session_id, &heartbeat(1))
            .await,
        Err(Error::ConnectionInactive)
    ));
    assert_eq!(count(&db, "events").await, before);
    let expired = db
        .store
        .connection_session(&token, &delayed.session_id)
        .await
        .unwrap();
    assert_eq!(
        (expired.state, expired.revision),
        (ConnectionState::Expired, 1)
    );
}

#[tokio::test]
async fn invalid_intent_missing_permissions_and_immutable_fields_never_create_connections() {
    let (db, definition, token, computer, workspace) = runtime_fixture().await;
    assert!(matches!(
        db.store
            .create_connection_session(&token, &key("denied"), &computer, &request())
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert!(matches!(
        db.store
            .create_connection_session(&definition, &key("scope"), &computer, &request())
            .await,
        Err(Error::Forbidden)
    ));
    for req in [
        ConnectRequest {
            requested_capabilities: vec![],
            ..request()
        },
        ConnectRequest {
            requested_capabilities: vec![RuntimePermission::Read],
            ..request()
        },
        ConnectRequest {
            requested_capabilities: vec![RuntimePermission::Connect, RuntimePermission::Connect],
            ..request()
        },
        ConnectRequest {
            lifetime_seconds: 0,
            ..request()
        },
        ConnectRequest {
            lifetime_seconds: 3601,
            ..request()
        },
    ] {
        assert!(matches!(
            db.store
                .create_connection_session(&token, &key("bad"), &computer, &req)
                .await,
            Err(Error::InvalidRuntimeRequest)
        ));
    }
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Connect,
        None,
    )
    .await;
    assert!(matches!(
        db.store
            .create_connection_session(&token, &key("wrong-kind"), &workspace, &request())
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let a = connect(&db, &token, &computer, "okay").await;
    for assignment in [
        "requested='[]'::jsonb",
        "expires_at_ms=expires_at_ms+1",
        "computer_id='replacement'",
        "principal='other'",
        "revision=revision+2",
        "state='Closed'",
    ] {
        assert!(
            sqlx::query(&format!("UPDATE connection_sessions SET {assignment}"))
                .execute(&db.pool)
                .await
                .is_err()
        );
    }
    assert!(
        sqlx::query("DELETE FROM connection_sessions")
            .execute(&db.pool)
            .await
            .is_err()
    );
    let mut reordered = request();
    reordered.requested_capabilities.reverse();
    assert_eq!(
        db.store
            .create_connection_session(&token, &key("okay"), &computer, &reordered)
            .await
            .unwrap()
            .session_id,
        a.session_id
    );
    reordered.lifetime_seconds = 100;
    assert!(matches!(
        db.store
            .create_connection_session(&token, &key("okay"), &computer, &reordered)
            .await,
        Err(Error::IdempotencyConflict)
    ));
    assert_eq!(count(&db, "connection_sessions").await, 1);
}

#[tokio::test]
async fn outbox_failure_and_late_identity_expiry_roll_back_connection_mutations() {
    let (db, token, computer) = setup().await;
    let baseline = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_connection_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END $$; CREATE TRIGGER reject_connection_event BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_connection_event();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .create_connection_session(&token, &key("one"), &computer, &request())
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "connection_sessions").await, 0);
    assert_eq!(count(&db, "events").await, baseline);
    sqlx::query("DROP TRIGGER reject_connection_event ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    let a = connect(&db, &token, &computer, "one").await;
    let baseline = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION expire_connection_credential() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.connect'=ANY(scopes); RETURN NEW; END $$; CREATE TRIGGER expire_connection_credential BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION expire_connection_credential();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .heartbeat_connection_session(&token, &key("hb"), &a.session_id, &heartbeat(1))
            .await,
        Err(Error::Unauthenticated)
    ));
    assert!(matches!(
        db.store
            .close_connection_session(&token, &a.session_id)
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(
        db.store
            .connection_session(&token, &a.session_id)
            .await
            .unwrap()
            .revision,
        1
    );
    assert_eq!(count(&db, "events").await, baseline);
    sqlx::query("DROP TRIGGER expire_connection_credential ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .heartbeat_connection_session(&token, &key("hb"), &a.session_id, &heartbeat(1))
            .await
            .unwrap()
            .revision,
        2
    );
}

#[tokio::test]
async fn connection_capacity_is_serialized_and_close_releases_only_connection_slots() {
    let (db, token, computer) = setup().await;
    let mut first = None;
    for i in 0..31 {
        first.get_or_insert(connect(&db, &token, &computer, &format!("c{i}")).await);
    }
    let req = request();
    let ka = key("last-a");
    let kb = key("last-b");
    let (a, b) = tokio::join!(
        db.store
            .create_connection_session(&token, &ka, &computer, &req),
        db.store
            .create_connection_session(&token, &kb, &computer, &req)
    );
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(Error::RuntimeCapacityUnavailable))
            | (Err(Error::RuntimeCapacityUnavailable), Ok(_))
    ));
    assert_eq!(count(&db, "connection_sessions").await, 32);
    db.store
        .close_connection_session(&token, &first.unwrap().session_id)
        .await
        .unwrap();
    assert_eq!(
        connect(&db, &token, &computer, "after-close").await.state,
        ConnectionState::Active
    );
    assert_eq!(count(&db, "runtime_controls").await, 0);
}

#[tokio::test]
async fn connection_upgrade_preserves_existing_metadata_and_checks_exact_migration_history() {
    let (db, token, computer) = setup().await;
    let before: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(r)) FROM runtime_grants r")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::raw_sql("DROP TABLE candidate_writer_drains,candidate_writer_dispatches,candidate_writer_epochs,candidate_writer_leases; DROP FUNCTION guard_writer_record_insert(); DROP FUNCTION guard_writer_lease_mutation(); DROP TABLE connection_sessions; DROP FUNCTION guard_connection_session_mutation(); DELETE FROM _sqlx_migrations WHERE version>=9;").execute(&db.pool).await.unwrap();
    assert!(matches!(db.store.ready().await, Err(Error::SchemaNotReady)));
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "connection_sessions").await, 0);
    assert_eq!(
        sqlx::query_scalar::<_, Value>("SELECT jsonb_agg(to_jsonb(r)) FROM runtime_grants r")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        connect(&db, &token, &computer, "after").await.state,
        ConnectionState::Active
    );
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=9")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(db.store.ready().await, Err(Error::SchemaNotReady)));
}

#[tokio::test]
async fn revocation_waits_for_inflight_connection_then_prevents_revival() {
    let (db, token, computer) = setup().await;
    let a = connect(&db, &token, &computer, "one").await;
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT last_sequence FROM organization_streams WHERE organization='acme' FOR UPDATE",
    )
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    let store = db.store.clone();
    let target = computer.clone();
    let revoke = tokio::spawn(async move {
        store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org("acme"),
                    principal: &principal("alice"),
                    kind: RuntimeKind::Computer,
                    resource_id: &target,
                    permission: RuntimePermission::Connect,
                    max_runtime_seconds: None,
                },
                false,
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE '%organization_streams%')").fetch_one(&db.pool).await.unwrap();
        if waiting {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(!revoke.is_finished());
    blocker.commit().await.unwrap();
    revoke.await.unwrap().unwrap();
    assert_eq!(
        db.store
            .connection_session(&token, &a.session_id)
            .await
            .unwrap()
            .state,
        ConnectionState::Revoked
    );
    assert!(matches!(
        db.store
            .create_connection_session(&token, &key("new"), &computer, &request())
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let event: Value = sqlx::query_scalar(
        "SELECT payload FROM events WHERE kind='access.revoked' AND payload->>'enabled'='false'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(event["process_termination_confirmed"], json!(false));
}
