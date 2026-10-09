use crate::support::*;
use agent_computer_store::auth::ServiceScope;
use axum::{
    body::Body,
    http::{HeaderValue, Request, StatusCode},
};

#[tokio::test]
async fn error_envelopes_body_limits_media_types_and_unknown_fields() {
    let service = Service::new().await;
    let issued = service
        .issue("reader", &[ServiceScope::DefinitionsValidate])
        .await;
    let mut request = validate_request(Some(issued.expose_token()), document().to_string());
    request
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("text/plain"));
    assert_eq!(
        service.send(request).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let mut request = validate_request(Some(issued.expose_token()), document().to_string());
    request
        .headers_mut()
        .insert("content-encoding", HeaderValue::from_static("gzip"));
    assert_eq!(
        service.send(request).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let (status, body) = service
        .send(validate_request(
            Some(issued.expose_token()),
            " ".repeat(1024 * 1024 + 1),
        ))
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["code"], "body_too_large");
    assert_eq!(
        service
            .send(validate_request(
                Some(issued.expose_token()),
                "{\"secret\":\"sensitive\"".into()
            ))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let request = Request::builder()
        .method("GET")
        .uri("/v1alpha1/definitions/validate")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        service.send(request).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let request = Request::builder()
        .uri("/missing?private=must-not-reflect")
        .header("x-request-id", "attacker-id")
        .body(Body::empty())
        .unwrap();
    let (status, body) = service.send(request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(body["request_id"], "attacker-id");
    assert!(!body.to_string().contains("must-not-reflect"));
}

#[tokio::test]
async fn readiness_checks_schema_and_database_while_health_and_contract_remain_available() {
    let service = Service::new().await;
    let get = |path| Request::builder().uri(path).body(Body::empty()).unwrap();
    assert_eq!(service.send(get("/ready")).await.0, StatusCode::OK);
    let (status, spec) = service.send(get("/v1alpha1/openapi.json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["openapi"], "3.1.0");
    assert!(spec["paths"]["/v1alpha1/definitions/validate"]["post"]["security"].is_array());
    let (_, capabilities) = service.send(get("/v1alpha1/capabilities")).await;
    assert_eq!(capabilities["capabilities"]["auth.oidc"], "unsupported");
    assert_eq!(capabilities["capabilities"]["computer"], "unsupported");
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=2")
        .execute(&service.database.pool)
        .await
        .unwrap();
    assert_eq!(
        service.send(get("/ready")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    service.database.pool.close().await;
    assert_eq!(service.send(get("/health")).await.0, StatusCode::OK);
    let (status, body) = service.send(get("/ready")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!body.to_string().contains("postgres"));
}
