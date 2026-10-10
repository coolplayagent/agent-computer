use super::connections::{provision, req};
use super::writers::prepared;
use crate::support::*;
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_store::{
    auth::{IssueCredential, PrincipalKind, ServiceScope},
    runtime::{connections::*, *},
};
use axum::http::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn prepared_stop_http_authority_input_validation_and_replay() {
    let s = Service::new().await;
    let credential = s
        .store
        .issue_credential(IssueCredential {
            organization: &OrganizationId::new("acme").unwrap(),
            principal: &PrincipalId::new("alice").unwrap(),
            kind: PrincipalKind::Human,
            scopes: &ServiceScope::ALL,
            lifetime: std::time::Duration::from_secs(3600),
        })
        .await
        .unwrap();
    let token = credential.expose_token();
    let computer = provision(&s.store, token, "alice").await;
    let start = prepared(&s.store, &s.database.pool, token, &computer, "alice").await;
    let path = format!("/v1alpha1/computers/{computer}/stop");
    let current = s.store.computer_runtime(token, &computer).await.unwrap();
    let body = json!({"expected_revision":current.revision,"request_id":start.request_id});
    assert_eq!(
        s.send(req(token, "POST", &path, Some("stop"), body.clone()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let org = OrganizationId::new("acme").unwrap();
    s.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org,
                principal: &PrincipalId::new("alice").unwrap(),
                kind: RuntimeKind::Computer,
                resource_id: &computer,
                permission: RuntimePermission::Manage,
                max_runtime_seconds: None,
            },
            true,
        )
        .await
        .unwrap();
    let narrow = s
        .store
        .issue_credential(IssueCredential {
            organization: &org,
            principal: &PrincipalId::new("alice").unwrap(),
            kind: PrincipalKind::Human,
            scopes: &[ServiceScope::RuntimeActivate],
            lifetime: std::time::Duration::from_secs(3600),
        })
        .await
        .unwrap();
    assert_eq!(
        s.send(req(
            narrow.expose_token(),
            "POST",
            &path,
            Some("stop"),
            body.clone()
        ))
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s.send(req(token, "POST", &path, None, body.clone()))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for field in [
        "organization",
        "principal",
        "force",
        "fencing_proof",
        "storage_drained",
    ] {
        let mut forged = body.clone();
        forged[field] = true.into();
        assert_eq!(
            s.send(req(token, "POST", &path, Some("forged"), forged))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut browser = req(token, "POST", &path, Some("browser"), body.clone());
    browser
        .headers_mut()
        .insert("origin", "https://untrusted.invalid".parse().unwrap());
    assert_eq!(s.send(browser).await.0, StatusCode::FORBIDDEN);
    let session = s
        .store
        .create_connection_session(
            token,
            &IdempotencyKey::new("session").unwrap(),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![RuntimePermission::Connect, RuntimePermission::Read],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    s.store
        .heartbeat_connection_session(
            token,
            &IdempotencyKey::new("active").unwrap(),
            &session.session_id,
            &ConnectionHeartbeat {
                expected_revision: 1,
                activity: ConnectionActivity::Active,
                visibility: ConnectionVisibility::Visible,
            },
        )
        .await
        .unwrap();
    let (code, blocked) = s
        .send(req(token, "POST", &path, Some("stop"), body.clone()))
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(blocked["code"], "runtime_stop_blocked");
    s.store
        .close_connection_session(token, &session.session_id)
        .await
        .unwrap();
    let (code, stopped) = s
        .send(req(token, "POST", &path, Some("stop"), body.clone()))
        .await;
    assert_eq!(code, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["proof"], "no_user_dispatch");
    assert_eq!(stopped["candidate_id"], start.candidate_id);
    assert_eq!(stopped["control_revision"], current.revision + 1);
    assert_eq!(
        s.send(req(token, "POST", &path, Some("stop"), body.clone()))
            .await
            .1,
        stopped
    );
    let (code, runtime) = s
        .send(req(
            token,
            "GET",
            &format!("/v1alpha1/computers/{computer}/runtime"),
            None,
            Value::Null,
        ))
        .await;
    assert_eq!(code, StatusCode::OK);
    assert!(runtime["active_request"].is_null());
    assert_eq!(runtime["stop_receipt"], stopped);
    assert_eq!(runtime["ready"], false);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM events e JOIN outbox o USING(organization,sequence) WHERE e.kind='computer.stopped'").fetch_one(&s.database.pool).await.unwrap(), 1);
    s.store
        .revoke_credential(&org, credential.id())
        .await
        .unwrap();
    assert_eq!(
        s.send(req(token, "POST", &path, Some("stop"), body))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
