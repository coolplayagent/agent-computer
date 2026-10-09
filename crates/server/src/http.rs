use crate::error::RequestContext;
use agent_computer_core::{API_VERSION, VERSION};
use agent_computer_store::{Store, auth::ServiceScope};
use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub(crate) struct ServiceState {
    pub(crate) store: Store,
    pub(crate) files: Option<agent_computer_worker::files::FileGateway>,
    requests: Arc<Semaphore>,
    pub(crate) validators: Arc<Semaphore>,
}

pub fn router(store: Store) -> Router {
    configured_router(store, None)
}
pub fn router_with_files(store: Store, files: agent_computer_worker::files::FileGateway) -> Router {
    configured_router(store, Some(files))
}
fn configured_router(
    store: Store,
    files: Option<agent_computer_worker::files::FileGateway>,
) -> Router {
    let state = ServiceState {
        store,
        files,
        requests: Arc::new(Semaphore::new(64)),
        validators: Arc::new(Semaphore::new(8)),
    };
    Router::new()
        .route("/health", get(|| async { Json(json!({"status":"alive"})) }))
        .route("/ready", get(ready))
        .route(
            "/v1alpha1/version",
            get(|| async { Json(json!({"version":VERSION,"api_version":API_VERSION})) }),
        )
        .route("/v1alpha1/capabilities", get(capabilities))
        .route(
            "/v1alpha1/computers/{id}/connection-sessions",
            post(crate::connections::connect),
        )
        .route(
            "/v1alpha1/connection-sessions/{id}",
            get(crate::connections::get).delete(crate::connections::close),
        )
        .route(
            "/v1alpha1/connection-sessions/{id}/heartbeat",
            post(crate::connections::heartbeat),
        )
        .route("/v1alpha1/openapi.json", get(openapi))
        .route("/v1alpha1/workspaces/{id}/files", get(crate::files::read))
        .route("/v1alpha1/leases/{id}/file", post(crate::files::save))
        .route(
            "/v1alpha1/computers/{id}/leases",
            post(crate::writers::acquire),
        )
        .route("/v1alpha1/leases/{id}", get(crate::writers::get))
        .route("/v1alpha1/leases/{id}/renew", post(crate::writers::renew))
        .route(
            "/v1alpha1/leases/{id}/release",
            post(crate::writers::release),
        )
        .route("/v1alpha1/definitions/validate", post(validate))
        .route("/v1alpha1/plans", post(crate::plans::create))
        .route("/v1alpha1/plans/{id}", get(crate::plans::get_plan))
        .route("/v1alpha1/plans/{id}/apply", post(crate::plans::apply))
        .route(
            "/v1alpha1/operations/{id}",
            get(crate::plans::get_operation),
        )
        .route(
            "/v1alpha1/runtime-access/{kind}/{id}",
            get(crate::runtime::access),
        )
        .route(
            "/v1alpha1/computers/{id}/runtime",
            get(crate::computers::runtime),
        )
        .route(
            "/v1alpha1/computers/{id}/start",
            post(crate::computers::start),
        )
        .route(
            "/v1alpha1/computers/{id}/start/cancel",
            post(crate::computers::cancel),
        )
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), envelope))
        .with_state(state)
}

async fn envelope(State(state): State<ServiceState>, mut request: Request, next: Next) -> Response {
    let mut random = [0u8; 16];
    let context = if getrandom::fill(&mut random).is_ok() {
        RequestContext(random.iter().map(|b| format!("{b:02x}")).collect())
    } else {
        return RequestContext("unavailable".into()).error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "The service is unavailable.",
            true,
        );
    };
    request.extensions_mut().insert(context.clone());
    let mut response = match state.requests.try_acquire_owned() {
        Err(_) => context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "Too many requests are in progress.",
            true,
        ),
        Ok(_permit) => match tokio::time::timeout(Duration::from_secs(10), next.run(request)).await
        {
            Ok(response) => response,
            Err(_) => context.error(
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                "The request exceeded its time limit.",
                true,
            ),
        },
    };
    response
        .headers_mut()
        .insert("x-request-id", context.0.parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response
}

async fn ready(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
) -> Response {
    match state.store.ready().await {
        Ok(()) => Json(json!({"status":"ready"})).into_response(),
        Err(error) => context.store_error(error),
    }
}

async fn capabilities(State(state): State<ServiceState>) -> Json<serde_json::Value> {
    Json(
        json!({"api_version":API_VERSION,"stage":"development","capabilities":{
            "definitions.validate":"static", "auth.service_credentials":"supported", "auth.oidc":"unsupported", "auth.runtime_grants":"control-plane",
            "definitions.plan":"control-plane", "definitions.apply":"control-plane", "reconciliation.coordination":"control-plane", "reconciliation":"unsupported", "computer":"unsupported", "computer.start_admission":"control-plane",
            "connection.sessions":"control-plane", "candidate.writer_leases":"control-plane","candidate.file_save":"trusted-worker", "files.read":if state.files.is_some(){"bounded-candidate"}else{"unsupported"}, "files.save":if state.files.is_some(){"bounded-candidate"}else{"unsupported"}, "browser":"unsupported", "execution":"unsupported", "artifacts":"unsupported",
            "presentation":"unsupported", "deployment":"unsupported", "mcp":"unsupported", "evaluation":"unsupported"
        }}),
    )
}

async fn openapi() -> impl IntoResponse {
    (
        [("content-type", "application/json")],
        include_str!("../../../schemas/openapi-v1alpha1.json"),
    )
}

async fn validate(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    request: Request,
) -> Response {
    let token = match crate::requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::DefinitionsValidate,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let checked = crate::requests::definition(&state, &context, request).await;
    if let Err(error) = state
        .store
        .authorize_service(&token, ServiceScope::DefinitionsValidate)
        .await
    {
        return context.store_error(error);
    }
    match checked {
        Ok(definition) => Json(definition.report()).into_response(),
        Err(response) => response,
    }
}

async fn not_found(Extension(context): Extension<RequestContext>) -> Response {
    context.error(
        StatusCode::NOT_FOUND,
        "not_found",
        "The endpoint does not exist.",
        false,
    )
}
async fn method_not_allowed(Extension(context): Extension<RequestContext>) -> Response {
    context.error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "The method is not supported.",
        false,
    )
}
