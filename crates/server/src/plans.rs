use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::IdempotencyKey;
use agent_computer_store::auth::ServiceScope;
use axum::{
    Extension, Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

pub(crate) fn key(
    context: &RequestContext,
    headers: &HeaderMap,
) -> Result<IdempotencyKey, Box<Response>> {
    let key = if headers.get_all("idempotency-key").iter().count() == 1 {
        headers
            .get("idempotency-key")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| IdempotencyKey::new(v).ok())
    } else {
        None
    };
    key.ok_or_else(|| {
        Box::new(context.error(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Supply one valid Idempotency-Key header.",
            false,
        ))
    })
}

pub(crate) async fn create(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    request: Request,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::DefinitionsManage,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let key = match key(&context, request.headers()) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let definition = match requests::definition(&state, &context, request).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    match state
        .store
        .create_definition_plan(&token, &key, &definition)
        .await
    {
        Ok(plan) => (StatusCode::CREATED, Json(plan)).into_response(),
        Err(e) => context.store_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply {
    plan_digest: String,
}

pub(crate) async fn apply(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::DefinitionsManage,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let id = match resource_id(&context, path, "plan") {
        Ok(id) => id,
        Err(error) => return *error,
    };
    let key = match key(&context, request.headers()) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let body = match requests::body(&context, request).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let request: Apply = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return context.error(
                StatusCode::BAD_REQUEST,
                "invalid_apply",
                "Supply only plan_digest in the JSON body.",
                false,
            );
        }
    };
    match state
        .store
        .apply_definition_plan(&token, &key, &id, &request.plan_digest)
        .await
    {
        Ok(operation) => (StatusCode::ACCEPTED, Json(operation)).into_response(),
        Err(e) => context.store_error(e),
    }
}

pub(crate) async fn get_plan(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        &headers,
        ServiceScope::DefinitionsManage,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let id = match resource_id(&context, path, "plan") {
        Ok(id) => id,
        Err(error) => return *error,
    };
    match state.store.definition_plan(&token, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => context.store_error(e),
    }
}

pub(crate) async fn get_operation(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        &headers,
        ServiceScope::DefinitionsManage,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let id = match resource_id(&context, path, "op") {
        Ok(id) => id,
        Err(error) => return *error,
    };
    match state.store.definition_operation(&token, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => context.store_error(e),
    }
}

fn resource_id(
    context: &RequestContext,
    path: Result<Path<String>, PathRejection>,
    prefix: &str,
) -> Result<String, Box<Response>> {
    let invalid = || {
        Box::new(context.error(
            StatusCode::BAD_REQUEST,
            "invalid_id",
            "Invalid resource identifier.",
            false,
        ))
    };
    let Path(id) = path.map_err(|_| invalid())?;
    let suffix = id.strip_prefix(&format!("{prefix}_")).ok_or_else(invalid)?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    Ok(id)
}
