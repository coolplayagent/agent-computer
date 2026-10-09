use crate::support::*;
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{auth::ServiceScope, plans::DefinitionKind, runtime::*};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
fn request(path: &str, token: Option<&str>, origin: bool) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if origin {
        builder = builder.header("origin", "https://untrusted.example");
    }
    builder.body(Body::empty()).unwrap()
}
#[tokio::test]
async fn runtime_access_requires_read_grant_and_scoped_credential_and_rechecks_revocation() {
    let service = Service::new().await;
    let org = OrganizationId::new("acme").unwrap();
    let alice = PrincipalId::new("alice").unwrap();
    let credential = service
        .issue(
            "alice",
            &[ServiceScope::RuntimeRead, ServiceScope::RuntimeAppUse],
        )
        .await;
    let definition = service
        .issue("alice", &[ServiceScope::DefinitionsManage])
        .await;
    let stranger = service
        .issue("stranger", &[ServiceScope::RuntimeRead])
        .await;
    let id = service
        .store
        .register_catalog_reference(&org, DefinitionKind::BrowserProfile, "private-profile")
        .await
        .unwrap();
    let path = format!("/v1alpha1/runtime-access/browser_profile/{id}");
    assert_eq!(
        service.send(request(&path, None, false)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        service
            .send(request(&path, Some(definition.expose_token()), false))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        service
            .send(request(&path, Some(credential.expose_token()), false))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for permission in [
        RuntimePermission::Read,
        RuntimePermission::AppUse,
        RuntimePermission::Manage,
    ] {
        service
            .store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org,
                    principal: &alice,
                    kind: RuntimeKind::BrowserProfile,
                    resource_id: &id,
                    permission,
                    max_runtime_seconds: None,
                },
                true,
            )
            .await
            .unwrap();
    }
    let (status, body) = service
        .send(request(&path, Some(credential.expose_token()), false))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["permissions"], serde_json::json!(["app.use", "read"]));
    assert_eq!(body["resource_id"], id);
    assert!(body["max_runtime_seconds"].is_null());
    assert_eq!(
        service
            .send(request(&path, Some(credential.expose_token()), true))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        service
            .send(request(
                &format!("{path}?principal=alice"),
                Some(stranger.expose_token()),
                false
            ))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        service
            .send(request(
                "/v1alpha1/runtime-access/secret/unknown",
                Some(credential.expose_token()),
                false
            ))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    service
        .store
        .revoke_credential(&org, credential.id())
        .await
        .unwrap();
    assert_eq!(
        service
            .send(request(&path, Some(credential.expose_token()), false))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
