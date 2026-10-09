use crate::support::*;
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{auth::ServiceScope, plans::*};
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
async fn plan_apply_and_operation_routes_bind_keys_digests_and_revisions() {
    let service = Service::new().await;
    let credential = service
        .issue("planner", &[ServiceScope::DefinitionsManage])
        .await;
    let token = credential.expose_token();
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                "/v1alpha1/plans",
                Some("forbidden"),
                document()
            ))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for kind in [DefinitionKind::Declaration, DefinitionKind::Agent] {
        service
            .store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &OrganizationId::new("acme").unwrap(),
                    principal: &PrincipalId::new("planner").unwrap(),
                    kind,
                    name: "*",
                    permission: DefinitionPermission::Create,
                },
                true,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        service
            .send(request(token, "POST", "/v1alpha1/plans", None, document()))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, plan) = service
        .send(request(
            token,
            "POST",
            "/v1alpha1/plans",
            Some("plan"),
            document(),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let plan_path = format!("/v1alpha1/plans/{}", plan["plan_id"].as_str().unwrap());
    assert_eq!(
        service
            .send(request(token, "GET", &plan_path, None, Value::Null))
            .await
            .1["plan_digest"],
        plan["plan_digest"]
    );
    let apply_path = format!("{plan_path}/apply");
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                &apply_path,
                Some("wrong"),
                json!({"plan_digest":"wrong"})
            ))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                &apply_path,
                Some("forged"),
                json!({"plan_digest":plan["plan_digest"],"principal":"victim"})
            ))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, operation) = service
        .send(request(
            token,
            "POST",
            &apply_path,
            Some("apply"),
            json!({"plan_digest":plan["plan_digest"]}),
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(operation["state"], "Queued");
    assert_eq!(operation["progress"][0]["state"], "Pending");
    assert_eq!(
        operation["progress"][0]["event_sequence"],
        operation["event_sequence"]
    );
    let retry = service
        .send(request(
            token,
            "POST",
            &apply_path,
            Some("apply"),
            json!({"plan_digest":plan["plan_digest"]}),
        ))
        .await
        .1;
    assert_eq!(retry["operation_id"], operation["operation_id"]);
    let operation_path = format!(
        "/v1alpha1/operations/{}",
        operation["operation_id"].as_str().unwrap()
    );
    assert_eq!(
        service
            .send(request(token, "GET", &operation_path, None, Value::Null))
            .await
            .1["state"],
        "Queued"
    );
    use agent_computer_store::reconciliation::{
        ClaimOutcome, ReconcileOutcome, ReconcileReason, WorkerId,
    };
    let ClaimOutcome::Claimed(lease) = service
        .store
        .claim_reconciliation(
            &OrganizationId::new("acme").unwrap(),
            &WorkerId::new("worker").unwrap(),
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap()
    else {
        panic!("expected claim")
    };
    service
        .store
        .finish_reconciliation(
            &lease,
            ReconcileOutcome::Blocked {
                reason: ReconcileReason::BackendUnavailable,
            },
        )
        .await
        .unwrap();
    let (status, blocked) = service
        .send(request(token, "GET", &operation_path, None, Value::Null))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(blocked["state"], "Blocked");
    assert_eq!(blocked["progress"][0]["reason"], "backend_unavailable");
    assert_eq!(blocked["progress"][0]["attempts"], 1);
    assert!(
        !blocked["progress"][0]["dispatch_started"]
            .as_bool()
            .unwrap()
    );
    assert!(
        blocked["watermark"].as_i64().unwrap()
            >= blocked["progress"][0]["event_sequence"].as_i64().unwrap()
    );
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                "/v1alpha1/plans",
                Some("missing-cas"),
                document()
            ))
            .await
            .0,
        StatusCode::PRECONDITION_REQUIRED
    );
    let mut changed = document();
    changed["metadata"]["expectedRevision"] = 2.into();
    changed["spec"]["agents"][0]["expectedRevision"] = 1.into();
    assert_eq!(
        service
            .send(request(
                token,
                "POST",
                "/v1alpha1/plans",
                Some("wrong-cas"),
                changed
            ))
            .await
            .0,
        StatusCode::PRECONDITION_FAILED
    );
    let other = service
        .issue("other", &[ServiceScope::DefinitionsManage])
        .await;
    assert_eq!(
        service
            .send(request(
                other.expose_token(),
                "GET",
                &plan_path,
                None,
                Value::Null
            ))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        service
            .send(request(
                token,
                "GET",
                "/v1alpha1/plans/%FF",
                None,
                Value::Null
            ))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}
