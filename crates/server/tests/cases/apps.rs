use super::connections::req;
use crate::support::*;
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_store::{
    auth::ServiceScope,
    plans::*,
    runtime::{connections::*, *},
};
use axum::http::StatusCode;
use serde_json::{Value, json};

async fn setup() -> (Service, String, String, Value) {
    setup_shared(false).await
}
async fn setup_shared(shared: bool) -> (Service, String, String, Value) {
    let s = Service::new().await;
    let issued = s.issue("alice", &ServiceScope::ALL).await;
    let token = issued.expose_token().to_owned();
    let org = OrganizationId::new("acme").unwrap();
    let actor = PrincipalId::new("alice").unwrap();
    for kind in [
        DefinitionKind::Declaration,
        DefinitionKind::Volume,
        DefinitionKind::Workspace,
        DefinitionKind::Sandbox,
        DefinitionKind::App,
        DefinitionKind::Computer,
    ] {
        s.store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &actor,
                    kind,
                    name: "*",
                    permission: DefinitionPermission::Create,
                },
                true,
            )
            .await
            .unwrap();
    }
    for (kind, name) in [
        (DefinitionKind::StorageClass, "storage"),
        (DefinitionKind::NetworkPolicy, "deny-all"),
        (DefinitionKind::BrowserProfile, "private-profile"),
        (DefinitionKind::BrowserProfile, "second-profile"),
    ] {
        s.store
            .register_catalog_reference(&org, kind, name)
            .await
            .unwrap();
        s.store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &actor,
                    kind,
                    name,
                    permission: DefinitionPermission::Reference,
                },
                true,
            )
            .await
            .unwrap();
    }
    let mut doc = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"app-http"},"spec":{
        "volumes":[{"name":"volume","storageClass":"storage","quotaBytes":10737418240_i64,"reclaimPolicy":"Retain"}],
        "workspaces":[{"name":"workspace","volumeRef":"volume","conflictPolicy":"explicit"}],
        "sandboxes":[{"name":"browser-env","runtimeClass":"gvisor","image":format!("registry.example.invalid/browser@sha256:{}","a".repeat(64)),"resources":{"cpuMillis":2000,"memoryMiB":4096},"networkPolicyRef":"deny-all"}],
        "apps":[{"name":"browser","driver":"chromium-playwright","sandboxRef":"browser-env","profileRef":"private-profile"}],
        "computers":[{"name":"computer","workspaceRef":"workspace","sandboxRefs":["browser-env"],"appRefs":["browser"],"desiredState":"Stopped"}]
    }});
    if shared {
        let mut second = doc["spec"]["apps"][0].clone();
        second["name"] = json!("second-browser");
        second["profileRef"] = json!("second-profile");
        doc["spec"]["apps"].as_array_mut().unwrap().push(second);
        doc["spec"]["computers"][0]["appRefs"]
            .as_array_mut()
            .unwrap()
            .push(json!("second-browser"));
    }
    let plan = s
        .store
        .create_definition_plan(
            &token,
            &IdempotencyKey::new("plan").unwrap(),
            &validate_bytes(&serde_json::to_vec(&doc).unwrap(), Format::Json).unwrap(),
        )
        .await
        .unwrap();
    s.store
        .apply_definition_plan(
            &token,
            &IdempotencyKey::new("apply").unwrap(),
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
    let app = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::App && r.name == "browser")
        .unwrap()
        .resource_id
        .clone();
    for (kind, id, permissions) in [
        (
            RuntimeKind::Computer,
            &computer,
            &[
                RuntimePermission::Connect,
                RuntimePermission::Read,
                RuntimePermission::AppUse,
                RuntimePermission::Manage,
            ][..],
        ),
        (
            RuntimeKind::App,
            &app,
            &[
                RuntimePermission::Read,
                RuntimePermission::AppUse,
                RuntimePermission::Activate,
                RuntimePermission::Manage,
            ][..],
        ),
    ] {
        for permission in permissions {
            s.store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org,
                        principal: &actor,
                        kind,
                        resource_id: id,
                        permission: *permission,
                        max_runtime_seconds: (*permission == RuntimePermission::Activate)
                            .then_some(300),
                    },
                    true,
                )
                .await
                .unwrap();
        }
    }
    for profile in sqlx::query_scalar::<_, String>(
        "SELECT resource_id FROM catalog_references WHERE kind='browser_profile'",
    )
    .fetch_all(&s.database.pool)
    .await
    .unwrap()
    {
        s.store
            .register_browser_profile_owner(&org, &profile, &actor)
            .await
            .unwrap();
        s.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org,
                    principal: &actor,
                    kind: RuntimeKind::BrowserProfile,
                    resource_id: &profile,
                    permission: RuntimePermission::AppUse,
                    max_runtime_seconds: None,
                },
                true,
            )
            .await
            .unwrap();
    }
    for app in plan
        .resources
        .iter()
        .filter(|r| r.kind == DefinitionKind::App)
    {
        for permission in [
            RuntimePermission::Read,
            RuntimePermission::AppUse,
            RuntimePermission::Activate,
            RuntimePermission::Manage,
        ] {
            s.store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org,
                        principal: &actor,
                        kind: RuntimeKind::App,
                        resource_id: &app.resource_id,
                        permission,
                        max_runtime_seconds: (permission == RuntimePermission::Activate)
                            .then_some(300),
                    },
                    true,
                )
                .await
                .unwrap();
        }
    }
    // Existing helper uses explicitly synthetic preparation evidence. These tests
    // prove HTTP/database contracts, not a browser runtime or storage deployment.
    let start =
        super::writers::prepared(&s.store, &s.database.pool, &token, &computer, "alice").await;
    let connection = s
        .store
        .create_connection_session(
            &token,
            &IdempotencyKey::new("connect").unwrap(),
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
    let body = json!({"connection_session_id":connection.session_id,"computer_generation":start.generation,"app_id":app,"expected_app_revision":1,"runtime_seconds":300});
    (s, token, computer, body)
}

#[tokio::test]
async fn app_http_queue_read_retry_cancel_keeps_browser_and_computer_unready() {
    let (s, token, computer, body) = setup().await;
    let path = format!("/v1alpha1/computers/{computer}/apps");
    let (code, app) = s
        .send(req(&token, "POST", &path, Some("app"), body.clone()))
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{app}");
    assert_eq!(app["state"], "Queued");
    assert_eq!(app["ready"], false);
    assert!(app["profile_id"].is_string());
    let get = format!(
        "/v1alpha1/app-instances/{}",
        app["instance_id"].as_str().unwrap()
    );
    assert_eq!(
        s.send(req(&token, "GET", &get, None, Value::Null)).await.1["binding_digest"],
        app["binding_digest"]
    );
    assert_eq!(
        s.send(req(&token, "POST", &path, Some("app"), body.clone()))
            .await
            .1["instance_id"],
        app["instance_id"]
    );
    assert_eq!(
        s.send(req(&token, "POST", &path, Some("competing"), body.clone()))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let cancel =
        json!({"app_generation":app["app_generation"],"expected_revision":app["revision"]});
    assert_eq!(
        s.send(req(
            &token,
            "POST",
            &format!("{get}/cancel"),
            Some("cancel"),
            cancel.clone()
        ))
        .await
        .1["state"],
        "Cancelled"
    );
    assert_eq!(
        s.send(req(
            &token,
            "POST",
            &format!("{get}/cancel"),
            Some("cancel"),
            cancel
        ))
        .await
        .1["state"],
        "Cancelled"
    );
    assert_eq!(
        s.send(req(&token, "POST", &path, Some("app"), body))
            .await
            .1["state"],
        "Cancelled"
    );
    let (_, caps) = s
        .send(req(
            &token,
            "GET",
            "/v1alpha1/capabilities",
            None,
            Value::Null,
        ))
        .await;
    assert_eq!(caps["capabilities"]["app.admission"], "bounded-queued");
    assert_eq!(caps["capabilities"]["browser"], "unsupported");
    assert!(
        !s.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .ready
    );
}

#[tokio::test]
async fn app_http_rejects_forged_authority_oversize_bodies_origin_and_cross_owner_access() {
    let (s, token, computer, body) = setup().await;
    let path = format!("/v1alpha1/computers/{computer}/apps");
    assert_eq!(
        s.send(req(&token, "POST", &path, None, body.clone()))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for field in [
        "organization",
        "principal_id",
        "credential_id",
        "profile_owner_id",
        "expires_at_ms",
        "ready",
        "binding",
        "control_epoch",
    ] {
        let mut forged = body.clone();
        forged[field] = json!("forged");
        assert_eq!(
            s.send(req(&token, "POST", &path, Some("bad"), forged))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut oversized = body.clone();
    oversized["app_id"] = json!("a".repeat(4097));
    assert_eq!(
        s.send(req(&token, "POST", &path, Some("large"), oversized))
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut browser = req(&token, "POST", &path, Some("browser"), body.clone());
    browser
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    assert_eq!(s.send(browser).await.0, StatusCode::FORBIDDEN);
    let restricted = s.issue("alice", &[ServiceScope::RuntimeRead]).await;
    assert_eq!(
        s.send(req(
            restricted.expose_token(),
            "POST",
            &path,
            Some("restricted"),
            body.clone()
        ))
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, app) = s.send(req(&token, "POST", &path, Some("app"), body)).await;
    let get = format!(
        "/v1alpha1/app-instances/{}",
        app["instance_id"].as_str().unwrap()
    );
    let bob = s.issue("bob", &ServiceScope::ALL).await;
    assert_eq!(
        s.send(req(bob.expose_token(), "GET", &get, None, Value::Null))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.send(req(
            bob.expose_token(),
            "POST",
            &format!("{get}/cancel"),
            Some("bob"),
            json!({"app_generation":1,"expected_revision":1})
        ))
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let text = app.to_string();
    assert!(!text.contains(&token));
    assert!(!text.contains("registry.example.invalid"));
}

#[tokio::test]
async fn two_apps_with_distinct_profiles_cannot_spend_one_sandbox_reservation() {
    let (s, token, computer, first) = setup_shared(true).await;
    let mut second = first.clone();
    second["app_id"]=json!(sqlx::query_scalar::<_,String>("SELECT resource_id FROM resource_definitions WHERE kind='app' AND name='second-browser'").fetch_one(&s.database.pool).await.unwrap());
    let path = format!("/v1alpha1/computers/{computer}/apps");
    let (code, app) = s
        .send(req(&token, "POST", &path, Some("first"), first))
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{app}");
    let (code, conflict) = s
        .send(req(&token, "POST", &path, Some("second"), second.clone()))
        .await;
    assert_eq!(code, StatusCode::CONFLICT, "{conflict}");
    let cancel = format!(
        "/v1alpha1/app-instances/{}/cancel",
        app["instance_id"].as_str().unwrap()
    );
    assert_eq!(
        s.send(req(
            &token,
            "POST",
            &cancel,
            Some("release"),
            json!({"app_generation":1,"expected_revision":1})
        ))
        .await
        .0,
        StatusCode::OK
    );
    let (code, next) = s
        .send(req(&token, "POST", &path, Some("second"), second))
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{next}");
    assert_eq!(next["sandbox_id"], app["sandbox_id"]);
    assert_ne!(next["profile_id"], app["profile_id"]);
    assert_ne!(next["app_id"], app["app_id"]);
}
