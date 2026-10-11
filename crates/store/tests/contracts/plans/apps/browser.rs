use super::*;
use agent_computer_browser::Viewport;
use agent_computer_kubernetes::NodeIdentity;
use agent_computer_store::runtime::apps::browser::BrowserDeployment;
pub(super) fn deployment() -> BrowserDeployment {
    BrowserDeployment {
        namespace: "browser-test".into(),
        namespace_uid: "namespace-uid".into(),
        approved_image: format!("registry.example.invalid/browser@sha256:{}", "a".repeat(64)),
        node: NodeIdentity {
            name: "node-a".into(),
            uid: "node-uid".into(),
            boot_id: "boot-id".into(),
        },
        viewport: Viewport {
            width: 800,
            height: 600,
        },
    }
}
#[tokio::test]
async fn browser_dispatch_is_one_shot_and_survives_commit_loss_and_restart() {
    let (mut db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    let deployment = deployment();
    let org = org("acme");
    let (a, b) = tokio::join!(
        db.store
            .begin_browser_app(&org, &app.instance_id, 1, &deployment),
        db.store
            .begin_browser_app(&org, &app.instance_id, 1, &deployment)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let attempt = a.ok().or(b.ok()).unwrap();
    let manifest = attempt.plan().pod_plan().manifest();
    assert_eq!(
        manifest["spec"]["containers"][0]["image"],
        deployment.approved_image
    );
    assert_eq!(attempt.plan().binding().actor.principal_id, "alice");
    assert_eq!(
        attempt.plan().binding().actor.connection_session_id,
        input.connection_session_id
    );
    assert_eq!(
        manifest["spec"]["containers"][0]["resources"]["limits"]["cpu"],
        "2"
    );
    assert!(attempt.remaining_budget_ms().unwrap() <= 300_000);
    db.store
        .record_browser_pod(&attempt, "pod-uid")
        .await
        .unwrap();
    db.store
        .record_browser_pod(&attempt, "pod-uid")
        .await
        .unwrap();
    assert!(matches!(
        db.store.record_browser_pod(&attempt, "replacement").await,
        Err(Error::RuntimeConflict)
    ));
    db.crash_and_restart().await;
    assert!(matches!(
        db.store
            .begin_browser_app(&org, &app.instance_id, 2, &deployment)
            .await,
        Err(Error::RuntimeConflict)
    ));
    let current = db
        .store
        .app_instance(&token, &app.instance_id)
        .await
        .unwrap();
    assert_eq!(
        (current.state, current.ready),
        (AppState::Dispatching, false)
    );
    assert!(matches!(
        db.store
            .cancel_app(
                &token,
                &key("unsafe-cancel"),
                &app.instance_id,
                &CancelApp {
                    app_generation: 1,
                    expected_revision: 2
                }
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    db.store
        .drain_browser_app(&org, &app.instance_id)
        .await
        .unwrap();
    db.store
        .drain_browser_app(&org, &app.instance_id)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .app_instance(&token, &app.instance_id)
            .await
            .unwrap()
            .state,
        AppState::Draining
    );
    assert!(matches!(
        db.store
            .admit_app(&token, &key("slot-retained"), &computer, &input)
            .await,
        Err(Error::RuntimeConflict)
    ));
    let entries = db
        .store
        .browser_app_journal(&org, &app.instance_id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].payload["manifest"], manifest);
    assert!(!serde_json::to_string(&entries).unwrap().contains(&token));
    for query in [
        "DELETE FROM app_browser_journal",
        "UPDATE app_browser_journal SET payload='{}'::jsonb",
        "UPDATE app_instances SET state='Cancelled',revision=revision+1",
        "UPDATE app_instances SET state='Running',revision=revision+1",
    ] {
        assert!(
            sqlx::query(query).execute(&db.pool).await.is_err(),
            "{query}"
        );
    }
}
#[tokio::test]
async fn browser_plan_failure_and_stale_revision_leave_no_dispatch() {
    let (db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    let mut deployment = deployment();
    let org = org("acme");
    assert!(matches!(
        db.store
            .begin_browser_app(&org, &app.instance_id, 2, &deployment)
            .await,
        Err(Error::RevisionConflict)
    ));
    deployment.approved_image = deployment.approved_image.replace('a', "b");
    assert!(matches!(
        db.store
            .begin_browser_app(&org, &app.instance_id, 1, &deployment)
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    assert_eq!(count(&db, "app_browser_journal").await, 0);
    assert_eq!(
        db.store
            .app_instance(&token, &app.instance_id)
            .await
            .unwrap()
            .state,
        AppState::Queued
    );
    cancel(&db, &token, &app).await;
}
#[tokio::test]
async fn browser_revocation_is_sticky_and_never_asserts_process_stop() {
    for cause in ["credential", "principal", "connection", "grant", "budget"] {
        let (db, token, computer, input) = setup().await;
        let app = admit(&db, &token, &computer, &input).await;
        let org = org("acme");
        let _attempt = db
            .store
            .begin_browser_app(&org, &app.instance_id, 1, &deployment())
            .await
            .unwrap();
        match cause {
            "credential" => {
                let id: String = sqlx::query_scalar("SELECT credential_id FROM app_instances")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
                db.store.revoke_credential(&org, &id).await.unwrap();
            }
            "principal" => {
                db.store
                    .disable_principal(&org, &principal("alice"))
                    .await
                    .unwrap();
            }
            "connection" => {
                db.store
                    .close_connection_session(&token, &input.connection_session_id)
                    .await
                    .unwrap();
            }
            "grant" => {
                db.store
                    .set_runtime_grant(
                        RuntimeGrant {
                            organization: &org,
                            principal: &principal("alice"),
                            kind: RuntimeKind::App,
                            resource_id: &app.app_id,
                            permission: RuntimePermission::AppUse,
                            max_runtime_seconds: None,
                        },
                        false,
                    )
                    .await
                    .unwrap();
                allow(
                    &db,
                    &app.app_id,
                    RuntimeKind::App,
                    RuntimePermission::AppUse,
                    None,
                )
                .await;
            }
            "budget" => {
                allow(
                    &db,
                    &app.app_id,
                    RuntimeKind::App,
                    RuntimePermission::Activate,
                    Some(1),
                )
                .await;
                allow(
                    &db,
                    &app.app_id,
                    RuntimeKind::App,
                    RuntimePermission::Activate,
                    Some(600),
                )
                .await;
            }
            _ => unreachable!(),
        }
        let state: String = sqlx::query_scalar("SELECT state FROM app_instances")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(state, "Draining", "{cause}");
        assert!(
            db.store
                .begin_browser_app(&org, &app.instance_id, 3, &deployment())
                .await
                .is_err()
        );
        let entries = db
            .store
            .browser_app_journal(&org, &app.instance_id)
            .await
            .unwrap();
        assert_eq!(
            entries.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["dispatch"]
        );
    }
}
#[tokio::test]
async fn browser_expiry_blocks_dispatch_without_releasing_profile() {
    let (db, token, computer, mut input) = setup().await;
    input.runtime_seconds = 1;
    let app = admit(&db, &token, &computer, &input).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        db.store
            .begin_browser_app(&org("acme"), &app.instance_id, 1, &deployment())
            .await
            .is_err()
    );
    assert_eq!(
        db.store
            .app_instance(&token, &app.instance_id)
            .await
            .unwrap()
            .state,
        AppState::Blocked
    );
    assert_eq!(count(&db, "app_browser_journal").await, 0);
}

#[tokio::test]
async fn browser_migration_preserves_existing_queued_admissions() {
    let (db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    // Disposable predecessor fixture with a real existing admission and credential.
    sqlx::raw_sql("DROP TABLE app_browser_journal; DROP FUNCTION guard_app_browser_journal(); ALTER TABLE app_instances DROP CONSTRAINT app_instances_state_check; ALTER TABLE app_instances ADD CHECK(state IN ('Queued','Blocked','Cancelled')); DELETE FROM _sqlx_migrations WHERE version=32;").execute(&db.pool).await.unwrap();
    let source = include_str!("../../../../migrations/0031_app_admission.sql");
    for name in ["guard_app_admission", "invalidate_app_authority"] {
        let body = source
            .split(&format!("FUNCTION {name}"))
            .nth(1)
            .unwrap()
            .split("$$;")
            .next()
            .unwrap();
        sqlx::raw_sql(&format!("CREATE OR REPLACE FUNCTION {name}{body}$$;"))
            .execute(&db.pool)
            .await
            .unwrap();
    }
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let current = db
        .store
        .app_instance(&token, &app.instance_id)
        .await
        .unwrap();
    assert_eq!(
        (current.state, current.revision, current.expires_at_ms),
        (AppState::Queued, 1, app.expires_at_ms)
    );
    assert_eq!(count(&db, "app_browser_journal").await, 0);
    db.store
        .begin_browser_app(&org("acme"), &app.instance_id, 1, &deployment())
        .await
        .unwrap();
    assert_eq!(count(&db, "app_browser_journal").await, 1);
}
