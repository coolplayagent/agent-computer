use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{
    Store,
    auth::{IssueCredential, IssuedCredential, PrincipalKind, ServiceScope},
};
use agent_computer_test_support::Postgres;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;

pub struct Service {
    pub database: Postgres,
    pub store: Store,
    pub router: Router,
}

impl Service {
    pub async fn new() -> Self {
        let database = Postgres::new().await;
        let store = Store::new(database.pool.clone());
        store.migrate().await.unwrap();
        Self {
            router: agent_computer_server::router(store.clone()),
            store,
            database,
        }
    }
    pub async fn issue(&self, principal: &str, scopes: &[ServiceScope]) -> IssuedCredential {
        self.store
            .issue_credential(IssueCredential {
                organization: &OrganizationId::new("acme").unwrap(),
                principal: &PrincipalId::new(principal).unwrap(),
                kind: PrincipalKind::Agent,
                scopes,
                lifetime: Duration::from_secs(3600),
            })
            .await
            .unwrap()
    }
    pub async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        if status.is_client_error() || status.is_server_error() {
            assert_eq!(value["request_id"], request_id);
            assert!(
                value["code"].is_string()
                    && value["message"].is_string()
                    && value["retryable"].is_boolean()
                    && value["details"].is_object()
            );
        }
        (status, value)
    }
}

pub fn document() -> Value {
    json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"research"},"spec":{"agents":[{"name":"external","mode":"external","adapter":"tools-api"}]}})
}
pub fn validate_request(token: Option<&str>, body: String) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1alpha1/definitions/validate")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(Body::from(body)).unwrap()
}
