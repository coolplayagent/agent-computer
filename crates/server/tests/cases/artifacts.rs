use super::connections::{provision, req};
use super::writers::prepared;
use crate::support::*;
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{
    auth::{IssueCredential, PrincipalKind, ServiceScope},
    runtime::*,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
#[tokio::test]
async fn artifact_http_authority_strict_input_and_durable_sealing() {
    check_admission(false).await;
}
#[tokio::test]
async fn checkpoint_stop_http_authority_strict_input_and_durable_acceptance() {
    check_admission(true).await;
}
async fn check_admission(checkpoint: bool) {
    let s = Service::new().await;
    let org = OrganizationId::new("acme").unwrap();
    let principal = PrincipalId::new("alice").unwrap();
    let credential = s
        .store
        .issue_credential(IssueCredential {
            organization: &org,
            principal: &principal,
            kind: PrincipalKind::Human,
            scopes: &ServiceScope::ALL,
            lifetime: std::time::Duration::from_secs(3600),
        })
        .await
        .unwrap();
    let token = credential.expose_token();
    let computer = provision(&s.store, token, "alice").await;
    let start = prepared(&s.store, &s.database.pool, token, &computer, "alice").await;
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&s.database.pool)
        .await
        .unwrap();
    let current = s.store.computer_runtime(token, &computer).await.unwrap();
    let body = if checkpoint {
        json!({"request_id":start.request_id,"expected_revision":current.revision,"publish_current":true})
    } else {
        json!({"request_id":start.request_id,"expected_revision":current.revision,"base_revision":start.input_revision,"base_manifest":start.input_manifest_digest,"publish_current":true})
    };
    let path = if checkpoint {
        format!("/v1alpha1/computers/{computer}/checkpoint-stop")
    } else {
        format!("/v1alpha1/workspaces/{workspace}/artifacts")
    };
    assert_eq!(
        s.send(req(token, "POST", &path, Some("artifact"), body.clone()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    s.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org,
                principal: &principal,
                kind: RuntimeKind::Workspace,
                resource_id: &workspace,
                permission: RuntimePermission::Publish,
                max_runtime_seconds: None,
            },
            true,
        )
        .await
        .unwrap();
    if checkpoint {
        s.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org,
                    principal: &principal,
                    kind: RuntimeKind::Computer,
                    resource_id: &computer,
                    permission: RuntimePermission::Manage,
                    max_runtime_seconds: None,
                },
                true,
            )
            .await
            .unwrap();
    }
    let narrow = s
        .store
        .issue_credential(IssueCredential {
            organization: &org,
            principal: &principal,
            kind: PrincipalKind::Human,
            scopes: &[ServiceScope::RuntimeRead],
            lifetime: std::time::Duration::from_secs(300),
        })
        .await
        .unwrap();
    assert_eq!(
        s.send(req(
            narrow.expose_token(),
            "POST",
            &path,
            Some("artifact"),
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
        "manifest",
        "object_ref",
        "organization",
        "principal",
        "drain_confirmed",
        "state",
        "force",
        "stop_after_commit",
    ] {
        let mut bad = body.clone();
        bad[field] = true.into();
        assert_eq!(
            s.send(req(token, "POST", &path, Some("bad"), bad)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    if checkpoint {
        use agent_computer_core::identity::IdempotencyKey;
        use agent_computer_store::runtime::connections::*;
        s.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org,
                    principal: &principal,
                    kind: RuntimeKind::Computer,
                    resource_id: &computer,
                    permission: RuntimePermission::Connect,
                    max_runtime_seconds: None,
                },
                true,
            )
            .await
            .unwrap();
        let connection = s
            .store
            .create_connection_session(
                token,
                &IdempotencyKey::new("active-input").unwrap(),
                &computer,
                &ConnectRequest {
                    requested_capabilities: vec![RuntimePermission::Connect],
                    lifetime_seconds: 300,
                },
            )
            .await
            .unwrap();
        s.store
            .heartbeat_connection_session(
                token,
                &IdempotencyKey::new("active-input-heartbeat").unwrap(),
                &connection.session_id,
                &ConnectionHeartbeat {
                    expected_revision: connection.revision,
                    activity: ConnectionActivity::Active,
                    visibility: ConnectionVisibility::Visible,
                },
            )
            .await
            .unwrap();
        let (code, error) = s
            .send(req(token, "POST", &path, Some("artifact"), body.clone()))
            .await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(error["code"], "active_use");
        s.store
            .close_connection_session(token, &connection.session_id)
            .await
            .unwrap();
    }
    let (code, admitted) = s
        .send(req(token, "POST", &path, Some("artifact"), body.clone()))
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{admitted}");
    assert_eq!(admitted["state"], "Capturing");
    assert_eq!(admitted["stop_after_commit"], checkpoint);
    assert!(admitted.get("stop_receipt").is_none());
    assert_eq!(
        s.send(req(token, "POST", &path, Some("artifact"), body))
            .await
            .1,
        admitted
    );
    let id = admitted["commit_id"].as_str().unwrap();
    let artifact_path = format!("/v1alpha1/artifacts/{id}");
    assert_eq!(
        s.send(req(token, "GET", &artifact_path, None, Value::Null))
            .await
            .1,
        admitted
    );
    assert_eq!(
        s.send(req(
            token,
            "GET",
            &format!("{artifact_path}/manifest"),
            None,
            Value::Null
        ))
        .await,
        (StatusCode::OK, Value::Null)
    );
    assert_eq!(
        s.store
            .computer_runtime(token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Sealing)
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM events e JOIN outbox o USING(organization,sequence) WHERE kind='artifact.sealing'").fetch_one(&s.database.pool).await.unwrap(),1);
    s.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org,
                principal: &principal,
                kind: RuntimeKind::Workspace,
                resource_id: &workspace,
                permission: RuntimePermission::Read,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    for endpoint in [artifact_path.clone(), format!("{artifact_path}/manifest")] {
        assert_eq!(
            s.send(req(token, "GET", &endpoint, None, Value::Null))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
}
