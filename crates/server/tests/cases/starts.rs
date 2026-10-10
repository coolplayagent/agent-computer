use crate::support::*;
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_store::{auth::ServiceScope, plans::*, runtime::*};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};

fn request(token: &str, method: &str, path: &str, key: Option<&str>, body: Value) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn queued_start_http_contract_validates_scope_identity_revisions_retry_and_cancel() {
    let service = Service::new().await;
    let credential = service.issue("alice", &ServiceScope::ALL).await;
    let token = credential.expose_token();
    let org = OrganizationId::new("acme").unwrap();
    let actor = PrincipalId::new("alice").unwrap();
    for kind in [
        DefinitionKind::Declaration,
        DefinitionKind::Computer,
        DefinitionKind::Workspace,
        DefinitionKind::Volume,
    ] {
        service
            .store
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
    service
        .store
        .register_catalog_reference(&org, DefinitionKind::StorageClass, "storage")
        .await
        .unwrap();
    service
        .store
        .set_definition_grant(
            DefinitionGrant {
                organization: &org,
                principal: &actor,
                kind: DefinitionKind::StorageClass,
                name: "storage",
                permission: DefinitionPermission::Reference,
            },
            true,
        )
        .await
        .unwrap();
    let definition = json!({"apiVersion":"agent-computer/v1alpha1", "kind":"ComputerSet", "metadata":{"name":"start"}, "spec":{
        "volumes":[{"name":"volume","storageClass":"storage","quotaBytes":10737418240_i64,"reclaimPolicy":"Retain"}],
        "workspaces":[{"name":"workspace","volumeRef":"volume","conflictPolicy":"explicit"}],
        "computers":[{"name":"computer","workspaceRef":"workspace","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"}]
    }});
    let plan = service
        .store
        .create_definition_plan(
            token,
            &IdempotencyKey::new("plan").unwrap(),
            &validate_bytes(&serde_json::to_vec(&definition).unwrap(), Format::Json).unwrap(),
        )
        .await
        .unwrap();
    service
        .store
        .apply_definition_plan(
            token,
            &IdempotencyKey::new("apply").unwrap(),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    let computer = &plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Computer)
        .unwrap()
        .resource_id;
    let path = format!("/v1alpha1/computers/{computer}");
    let body = json!({"expected_revision":1,"expected_spec_revision":1,"max_runtime_seconds":300});
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                &format!("{path}/start"),
                Some("start"),
                body.clone()
            ))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for resource in &plan.resources {
        let (kind, permissions): (RuntimeKind, &[RuntimePermission]) = match resource.kind {
            DefinitionKind::Computer => (
                RuntimeKind::Computer,
                &[
                    RuntimePermission::Read,
                    RuntimePermission::Activate,
                    RuntimePermission::Manage,
                ],
            ),
            DefinitionKind::Workspace => (
                RuntimeKind::Workspace,
                &[RuntimePermission::Read, RuntimePermission::Modify],
            ),
            _ => continue,
        };
        for permission in permissions {
            service
                .store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org,
                        principal: &actor,
                        kind,
                        resource_id: &resource.resource_id,
                        permission: *permission,
                        max_runtime_seconds: (*permission == RuntimePermission::Activate)
                            .then_some(600),
                    },
                    true,
                )
                .await
                .unwrap();
        }
    }
    assert_eq!(
        service
            .send(request(
                token,
                "GET",
                &format!("{path}/runtime"),
                None,
                Value::Null
            ))
            .await
            .1["generation"],
        0
    );
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                &format!("{path}/start"),
                None,
                body.clone()
            ))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for field in [
        "organization",
        "principal",
        "candidate_id",
        "generation",
        "checkpoint",
    ] {
        let mut forged = body.clone();
        forged[field] = "forged".into();
        assert_eq!(
            service
                .send(request(
                    token,
                    "POST",
                    &format!("{path}/start"),
                    Some("forged"),
                    forged
                ))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut browser = request(
        token,
        "POST",
        &format!("{path}/start"),
        Some("browser"),
        body.clone(),
    );
    browser
        .headers_mut()
        .insert("origin", "https://untrusted.example".parse().unwrap());
    assert_eq!(service.send(browser).await.0, StatusCode::FORBIDDEN);
    for (selector, expected) in [
        (json!("artifact_unknown"), StatusCode::NOT_FOUND),
        (json!("../object-key"), StatusCode::BAD_REQUEST),
        (json!(""), StatusCode::BAD_REQUEST),
        (json!({"digest":"forged"}), StatusCode::BAD_REQUEST),
    ] {
        let mut selected = body.clone();
        selected["input_artifact_id"] = selector;
        assert_eq!(
            service
                .send(request(
                    token,
                    "POST",
                    &format!("{path}/start"),
                    Some("start"),
                    selected
                ))
                .await
                .0,
            expected
        );
    }
    let (status, receipt) = service
        .send(request(
            token,
            "POST",
            &format!("{path}/start"),
            Some("start"),
            body.clone(),
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(receipt["state"], "Queued");
    assert_eq!(receipt["generation"], 1);
    let mut null_selector = body.clone();
    null_selector["input_artifact_id"] = Value::Null;
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                &format!("{path}/start"),
                Some("start"),
                null_selector
            ))
            .await,
        (StatusCode::ACCEPTED, receipt.clone())
    );
    assert_eq!(
        receipt,
        service
            .send(request(
                token,
                "POST",
                &format!("{path}/start"),
                Some("start"),
                body.clone()
            ))
            .await
            .1
    );
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                &format!("{path}/start"),
                Some("other"),
                body
            ))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (_, current) = service
        .send(request(
            token,
            "GET",
            &format!("{path}/runtime"),
            None,
            Value::Null,
        ))
        .await;
    assert_eq!(current["ready"], false);
    assert_eq!(current["active_request"], receipt["request_id"]);
    assert!(current.get("snapshot").is_none());
    let cancel = json!({"expected_revision":2,"request_id":receipt["request_id"]});
    let narrow = service
        .issue("alice", &[ServiceScope::RuntimeActivate])
        .await;
    assert_eq!(
        service
            .send(request(
                narrow.expose_token(),
                "POST",
                &format!("{path}/start/cancel"),
                Some("cancel"),
                cancel.clone()
            ))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, stopped) = service
        .send(request(
            token,
            "POST",
            &format!("{path}/start/cancel"),
            Some("cancel"),
            cancel.clone(),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stopped["revision"], 3);
    assert!(stopped["active_request"].is_null());
    assert_eq!(
        stopped,
        service
            .send(request(
                token,
                "POST",
                &format!("{path}/start/cancel"),
                Some("cancel"),
                cancel
            ))
            .await
            .1
    );
    service
        .store
        .revoke_credential(&org, credential.id())
        .await
        .unwrap();
    assert_eq!(
        service
            .send(request(
                token,
                "GET",
                &format!("{path}/runtime"),
                None,
                Value::Null
            ))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
