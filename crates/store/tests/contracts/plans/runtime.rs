use super::*;
use agent_computer_store::{Error, runtime::*};

pub(super) async fn runtime_fixture() -> (Database, String, String, String, String) {
    runtime_fixture_with_quota(10 * 1024 * 1024 * 1024).await
}
pub(super) async fn runtime_fixture_with_quota(
    quota: i64,
) -> (Database, String, String, String, String) {
    runtime_fixture_with_apps(quota, true).await
}
pub(super) async fn runtime_fixture_with_apps(
    quota: i64,
    apps: bool,
) -> (Database, String, String, String, String) {
    runtime_fixture_many(quota, apps, 1).await
}
pub(super) async fn runtime_fixture_many(
    quota: i64,
    apps: bool,
    computers: usize,
) -> (Database, String, String, String, String) {
    let (db, definition_token, mut document) = fixture().await;
    for i in 1..computers {
        let mut computer = document["spec"]["computers"][0].clone();
        computer["name"] = format!("parallel-{i}").into();
        document["spec"]["computers"]
            .as_array_mut()
            .unwrap()
            .push(computer);
    }
    if !apps {
        for computer in document["spec"]["computers"].as_array_mut().unwrap() {
            computer["appRefs"] = serde_json::json!([]);
        }
        document["spec"]["apps"] = serde_json::json!([]);
    }
    document["spec"]["volumes"][0]["quotaBytes"] = quota.into();
    let plan = db
        .store
        .create_definition_plan(&definition_token, &key("runtime-plan"), &checked(&document))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(
            &definition_token,
            &key("runtime-apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    let computer = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Computer)
        .unwrap()
        .resource_id
        .clone();
    let workspace = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Workspace)
        .unwrap()
        .resource_id
        .clone();
    let token = runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    (db, definition_token, token, computer, workspace)
}
pub(super) async fn runtime_token(
    db: &Database,
    organization: &str,
    actor: &str,
    scopes: &[ServiceScope],
) -> String {
    db.store
        .issue_credential(IssueCredential {
            organization: &org(organization),
            principal: &principal(actor),
            kind: PrincipalKind::Human,
            scopes,
            lifetime: Duration::from_secs(3600),
        })
        .await
        .unwrap()
        .expose_token()
        .into()
}
pub(super) async fn allow(
    db: &Database,
    id: &str,
    kind: RuntimeKind,
    permission: RuntimePermission,
    seconds: Option<u32>,
) {
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind,
                resource_id: id,
                permission,
                max_runtime_seconds: seconds,
            },
            true,
        )
        .await
        .unwrap();
}
fn need(id: &str, kind: RuntimeKind, permission: RuntimePermission) -> RuntimeRequirement {
    RuntimeRequirement {
        kind,
        resource_id: id.into(),
        permission,
        runtime_seconds: None,
    }
}

#[tokio::test]
async fn definition_creators_scopes_and_runtime_manage_do_not_imply_other_permissions() {
    let (db, definition, token, computer, _) = runtime_fixture().await;
    let read = need(&computer, RuntimeKind::Computer, RuntimePermission::Read);
    assert!(matches!(
        db.store
            .check_runtime_permissions(&definition, std::slice::from_ref(&read))
            .await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, std::slice::from_ref(&read))
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Manage,
        None,
    )
    .await;
    assert!(matches!(
        db.store
            .runtime_access(&token, RuntimeKind::Computer, &computer)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Read,
        None,
    )
    .await;
    db.store
        .check_runtime_permissions(&token, &[read])
        .await
        .unwrap();
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Control,
        None,
    )
    .await;
    for permission in [
        RuntimePermission::Connect,
        RuntimePermission::Observe,
        RuntimePermission::Execute,
        RuntimePermission::Modify,
        RuntimePermission::AppUse,
    ] {
        assert!(matches!(
            db.store
                .check_runtime_permissions(
                    &token,
                    &[need(&computer, RuntimeKind::Computer, permission)]
                )
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
    assert!(matches!(
        db.store
            .runtime_access(&definition, RuntimeKind::Computer, &computer)
            .await,
        Err(Error::Forbidden)
    ));
}

#[tokio::test]
async fn exact_organization_principal_kind_and_resource_match_is_required() {
    let (db, _, token, computer, workspace) = runtime_fixture().await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Read,
        None,
    )
    .await;
    let stranger = runtime_token(&db, "acme", "stranger", &[ServiceScope::RuntimeRead]).await;
    let foreign = runtime_token(&db, "other", "alice", &[ServiceScope::RuntimeRead]).await;
    for bearer in [stranger, foreign] {
        assert!(matches!(
            db.store
                .runtime_access(&bearer, RuntimeKind::Computer, &computer)
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
    for (kind, id) in [
        (RuntimeKind::Workspace, workspace.as_str()),
        (RuntimeKind::Workspace, computer.as_str()),
        (RuntimeKind::Computer, "does-not-exist"),
    ] {
        assert!(matches!(
            db.store.runtime_access(&token, kind, id).await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
    let profile = db
        .store
        .register_catalog_reference(
            &org("acme"),
            DefinitionKind::BrowserProfile,
            "private-profile",
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .runtime_access(&token, RuntimeKind::BrowserProfile, &profile)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    allow(
        &db,
        &profile,
        RuntimeKind::BrowserProfile,
        RuntimePermission::AppUse,
        None,
    )
    .await;
    db.store
        .check_runtime_permissions(
            &token,
            &[need(
                &profile,
                RuntimeKind::BrowserProfile,
                RuntimePermission::AppUse,
            )],
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .runtime_access(&token, RuntimeKind::BrowserProfile, &profile)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    db.store
        .disable_catalog_reference(&org("acme"), &profile)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .check_runtime_permissions(
                &token,
                &[need(
                    &profile,
                    RuntimeKind::BrowserProfile,
                    RuntimePermission::AppUse
                )]
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
}

#[tokio::test]
async fn effective_access_intersects_grants_with_credential_scopes() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    for permission in [
        RuntimePermission::Read,
        RuntimePermission::Observe,
        RuntimePermission::Modify,
    ] {
        allow(&db, &computer, RuntimeKind::Computer, permission, None).await;
    }
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Activate,
        Some(120),
    )
    .await;
    let narrow = runtime_token(
        &db,
        "acme",
        "alice",
        &[ServiceScope::RuntimeRead, ServiceScope::RuntimeObserve],
    )
    .await;
    let view = db
        .store
        .runtime_access(&narrow, RuntimeKind::Computer, &computer)
        .await
        .unwrap();
    assert_eq!(
        view.permissions,
        vec![RuntimePermission::Observe, RuntimePermission::Read]
    );
    assert_eq!(view.max_runtime_seconds, None);
    assert!(matches!(
        db.store
            .check_runtime_permissions(
                &narrow,
                &[need(
                    &computer,
                    RuntimeKind::Computer,
                    RuntimePermission::Modify
                )]
            )
            .await,
        Err(Error::Forbidden)
    ));
    let view = db
        .store
        .runtime_access(&token, RuntimeKind::Computer, &computer)
        .await
        .unwrap();
    assert_eq!(view.max_runtime_seconds, Some(120));
    assert!(view.permissions.contains(&RuntimePermission::Modify));
    assert!(view.checked_at_ms > 0);
}

#[tokio::test]
async fn activation_requires_explicit_bounded_budget_and_budget_updates_take_effect() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    let invalid = need(
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Activate,
    );
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, std::slice::from_ref(&invalid))
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
    for cap in [None, Some(0), Some(86401)] {
        assert!(matches!(
            db.store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org("acme"),
                        principal: &principal("alice"),
                        kind: RuntimeKind::Computer,
                        resource_id: &computer,
                        permission: RuntimePermission::Activate,
                        max_runtime_seconds: cap
                    },
                    true
                )
                .await,
            Err(Error::InvalidRuntimeRequest)
        ));
    }
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Activate,
        Some(120),
    )
    .await;
    let mut wanted = invalid;
    wanted.runtime_seconds = Some(120);
    db.store
        .check_runtime_permissions(&token, std::slice::from_ref(&wanted))
        .await
        .unwrap();
    wanted.runtime_seconds = Some(121);
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, std::slice::from_ref(&wanted))
            .await,
        Err(Error::RuntimeBudgetExceeded)
    ));
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Activate,
        Some(60),
    )
    .await;
    wanted.runtime_seconds = Some(120);
    assert!(matches!(
        db.store.check_runtime_permissions(&token, &[wanted]).await,
        Err(Error::RuntimeBudgetExceeded)
    ));
}

#[tokio::test]
async fn invalid_targets_combinations_and_duplicate_requirements_fail_closed() {
    let (db, _, token, computer, workspace) = runtime_fixture().await;
    for (kind, id, permission) in [
        (RuntimeKind::Computer, "*", RuntimePermission::Read),
        (RuntimeKind::Computer, "../x", RuntimePermission::Read),
        (
            RuntimeKind::Workspace,
            workspace.as_str(),
            RuntimePermission::Execute,
        ),
    ] {
        assert!(matches!(
            db.store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org("acme"),
                        principal: &principal("alice"),
                        kind,
                        resource_id: id,
                        permission,
                        max_runtime_seconds: None
                    },
                    true
                )
                .await,
            Err(Error::InvalidRuntimeRequest)
        ));
    }
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Read,
        None,
    )
    .await;
    let read = need(&computer, RuntimeKind::Computer, RuntimePermission::Read);
    assert!(matches!(
        db.store.check_runtime_permissions(&token, &[]).await,
        Err(Error::InvalidRuntimeRequest)
    ));
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, &vec![read.clone(); 33])
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, &[read.clone(), read])
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
    let mut inappropriate = need(&computer, RuntimeKind::Computer, RuntimePermission::Read);
    inappropriate.runtime_seconds = Some(1);
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, &[inappropriate])
            .await,
        Err(Error::InvalidRuntimeRequest)
    ));
}

#[tokio::test]
async fn grant_event_and_outbox_are_atomic_and_exact_repeats_emit_no_event() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    let before = count(&db, "events").await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Read,
        None,
    )
    .await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Read,
        None,
    )
    .await;
    assert_eq!(count(&db, "events").await, before + 1);
    sqlx::query("CREATE FUNCTION reject_runtime_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END $$").execute(&db.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_runtime_outbox BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_runtime_outbox()").execute(&db.pool).await.unwrap();
    let revoke = RuntimeGrant {
        organization: &org("acme"),
        principal: &principal("alice"),
        kind: RuntimeKind::Computer,
        resource_id: &computer,
        permission: RuntimePermission::Read,
        max_runtime_seconds: None,
    };
    assert!(matches!(
        db.store.set_runtime_grant(revoke, false).await,
        Err(Error::Database(_))
    ));
    db.store
        .runtime_access(&token, RuntimeKind::Computer, &computer)
        .await
        .unwrap();
    assert_eq!(count(&db, "events").await, before + 1);
    sqlx::query("DROP TRIGGER reject_runtime_outbox ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    for _ in 0..2 {
        db.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org("acme"),
                    principal: &principal("alice"),
                    kind: RuntimeKind::Computer,
                    resource_id: &computer,
                    permission: RuntimePermission::Read,
                    max_runtime_seconds: None,
                },
                false,
            )
            .await
            .unwrap();
    }
    assert_eq!(count(&db, "events").await, before + 2);
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM events WHERE kind='access.revoked' ORDER BY sequence DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(payload["process_termination_confirmed"], false);
    assert!(matches!(
        db.store
            .runtime_access(&token, RuntimeKind::Computer, &computer)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
}

async fn wait_for_lock(db: &Database) {
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name='agent_computer_store_tests' AND wait_event_type='Lock'").fetch_one(&db.pool).await.unwrap();
            if waiting>0 {break;} tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}
#[tokio::test]
async fn grant_revocation_winning_stream_lock_blocks_waiting_authorization() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Read,
        None,
    )
    .await;
    let mut revoke = db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT last_sequence FROM organization_streams WHERE organization='acme' FOR UPDATE",
    )
    .execute(&mut *revoke)
    .await
    .unwrap();
    sqlx::query("DELETE FROM runtime_grants WHERE organization='acme'")
        .execute(&mut *revoke)
        .await
        .unwrap();
    let store = db.store.clone();
    let checking = tokio::spawn(async move {
        store
            .runtime_access(&token, RuntimeKind::Computer, &computer)
            .await
    });
    wait_for_lock(&db).await;
    revoke.commit().await.unwrap();
    assert!(matches!(
        checking.await.unwrap(),
        Err(Error::RuntimeAccessUnavailable)
    ));
}
#[tokio::test]
async fn credential_revocation_is_rechecked_after_request_time_authentication() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Read,
        None,
    )
    .await;
    let mut revoke = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE service_credentials SET revoked=TRUE WHERE organization='acme'")
        .execute(&mut *revoke)
        .await
        .unwrap();
    let store = db.store.clone();
    let checking = tokio::spawn(async move {
        store
            .runtime_access(&token, RuntimeKind::Computer, &computer)
            .await
    });
    wait_for_lock(&db).await;
    revoke.commit().await.unwrap();
    assert!(matches!(
        checking.await.unwrap(),
        Err(Error::Unauthenticated)
    ));
}
#[tokio::test]
async fn requirements_cover_all_resources_and_disabled_principals_lose_access() {
    let (db, _, token, computer, workspace) = runtime_fixture().await;
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Modify,
        None,
    )
    .await;
    let requirements = [
        need(&computer, RuntimeKind::Computer, RuntimePermission::Modify),
        need(
            &workspace,
            RuntimeKind::Workspace,
            RuntimePermission::Modify,
        ),
    ];
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, &requirements)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    allow(
        &db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Modify,
        None,
    )
    .await;
    db.store
        .check_runtime_permissions(&token, &requirements)
        .await
        .unwrap();
    db.store
        .disable_principal(&org("acme"), &principal("alice"))
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .check_runtime_permissions(&token, &requirements)
            .await,
        Err(Error::Unauthenticated)
    ));
}
