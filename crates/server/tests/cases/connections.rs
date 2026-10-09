use crate::support::*;
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_store::{Store, auth::ServiceScope, plans::*, runtime::*};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};

pub(super) async fn provision(store: &Store, token: &str, actor: &str) -> String {
    let org = OrganizationId::new("acme").unwrap();
    let principal = PrincipalId::new(actor).unwrap();
    for kind in [
        DefinitionKind::Declaration,
        DefinitionKind::Computer,
        DefinitionKind::Workspace,
        DefinitionKind::Volume,
    ] {
        store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &principal,
                    kind,
                    name: "*",
                    permission: DefinitionPermission::Create,
                },
                true,
            )
            .await
            .unwrap();
    }
    store
        .register_catalog_reference(&org, DefinitionKind::StorageClass, "connection-storage")
        .await
        .unwrap();
    store
        .set_definition_grant(
            DefinitionGrant {
                organization: &org,
                principal: &principal,
                kind: DefinitionKind::StorageClass,
                name: "connection-storage",
                permission: DefinitionPermission::Reference,
            },
            true,
        )
        .await
        .unwrap();
    let document = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"connections"},"spec":{
        "volumes":[{"name":"data","storageClass":"connection-storage","quotaBytes":10737418240_i64,"reclaimPolicy":"Retain"}],
        "workspaces":[{"name":"work","volumeRef":"data","conflictPolicy":"explicit"}],
        "computers":[{"name":"computer","workspaceRef":"work","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"}]
    }});
    let plan = store
        .create_definition_plan(
            token,
            &IdempotencyKey::new("connections-plan").unwrap(),
            &validate_bytes(&serde_json::to_vec(&document).unwrap(), Format::Json).unwrap(),
        )
        .await
        .unwrap();
    store
        .apply_definition_plan(
            token,
            &IdempotencyKey::new("connections-apply").unwrap(),
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
    for permission in [RuntimePermission::Connect, RuntimePermission::Read] {
        store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org,
                    principal: &principal,
                    kind: RuntimeKind::Computer,
                    resource_id: &computer,
                    permission,
                    max_runtime_seconds: None,
                },
                true,
            )
            .await
            .unwrap();
    }
    computer
}
fn req(token: &str, method: &str, path: &str, key: Option<&str>, body: Value) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(k) = key {
        r = r.header("idempotency-key", k);
    }
    r.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn connection_http_lifecycle_rechecks_authority_without_starting_compute() {
    let s = Service::new().await;
    let issued = s.issue("alice", &ServiceScope::ALL).await;
    let token = issued.expose_token();
    let computer = provision(&s.store, token, "alice").await;
    let path = format!("/v1alpha1/computers/{computer}/connection-sessions");
    let body = json!({"requested_capabilities":["connect","read","modify"]});
    let (code, session) = s
        .send(req(token, "POST", &path, Some("connect"), body.clone()))
        .await;
    assert_eq!(code, StatusCode::CREATED, "{session}");
    assert_eq!(session["capabilities"], json!(["connect", "read"]));
    assert_eq!(session["principal_kind"], "agent");
    assert_eq!(
        session["expires_at_ms"].as_i64().unwrap() - session["created_at_ms"].as_i64().unwrap(),
        900_000
    );
    let connection = format!(
        "/v1alpha1/connection-sessions/{}",
        session["session_id"].as_str().unwrap()
    );
    let hb = json!({"expected_revision":1,"activity":"active","visibility":"visible"});
    let (code, updated) = s
        .send(req(
            token,
            "POST",
            &format!("{connection}/heartbeat"),
            Some("hb"),
            hb.clone(),
        ))
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(updated["revision"], 2);
    assert_eq!(updated["expires_at_ms"], session["expires_at_ms"]);
    assert_eq!(
        s.send(req(
            token,
            "POST",
            &format!("{connection}/heartbeat"),
            Some("stale"),
            hb.clone()
        ))
        .await
        .0,
        StatusCode::CONFLICT
    );
    let closed = s
        .send(req(token, "DELETE", &connection, None, Value::Null))
        .await;
    assert_eq!(closed.0, StatusCode::OK);
    assert_eq!(closed.1["state"], "Closed");
    assert_eq!(closed.1["capabilities"], json!([]));
    assert_eq!(
        s.send(req(token, "POST", &path, Some("connect"), body))
            .await
            .1["state"],
        "Closed"
    );
    assert_eq!(
        s.send(req(
            token,
            "POST",
            &format!("{connection}/heartbeat"),
            Some("after"),
            json!({"expected_revision":3,"activity":"idle","visibility":"hidden"})
        ))
        .await
        .0,
        StatusCode::GONE
    );
    assert_eq!(
        s.send(req(token, "GET", &connection, None, Value::Null))
            .await
            .1["state"],
        "Closed"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_start_requests")
            .fetch_one(&s.database.pool)
            .await
            .unwrap(),
        0
    );
    s.store
        .revoke_credential(&OrganizationId::new("acme").unwrap(), issued.id())
        .await
        .unwrap();
    assert_eq!(
        s.send(req(token, "GET", &connection, None, Value::Null))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn connection_http_rejects_identity_spoofing_unknown_activity_browser_tokens_and_cross_owner_reads()
 {
    let s = Service::new().await;
    let token = s.issue("alice", &ServiceScope::ALL).await;
    let token = token.expose_token();
    let computer = provision(&s.store, token, "alice").await;
    let path = format!("/v1alpha1/computers/{computer}/connection-sessions");
    let body = json!({"requested_capabilities":["connect"]});
    assert_eq!(
        s.send(req(token, "POST", &path, None, body.clone()))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for field in [
        "principal_id",
        "organization",
        "agent_spec_ref",
        "caller_ref",
        "credential_id",
        "generation",
        "expires_at_ms",
    ] {
        let mut forged = body.clone();
        forged[field] = "forged".into();
        assert_eq!(
            s.send(req(token, "POST", &path, Some("bad"), forged))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut browser = req(token, "POST", &path, Some("browser"), body.clone());
    browser
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    assert_eq!(s.send(browser).await.0, StatusCode::FORBIDDEN);
    let session = s
        .send(req(token, "POST", &path, Some("connect"), body.clone()))
        .await
        .1;
    let connection = format!(
        "/v1alpha1/connection-sessions/{}",
        session["session_id"].as_str().unwrap()
    );
    let other = s.issue("bob", &ServiceScope::ALL).await;
    for method in ["GET", "DELETE"] {
        assert_eq!(
            s.send(req(
                other.expose_token(),
                method,
                &connection,
                None,
                Value::Null
            ))
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        s.send(req(
            token,
            "POST",
            &format!("{connection}/heartbeat"),
            Some("hb"),
            json!({"expected_revision":1,"activity":"typing","visibility":"visible"})
        ))
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let restricted = s.issue("alice", &[ServiceScope::RuntimeRead]).await;
    assert_eq!(
        s.send(req(
            restricted.expose_token(),
            "POST",
            &path,
            Some("restricted"),
            body
        ))
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
