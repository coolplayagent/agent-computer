use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_store::{auth::ServiceScope, runtime::writers::*};
use axum::{
    Extension, Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
#[derive(Clone, Copy)]
enum Operation {
    Acquire,
    Get,
    Renew,
    Release,
}
async fn handle(
    state: ServiceState,
    context: RequestContext,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    op: Operation,
) -> Response {
    let scope = if matches!(op, Operation::Acquire | Operation::Renew) {
        ServiceScope::RuntimeModify
    } else {
        ServiceScope::RuntimeConnect
    };
    let token = match requests::authorize(&state, &context, request.headers(), scope).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let id = match path {
        Ok(Path(id)) if ComputerId::new(&id).is_ok() => id,
        _ => {
            return context.error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid resource path.",
                false,
            );
        }
    };
    if matches!(op, Operation::Get) {
        return match state.store.candidate_writer(&token, &id).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => context.store_error(e),
        };
    }
    let key = match crate::plans::key(&context, request.headers()) {
        Ok(k) => k,
        Err(e) => return *e,
    };
    let body = match requests::body(&context, request).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    let bad = || {
        context.error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid writer lease request.",
            false,
        )
    };
    let result = match op {
        Operation::Acquire => {
            let Ok(input) = serde_json::from_slice::<AcquireWriterLease>(&body) else {
                return bad();
            };
            state
                .store
                .acquire_candidate_writer(&token, &key, &id, &input)
                .await
        }
        Operation::Renew => {
            let Ok(input) = serde_json::from_slice::<RenewWriterLease>(&body) else {
                return bad();
            };
            state
                .store
                .renew_candidate_writer(&token, &key, &id, &input)
                .await
        }
        Operation::Release => {
            let Ok(input) = serde_json::from_slice::<WriterLeaseCommand>(&body) else {
                return bad();
            };
            state
                .store
                .release_candidate_writer(&token, &key, &id, &input)
                .await
        }
        Operation::Get => unreachable!(),
    };
    match result {
        Ok(v) => {
            let status = match op {
                Operation::Acquire => StatusCode::CREATED,
                Operation::Release if v.state == WriterLeaseState::Draining => StatusCode::ACCEPTED,
                _ => StatusCode::OK,
            };
            (status, Json(v)).into_response()
        }
        Err(e) => context.store_error(e),
    }
}
pub(crate) async fn acquire(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Acquire).await
}
pub(crate) async fn get(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Get).await
}
pub(crate) async fn renew(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Renew).await
}
pub(crate) async fn release(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Release).await
}
