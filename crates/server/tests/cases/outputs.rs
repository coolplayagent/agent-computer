use crate::support::*;
use agent_computer_store::auth::ServiceScope;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;

fn req(token: &str, path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}
fn gateway() -> (tempfile::TempDir, agent_computer_server::OutputGateway) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    std::fs::write(
        &path,
        br#"{"access_key":"fixture","secret_key":"private-fixture-secret-key"}"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let gateway =
        agent_computer_server::OutputGateway::new(&agent_computer_objects::Configuration {
            endpoint: "http://127.0.0.1:9/".into(),
            region: "test".into(),
            bucket: "outputs".into(),
            credentials_file: path,
            ca_file: None,
            allow_http: true,
        })
        .unwrap();
    (dir, gateway)
}
#[tokio::test]
async fn output_download_http_authentication_configuration_and_capabilities_are_explicit() {
    let mut s = Service::new().await;
    let credential = s.issue("alice", &ServiceScope::ALL).await;
    let token = credential.expose_token();
    let path = "/v1alpha1/executions/execution/output/stdout";
    assert_eq!(
        s.send(req("invalid", path)).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (code, body) = s.send(req(token, path)).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "outputs_unavailable");
    let caps = || {
        Request::builder()
            .uri("/v1alpha1/capabilities")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        s.send(caps()).await.1["capabilities"]["execution.output_downloads"],
        "unsupported"
    );
    let (_dir, gateway) = gateway();
    s.router = agent_computer_server::router_with_gateways(s.store.clone(), None, Some(gateway));
    let (_, caps) = s.send(caps()).await;
    assert_eq!(
        caps["capabilities"]["execution.output_downloads"],
        "bounded-verified-streams"
    );
    assert_eq!(caps["capabilities"]["execution"], "unsupported");
    assert_eq!(caps["capabilities"]["files.read"], "unsupported");
    assert_eq!(s.send(req(token, path)).await.0, StatusCode::NOT_FOUND);
    let limited = s.issue("alice", &[ServiceScope::RuntimeConnect]).await;
    assert_eq!(
        s.send(req(limited.expose_token(), path)).await.0,
        StatusCode::FORBIDDEN
    );
    let mut origin = req(token, path);
    origin
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    assert_eq!(s.send(origin).await.0, StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn output_download_http_rejects_range_queries_and_nonpublic_objects() {
    let s = Service::new().await;
    let credential = s.issue("alice", &ServiceScope::ALL).await;
    for suffix in [
        "report",
        "diagnostics",
        "stdout?key=private",
        "stderr?offset=1",
        "stdout?",
    ] {
        let (status, body) = s
            .send(req(
                credential.expose_token(),
                &format!("/v1alpha1/executions/execution/output/{suffix}"),
            ))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_request");
        assert!(!body.to_string().contains("private"));
    }
    let mut range = req(
        credential.expose_token(),
        "/v1alpha1/executions/execution/output/stdout",
    );
    range
        .headers_mut()
        .insert("range", "bytes=0-1".parse().unwrap());
    assert_eq!(s.send(range).await.0, StatusCode::BAD_REQUEST);
}
#[tokio::test]
async fn output_download_http_does_not_publish_queued_output() {
    let (mut s, token, computer, input) = super::executions::setup().await;
    let (_dir, gateway) = gateway();
    s.router = agent_computer_server::router_with_gateways(s.store.clone(), None, Some(gateway));
    let (_, queued) = s
        .send(super::connections::req(
            &token,
            "POST",
            &format!("/v1alpha1/computers/{computer}/executions"),
            Some("submit"),
            input,
        ))
        .await;
    let id = queued["execution_id"].as_str().unwrap();
    for stream in ["stdout", "stderr"] {
        let path = format!("/v1alpha1/executions/{id}/output/{stream}");
        let (status, body) = s.send(req(&token, &path)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "execution_output_unavailable");
        assert_eq!(body["retryable"], Value::Bool(true));
        let other = s.issue("alice", &ServiceScope::ALL).await;
        assert_eq!(
            s.send(req(other.expose_token(), &path)).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_dispatch_intents")
            .fetch_one(&s.database.pool)
            .await
            .unwrap(),
        0
    );
}
