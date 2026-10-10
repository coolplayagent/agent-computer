mod browser;
mod live;
use super::runtime::{allow, runtime_token};
use super::*;
use agent_computer_store::{
    Error,
    runtime::{apps::*, connections::*, *},
};
use sqlx::Row;

async fn setup() -> (Database, String, String, AdmitApp) {
    setup_browser(None).await
}
async fn setup_browser(image: Option<&str>) -> (Database, String, String, AdmitApp) {
    let (db, token, computer, start, target) =
        super::preparation::setup_browser(10 * 1024 * 1024 * 1024, true, 1, image).await;
    let lease = super::preparation::claim(&db, &start, &target, "app-test-preparer").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    // Database authority contracts only; this is explicitly synthetic storage evidence.
    db.store
        .finish_candidate_preparation(&lease, &super::preparation::evidence(&lease))
        .await
        .unwrap();
    let app: String =
        sqlx::query_scalar("SELECT resource_id FROM resource_definitions WHERE kind='app'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    for (kind, id, permissions) in [
        (
            RuntimeKind::Computer,
            &computer,
            &[RuntimePermission::Connect, RuntimePermission::AppUse][..],
        ),
        (
            RuntimeKind::App,
            &app,
            &[RuntimePermission::Read, RuntimePermission::Manage][..],
        ),
    ] {
        for p in permissions {
            allow(&db, id, kind, *p, None).await;
        }
    }
    let profile: String = sqlx::query_scalar(
        "SELECT resource_id FROM catalog_references WHERE kind='browser_profile'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    db.store
        .register_browser_profile_owner(&org("acme"), &profile, &principal("alice"))
        .await
        .unwrap();
    let connection = db
        .store
        .create_connection_session(
            &token,
            &key("app-connection"),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::AppUse,
                    RuntimePermission::Activate,
                ],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    let input = AdmitApp {
        connection_session_id: connection.session_id,
        computer_generation: start.generation,
        app_id: app,
        expected_app_revision: 1,
        runtime_seconds: 300,
    };
    (db, token, computer, input)
}
async fn admit(db: &Database, token: &str, computer: &str, input: &AdmitApp) -> AppInstance {
    db.store
        .admit_app(token, &key("admit-app"), computer, input)
        .await
        .unwrap()
}
async fn cancel(db: &Database, token: &str, instance: &AppInstance) -> AppInstance {
    db.store
        .cancel_app(
            token,
            &key(&format!("cancel-{}", instance.instance_id)),
            &instance.instance_id,
            &CancelApp {
                app_generation: instance.app_generation,
                expected_revision: instance.revision,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn durable_app_admission_retry_cancel_and_new_generation_survive_restart() {
    let (mut db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    assert_eq!(
        (app.state, app.app_generation, app.revision, app.ready),
        (AppState::Queued, 1, 1, false)
    );
    assert_eq!(app.profile_owner_id, "alice");
    assert!(app.expires_at_ms - app.created_at_ms <= 300_000);
    assert!(
        !db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .ready
    );
    assert_eq!(count(&db, "execution_requests").await, 0);
    assert_eq!(count(&db, "candidate_writer_leases").await, 0);
    db.crash_and_restart().await;
    let before = count(&db, "events").await;
    let retry = admit(&db, &token, &computer, &input).await;
    assert_eq!(retry.instance_id, app.instance_id);
    assert_eq!(retry.expires_at_ms, app.expires_at_ms);
    assert_eq!(count(&db, "events").await, before);
    let mut changed = input.clone();
    changed.runtime_seconds = 299;
    assert!(matches!(
        db.store
            .admit_app(&token, &key("admit-app"), &computer, &changed)
            .await,
        Err(Error::IdempotencyConflict)
    ));
    let cancelled = cancel(&db, &token, &app).await;
    assert_eq!(
        (cancelled.state, cancelled.revision),
        (AppState::Cancelled, 2)
    );
    assert_eq!(cancel(&db, &token, &app).await.state, AppState::Cancelled);
    assert_eq!(
        admit(&db, &token, &computer, &input).await.state,
        AppState::Cancelled
    );
    let second = db
        .store
        .admit_app(&token, &key("next-app"), &computer, &input)
        .await
        .unwrap();
    assert_eq!(second.app_generation, 2);
    assert_ne!(second.instance_id, app.instance_id);
    let storage: String =
        sqlx::query_scalar("SELECT string_agg(binding::text,'') FROM app_instances")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!storage.contains(&token));
    assert!(
        sqlx::query("UPDATE app_instances SET expires_at_ms=expires_at_ms+1")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE app_instances SET state='Queued',revision=revision+1 WHERE state='Cancelled'"
        )
        .execute(&db.pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM app_instances")
            .execute(&db.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_app_admission_reserves_only_one_sandbox_and_profile() {
    let (db, token, computer, input) = setup().await;
    let a = key("first");
    let b = key("second");
    let (first, second) = tokio::join!(
        db.store.admit_app(&token, &a, &computer, &input),
        db.store.admit_app(&token, &b, &computer, &input)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(matches!(
        first.err().or(second.err()),
        Some(Error::RuntimeConflict)
    ));
    assert_eq!(count(&db, "app_instances").await, 1);
    let row = sqlx::query("SELECT binding, sandbox_id, profile_id FROM app_instances")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let binding: Value = row.try_get("binding").unwrap();
    assert_eq!(
        binding["app"]["spec"]["sandboxRef"],
        format!("id:{}", row.try_get::<String, _>("sandbox_id").unwrap())
    );
    assert_eq!(binding["sandbox"]["spec"]["resources"]["cpuMillis"], 2000);
}

#[tokio::test]
async fn app_grants_are_independent_and_revocation_never_revives_a_reservation() {
    let (db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    let profile = app.profile_id.as_str();
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::BrowserProfile,
                resource_id: profile,
                permission: RuntimePermission::AppUse,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    allow(
        &db,
        profile,
        RuntimeKind::BrowserProfile,
        RuntimePermission::AppUse,
        None,
    )
    .await;
    let blocked = db
        .store
        .app_instance(&token, &app.instance_id)
        .await
        .unwrap();
    assert_eq!(blocked.state, AppState::Blocked);
    assert!(matches!(
        db.store
            .admit_app(&token, &key("busy"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
    cancel(&db, &token, &blocked).await;
    let app = db
        .store
        .admit_app(&token, &key("again"), &computer, &input)
        .await
        .unwrap();
    allow(
        &db,
        &input.app_id,
        RuntimeKind::App,
        RuntimePermission::Activate,
        Some(1),
    )
    .await;
    allow(
        &db,
        &input.app_id,
        RuntimeKind::App,
        RuntimePermission::Activate,
        Some(600),
    )
    .await;
    assert_eq!(
        db.store
            .app_instance(&token, &app.instance_id)
            .await
            .unwrap()
            .state,
        AppState::Blocked
    );
}

#[tokio::test]
async fn closed_or_expired_connections_retain_reservations_until_explicit_cancel() {
    let (db, token, computer, mut input) = setup().await;
    input.runtime_seconds = 1;
    let app = admit(&db, &token, &computer, &input).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let blocked = db
        .store
        .app_instance(&token, &app.instance_id)
        .await
        .unwrap();
    assert_eq!((blocked.state, blocked.revision), (AppState::Blocked, 2));
    assert!(matches!(
        db.store
            .admit_app(&token, &key("expired-slot"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
    cancel(&db, &token, &blocked).await;
    input.runtime_seconds = 300;
    let next = db
        .store
        .admit_app(&token, &key("new"), &computer, &input)
        .await
        .unwrap();
    db.store
        .close_connection_session(&token, &input.connection_session_id)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .app_instance(&token, &next.instance_id)
            .await
            .unwrap()
            .state,
        AppState::Blocked
    );
    assert_eq!(count(&db, "app_instances").await, 2);
}

#[tokio::test]
async fn app_ownership_generation_scope_and_pinned_revision_cannot_be_spoofed() {
    let (db, token, computer, input) = setup().await;
    for field in [
        "computer_generation",
        "expected_app_revision",
        "runtime_seconds",
    ] {
        let mut changed = serde_json::to_value(&input).unwrap();
        changed[field] = serde_json::json!(if field == "runtime_seconds" { 301 } else { 2 });
        let changed: AdmitApp = serde_json::from_value(changed).unwrap();
        assert!(
            db.store
                .admit_app(&token, &key(field), &computer, &changed)
                .await
                .is_err()
        );
    }
    let restricted = runtime_token(&db, "acme", "alice", &[ServiceScope::RuntimeActivate]).await;
    assert!(
        db.store
            .admit_app(&restricted, &key("restricted"), &computer, &input)
            .await
            .is_err()
    );
    let app = admit(&db, &token, &computer, &input).await;
    let bob = runtime_token(&db, "acme", "bob", &ServiceScope::ALL).await;
    assert!(matches!(
        db.store.app_instance(&bob, &app.instance_id).await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert!(matches!(
        db.store
            .cancel_app(
                &bob,
                &key("bob-cancel"),
                &app.instance_id,
                &CancelApp {
                    app_generation: 1,
                    expected_revision: 1
                }
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert!(matches!(
        db.store
            .register_browser_profile_owner(&org("acme"), &app.profile_id, &principal("bob"))
            .await,
        Err(Error::RuntimeConflict)
    ));
    assert!(
        sqlx::query("UPDATE browser_profile_owners SET principal='bob'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(matches!(
        db.store
            .cancel_app(
                &token,
                &key("stale-cancel"),
                &app.instance_id,
                &CancelApp {
                    app_generation: 2,
                    expected_revision: 1
                }
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    let renewed = runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    // New credentials can inspect/cancel, but cannot adopt the original connection.
    assert_eq!(
        db.store
            .app_instance(&renewed, &app.instance_id)
            .await
            .unwrap()
            .instance_id,
        app.instance_id
    );
    assert!(
        db.store
            .admit_app(&renewed, &key("new-credential"), &computer, &input)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn credential_and_principal_revocation_block_bound_apps_and_allow_authorized_cleanup() {
    let (db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    let credential: String = sqlx::query_scalar("SELECT credential_id FROM app_instances")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    db.store
        .revoke_credential(&org("acme"), &credential)
        .await
        .unwrap();
    let next = runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    let blocked = db
        .store
        .app_instance(&next, &app.instance_id)
        .await
        .unwrap();
    assert_eq!(blocked.state, AppState::Blocked);
    cancel(&db, &next, &blocked).await;
    let conn = db
        .store
        .create_connection_session(
            &next,
            &key("new-connection"),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::AppUse,
                    RuntimePermission::Activate,
                ],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    let input = AdmitApp {
        connection_session_id: conn.session_id,
        ..input
    };
    let app = db
        .store
        .admit_app(&next, &key("new-instance"), &computer, &input)
        .await
        .unwrap();
    db.store
        .disable_principal(&org("acme"), &principal("alice"))
        .await
        .unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM app_instances WHERE instance_id=$1")
        .bind(&app.instance_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "Blocked");
    assert!(matches!(
        db.store.app_instance(&next, &app.instance_id).await,
        Err(Error::Unauthenticated)
    ));
}

#[tokio::test]
async fn app_reservation_blocks_computer_stop_until_undispatched_cancellation() {
    let (db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    let stop = StopPreparedComputer {
        expected_revision: current.revision,
        request_id: current.active_request.unwrap(),
    };
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    let source=sqlx::query("SELECT i.workspace_id,i.revision,v.digest FROM runtime_start_inputs i JOIN workspace_input_versions v USING(organization,workspace_id,revision) WHERE i.request_id=$1").bind(&stop.request_id).fetch_one(&db.pool).await.unwrap();
    let workspace: String = source.try_get("workspace_id").unwrap();
    allow(
        &db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Publish,
        None,
    )
    .await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Modify,
        None,
    )
    .await;
    let artifact = agent_computer_store::runtime::artifacts::CommitArtifact {
        request_id: stop.request_id.clone(),
        expected_revision: stop.expected_revision,
        base_revision: source.try_get("revision").unwrap(),
        base_manifest: source.try_get("digest").unwrap(),
        publish_current: false,
    };
    assert!(matches!(
        db.store
            .commit_workspace_artifact(&token, &key("artifact"), &workspace, &artifact)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    assert_eq!(count(&db, "artifact_commits").await, 0);
    cancel(&db, &token, &app).await;
    let stopped = db
        .store
        .stop_prepared_computer(&token, &key("stop"), &computer, &stop)
        .await
        .unwrap();
    assert_eq!(stopped.proof, "no_user_dispatch");
    assert!(matches!(
        db.store
            .admit_app(&token, &key("after-stop"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
}

#[tokio::test]
async fn migration_from_predecessor_preserves_credentials_and_prepared_computers() {
    let (db, token, computer, input) = setup().await;
    db.remove_app_admission().await;
    let before = db.store.computer_runtime(&token, &computer).await.unwrap();
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let after = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(before, after);
    // Migration never invents ownership from existing definition/runtime grants.
    assert_eq!(count(&db, "browser_profile_owners").await, 0);
    assert!(matches!(
        db.store
            .admit_app(&token, &key("no-owner"), &computer, &input)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
}

#[tokio::test]
async fn every_app_activation_grant_is_required_before_pinned_details_are_disclosed() {
    let (db, token, computer, input) = setup().await;
    let profile: String = sqlx::query_scalar("SELECT profile_id FROM browser_profile_owners")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    for (kind, id, permission) in [
        (RuntimeKind::Computer, &computer, RuntimePermission::Read),
        (RuntimeKind::Computer, &computer, RuntimePermission::AppUse),
        (
            RuntimeKind::Computer,
            &computer,
            RuntimePermission::Activate,
        ),
        (RuntimeKind::App, &input.app_id, RuntimePermission::Read),
        (RuntimeKind::App, &input.app_id, RuntimePermission::AppUse),
        (RuntimeKind::App, &input.app_id, RuntimePermission::Activate),
        (
            RuntimeKind::BrowserProfile,
            &profile,
            RuntimePermission::AppUse,
        ),
        (RuntimeKind::Computer, &computer, RuntimePermission::Connect),
    ] {
        let budget = (permission == RuntimePermission::Activate).then_some(600);
        db.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org("acme"),
                    principal: &principal("alice"),
                    kind,
                    resource_id: id,
                    permission,
                    max_runtime_seconds: None,
                },
                false,
            )
            .await
            .unwrap();
        let mut request = input.clone();
        if kind != RuntimeKind::BrowserProfile {
            request.expected_app_revision = 2;
        }
        // Missing grant wins over the otherwise visible stale-revision error.
        assert!(
            matches!(
                db.store
                    .admit_app(&token, &key("denied"), &computer, &request)
                    .await,
                Err(Error::RuntimeAccessUnavailable)
            ),
            "{kind:?} {permission:?}"
        );
        allow(&db, id, kind, permission, budget).await;
    }
    assert_eq!(count(&db, "app_instances").await, 0);
}
