use crate::support::*;
use agent_computer_store::auth::ServiceScope;
use agent_computer_worker::files::{Configuration, FILE_IO_SLOTS, FileGateway};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::time::Duration;

fn config() -> Configuration {
    serde_json::from_value(json!({"mount_root":"/not-mounted","target":{"volume_id":"volume","namespace_uid":"namespace","pvc_uid":"pvc","pv_uid":"pv","filesystem_uuid":"filesystem","volume_path":"volume-path","writer_uid":1000,"writer_gid":1000}})).unwrap()
}
fn req(token: &str, method: &str, path: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}
fn read_path() -> &'static str {
    "/v1alpha1/workspaces/workspace/files?connection_session_id=conn&generation=1&candidate_id=candidate&path=note.txt"
}
fn save() -> Value {
    json!({"lease":{"connection_session_id":"conn","generation":1,"epoch":1,"expected_revision":1},"dispatch_id":"save","edit":{"path":"note.txt","expected":null,"content":[72,105],"executable":false}})
}
#[tokio::test]
async fn file_routes_require_authentication_and_explicit_gateway_configuration() {
    let mut s = Service::new().await;
    let issued = s.issue("reader", &ServiceScope::ALL).await;
    let token = issued.expose_token();
    assert_eq!(
        s.send(req("invalid", "GET", read_path(), Body::empty()))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = s.send(req(token, "GET", read_path(), Body::empty())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "files_unavailable");
    let caps = || {
        Request::builder()
            .uri("/v1alpha1/capabilities")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        s.send(caps()).await.1["capabilities"]["files.read"],
        "unsupported"
    );
    s.router = agent_computer_server::router_with_files(
        s.store.clone(),
        FileGateway::new(vec![config()]).unwrap(),
    );
    assert_eq!(
        s.send(caps()).await.1["capabilities"]["files.read"],
        "bounded-candidate"
    );
    assert_eq!(
        s.send(req(token, "GET", read_path(), Body::empty()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.send(req(
            token,
            "POST",
            "/v1alpha1/leases/lease/file",
            save().to_string()
        ))
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let limited = s.issue("reader", &[ServiceScope::RuntimeConnect]).await;
    assert_eq!(
        s.send(req(
            limited.expose_token(),
            "GET",
            read_path(),
            Body::empty()
        ))
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut origin = req(token, "GET", read_path(), Body::empty());
    origin
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    assert_eq!(s.send(origin).await.0, StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn file_requests_reject_ambiguous_queries_unknown_fields_and_oversize_bodies() {
    let mut s = Service::new().await;
    s.router = agent_computer_server::router_with_files(
        s.store.clone(),
        FileGateway::new(vec![config()]).unwrap(),
    );
    let issued = s.issue("reader", &ServiceScope::ALL).await;
    let token = issued.expose_token();
    for path in [
        format!("{}&path=other", read_path()),
        format!("{}&host_path=secret", read_path()),
        "/v1alpha1/workspaces/workspace/files?path=note".into(),
    ] {
        assert_eq!(
            s.send(req(token, "GET", &path, Body::empty())).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut input = save();
    input["process_stopped"] = json!(true);
    assert_eq!(
        s.send(req(
            token,
            "POST",
            "/v1alpha1/leases/lease/file",
            input.to_string()
        ))
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut request = req(
        token,
        "POST",
        "/v1alpha1/leases/lease/file",
        save().to_string(),
    );
    request
        .headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    assert_eq!(s.send(request).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        s.send(req(
            token,
            "POST",
            "/v1alpha1/leases/lease/file",
            " ".repeat(5 * 1024 * 1024 + 1)
        ))
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}
#[tokio::test]
async fn blocked_file_jobs_retain_slots_after_caller_abort_and_preserve_control_routes() {
    let mut s = Service::new().await;
    let gateway = FileGateway::new(vec![config()]).unwrap();
    s.router = agent_computer_server::router_with_files(s.store.clone(), gateway.clone());
    let issued = s.issue("reader", &ServiceScope::ALL).await;
    let occupied: Vec<_> = (0..FILE_IO_SLOTS - 1)
        .map(|_| gateway.admit().unwrap())
        .collect();
    let slot = gateway.admit().unwrap();
    let (begun, started) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(async move {
        slot.run(async move {
            begun.send(()).unwrap();
            wait.await.unwrap();
            Ok(())
        })
        .await
    });
    started.await.unwrap();
    caller.abort();
    let _ = caller.await;
    assert!(gateway.admit().is_err());
    let (status, body) = s
        .send(req(
            issued.expose_token(),
            "GET",
            read_path(),
            Body::empty(),
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "file_io_busy");
    assert_eq!(
        s.send(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .0,
        StatusCode::OK
    );
    release.send(()).unwrap();
    let recovered = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(slot) = gateway.admit() {
                break slot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop((recovered, occupied));
    assert!(gateway.admit().is_ok());
}
#[test]
fn gateway_configuration_rejects_empty_duplicate_and_relative_mounts() {
    assert!(FileGateway::new(vec![]).is_err());
    assert!(FileGateway::new(vec![config(); 17]).is_err());
    assert!(FileGateway::new(vec![config(), config()]).is_err());
    let mut c = config();
    c.mount_root = "relative".into();
    assert!(FileGateway::new(vec![c]).is_err());
}
