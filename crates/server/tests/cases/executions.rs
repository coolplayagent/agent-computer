use super::{
    connections::{provision_with_sandbox, req},
    writers::prepared,
};
use crate::support::*;
use agent_computer_core::identity::IdempotencyKey;
use agent_computer_store::{
    auth::ServiceScope,
    runtime::{connections::*, writers::*, *},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};

async fn setup() -> (Service, String, String, Value) {
    let s = Service::new().await;
    let token = s
        .issue("alice", &ServiceScope::ALL)
        .await
        .expose_token()
        .to_string();
    let computer = provision_with_sandbox(&s.store, &token, "alice", true).await;
    let start = prepared(&s.store, &s.database.pool, &token, &computer, "alice").await;
    let session = s
        .store
        .create_connection_session(
            &token,
            &IdempotencyKey::new("connect").unwrap(),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::Modify,
                ],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    let lease = s
        .store
        .acquire_candidate_writer(
            &token,
            &IdempotencyKey::new("lease").unwrap(),
            &computer,
            &AcquireWriterLease {
                scope: WriterScope::Modify,
                connection_session_id: session.session_id,
                candidate_id: start.candidate_id,
                generation: start.generation,
                duration_seconds: 30,
            },
        )
        .await
        .unwrap();
    let sandbox: String =
        sqlx::query_scalar("SELECT resource_id FROM resource_definitions WHERE kind='sandbox'")
            .fetch_one(&s.database.pool)
            .await
            .unwrap();
    let input = json!({"lease_id":lease.lease_id,"lease":{"connection_session_id":lease.connection_session_id,"generation":lease.generation,"epoch":lease.epoch,"expected_revision":lease.revision},"sandbox_id":sandbox,"lifetime":"connection","command":{"argv":["/bin/echo","private-argv"],"cwd":"","timeout_seconds":10,"term_grace_ms":100,"output_limit_bytes":100}});
    (s, token, computer, input)
}
#[tokio::test]
async fn execution_http_queue_retry_query_and_cancel_do_not_dispatch() {
    let (s, token, computer, input) = setup().await;
    let path = format!("/v1alpha1/computers/{computer}/executions");
    let (code, queued) = s
        .send(req(&token, "POST", &path, Some("submit"), input.clone()))
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "Queued");
    assert_eq!(queued["dispatch_started"], false);
    assert!(!queued.to_string().contains("private-argv"));
    assert_eq!(
        s.send(req(&token, "POST", &path, Some("submit"), input.clone()))
            .await,
        (code, queued.clone())
    );
    let id = queued["execution_id"].as_str().unwrap();
    let get = format!("/v1alpha1/executions/{id}");
    assert_eq!(
        s.send(req(&token, "GET", &get, None, Value::Null)).await,
        (StatusCode::OK, queued.clone())
    );
    let other = s.issue("alice", &ServiceScope::ALL).await;
    let output = format!("{get}/output");
    assert_eq!(
        s.send(req(&token, "GET", &output, None, Value::Null)).await,
        (StatusCode::OK, Value::Null)
    );
    assert_eq!(
        s.send(req(other.expose_token(), "GET", &output, None, Value::Null))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.send(req(other.expose_token(), "GET", &get, None, Value::Null))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let cancel = format!("{get}/cancel");
    let body = json!({"expected_revision":queued["revision"]});
    let (code, cancelled) = s
        .send(req(&token, "POST", &cancel, Some("cancel"), body.clone()))
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(cancelled["state"], "Cancelled");
    assert_eq!(
        s.send(req(&token, "POST", &cancel, Some("cancel"), body))
            .await,
        (code, cancelled.clone())
    );
    assert_eq!(
        s.send(req(&token, "POST", &path, Some("submit"), input))
            .await,
        (StatusCode::OK, cancelled)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM candidate_writer_dispatches")
            .fetch_one(&s.database.pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn execution_http_rejects_background_forgery_unbounded_body_and_browser_origin() {
    let (s, token, computer, input) = setup().await;
    let path = format!("/v1alpha1/computers/{computer}/executions");
    for (field, value) in [
        ("lifetime", json!("background")),
        ("process_stopped", json!(true)),
        ("lease_budget_ms", json!(30000)),
    ] {
        let mut changed = input.clone();
        changed[field] = value;
        assert_eq!(
            s.send(req(&token, "POST", &path, Some("bad"), changed))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut changed = input.clone();
    changed["command"]["cwd"] = json!("../escape");
    assert_eq!(
        s.send(req(&token, "POST", &path, Some("bad"), changed))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        s.send(req(&token, "POST", &path, None, input.clone()))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut origin = req(&token, "POST", &path, Some("bad"), input);
    origin
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    assert_eq!(s.send(origin).await.0, StatusCode::FORBIDDEN);
    let big = Request::builder()
        .method("POST")
        .uri(&path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("idempotency-key", "big")
        .body(Body::from(" ".repeat(65537)))
        .unwrap();
    assert_eq!(s.send(big).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_requests")
            .fetch_one(&s.database.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn execution_http_after_dispatch_reports_cancel_request_and_unknown_without_completion() {
    let (s, token, computer, input) = setup().await;
    let path = format!("/v1alpha1/computers/{computer}/executions");
    let (code, queued) = s
        .send(req(&token, "POST", &path, Some("submit"), input.clone()))
        .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let id = queued["execution_id"].as_str().unwrap();
    let organization = agent_computer_core::identity::OrganizationId::new("acme").unwrap();
    s.store
        .begin_candidate_execution_dispatch(&organization, id, 1)
        .await
        .unwrap();
    let (code, dispatching) = s
        .send(req(&token, "POST", &path, Some("submit"), input))
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(dispatching["state"], "Dispatching");
    assert_eq!(dispatching["dispatch_started"], true);
    assert!(!dispatching.to_string().contains("private-argv"));
    let get = format!("/v1alpha1/executions/{id}");
    let cancel = format!("{get}/cancel");
    let (code, body) = s
        .send(req(
            &token,
            "POST",
            &cancel,
            Some("stale"),
            json!({"expected_revision":1}),
        ))
        .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    let (code, body) = s
        .send(req(
            &token,
            "POST",
            &cancel,
            Some("cancel"),
            json!({"expected_revision":2}),
        ))
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["state"], "CancelRequested");
    s.store
        .mark_candidate_execution_unknown(&organization, id, 3)
        .await
        .unwrap();
    let (code, body) = s.send(req(&token, "GET", &get, None, Value::Null)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["state"], "Unknown");
    assert_eq!(body["dispatch_started"], true);
    let other = s.issue("alice", &ServiceScope::ALL).await;
    assert_eq!(
        s.send(req(other.expose_token(), "GET", &get, None, Value::Null))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM candidate_writer_drains")
            .fetch_one(&s.database.pool)
            .await
            .unwrap(),
        0
    );
}
