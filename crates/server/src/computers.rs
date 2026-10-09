use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_store::{
    auth::ServiceScope,
    runtime::{CancelQueuedStart, StartRequest},
};
use axum::{
    Extension, Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};

fn id(
    context: &RequestContext,
    path: Result<Path<String>, PathRejection>,
) -> Result<String, Box<Response>> {
    if let Ok(Path(id)) = path
        && ComputerId::new(&id).is_ok()
    {
        return Ok(id);
    }
    Err(Box::new(context.error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid computer path.",
        false,
    )))
}

pub(crate) async fn runtime(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::RuntimeRead,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let id = match id(&context, path) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    match state.store.computer_runtime(&token, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => context.store_error(e),
    }
}

async fn mutate(
    state: ServiceState,
    context: RequestContext,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    cancel: bool,
) -> Response {
    let scope = if cancel {
        ServiceScope::RuntimeManage
    } else {
        ServiceScope::RuntimeActivate
    };
    let token = match requests::authorize(&state, &context, request.headers(), scope).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let id = match id(&context, path) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let key = match crate::plans::key(&context, request.headers()) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let body = match requests::body(&context, request).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    if cancel {
        let Ok(request) = serde_json::from_slice::<CancelQueuedStart>(&body) else {
            return context.error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Supply expected_revision and request_id.",
                false,
            );
        };
        match state
            .store
            .cancel_queued_computer_start(&token, &key, &id, &request)
            .await
        {
            Ok(v) => Json(v).into_response(),
            Err(e) => context.store_error(e),
        }
    } else {
        let Ok(request) = serde_json::from_slice::<StartRequest>(&body) else {
            return context.error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Supply expected_revision, expected_spec_revision and max_runtime_seconds.",
                false,
            );
        };
        match state
            .store
            .admit_computer_start(&token, &key, &id, &request)
            .await
        {
            Ok(v) => (StatusCode::ACCEPTED, Json(v)).into_response(),
            Err(e) => context.store_error(e),
        }
    }
}

pub(crate) async fn start(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    mutate(state, context, path, request, false).await
}
pub(crate) async fn cancel(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    mutate(state, context, path, request, true).await
}
