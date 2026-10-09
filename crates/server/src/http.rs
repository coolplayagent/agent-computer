use crate::error::RequestContext;
use agent_computer_core::{API_VERSION, VERSION};
use agent_computer_definitions::{Format, MAX_DOCUMENT_BYTES, validate_bytes};
use agent_computer_store::{Store, auth::ServiceScope};
use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use std::{error::Error as _, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct ServiceState {
    store: Store,
    requests: Arc<Semaphore>,
    validators: Arc<Semaphore>,
}

pub fn router(store: Store) -> Router {
    let state = ServiceState {
        store,
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
        .route("/v1alpha1/openapi.json", get(openapi))
        .route("/v1alpha1/definitions/validate", post(validate))
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

async fn capabilities() -> Json<serde_json::Value> {
    Json(
        json!({"api_version":API_VERSION,"stage":"development","capabilities":{
            "definitions.validate":"static", "auth.service_credentials":"supported", "auth.oidc":"unsupported",
            "definitions.plan":"unsupported", "definitions.apply":"unsupported", "computer":"unsupported",
            "browser":"unsupported", "execution":"unsupported", "artifacts":"unsupported",
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

fn bearer(headers: &HeaderMap) -> Option<&str> {
    if headers.get_all("authorization").iter().count() != 1 {
        return None;
    }
    let (scheme, token) = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    Some(token)
}

async fn validate(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    request: Request,
) -> Response {
    let Some(token) = bearer(request.headers()).map(str::to_owned) else {
        return context.store_error(agent_computer_store::Error::Unauthenticated);
    };
    if let Err(error) = state
        .store
        .authorize_service(&token, ServiceScope::DefinitionsValidate)
        .await
    {
        return context.store_error(error);
    }
    // This service-credential endpoint has no cookie or browser login mode. The
    // OIDC/CSRF/origin-bound human flow will be a separate authenticated adapter.
    if request.headers().contains_key("origin") {
        return context.error(
            StatusCode::FORBIDDEN,
            "browser_auth_unavailable",
            "Browser authentication is not available on this endpoint.",
            false,
        );
    }
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if !content_type.eq_ignore_ascii_case("application/json")
        || request.headers().get_all("content-type").iter().count() != 1
        || request.headers().contains_key("content-encoding")
    {
        return context.error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Use an uncompressed application/json body.",
            false,
        );
    }
    let body = match to_bytes(request.into_body(), MAX_DOCUMENT_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            let oversized = error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>());
            return if oversized {
                context.error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    "The body exceeds 1 MiB.",
                    false,
                )
            } else {
                context.error(
                    StatusCode::BAD_REQUEST,
                    "invalid_body",
                    "Unable to read the request body.",
                    false,
                )
            };
        }
    };
    let Ok(permit) = state.validators.try_acquire_owned() else {
        return context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "Too many validations are in progress.",
            true,
        );
    };
    let checked = tokio::task::spawn_blocking(move || {
        let _permit = permit; // Held until parsing finishes, even if the request times out.
        validate_bytes(&body, Format::Json)
    })
    .await;
    // Recheck at response construction; do not cache revocation/expiry decisions.
    if let Err(error) = state
        .store
        .authorize_service(&token, ServiceScope::DefinitionsValidate)
        .await
    {
        return context.store_error(error);
    }
    match checked {
        Ok(Ok(definition)) => Json(definition.report()).into_response(),
        Ok(Err(report)) => context.details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_definition",
            "The declaration is invalid.",
            false,
            json!({"validation":report}),
        ),
        Err(_) => context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "The service is unavailable.",
            true,
        ),
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
