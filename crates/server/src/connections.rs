use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_store::{
    auth::ServiceScope,
    runtime::connections::{ConnectRequest, ConnectionHeartbeat},
};
use axum::{
    Extension, Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Clone, Copy)]
enum Operation {
    Connect,
    Get,
    Heartbeat,
    Close,
}

async fn handle(
    state: ServiceState,
    context: RequestContext,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    operation: Operation,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::RuntimeConnect,
    )
    .await
    {
        Ok(token) => token,
        Err(response) => return response,
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
    let (status, result) = match operation {
        Operation::Get => (
            StatusCode::OK,
            state.store.connection_session(&token, &id).await,
        ),
        Operation::Close => (
            StatusCode::OK,
            state.store.close_connection_session(&token, &id).await,
        ),
        Operation::Connect | Operation::Heartbeat => {
            let key = match crate::plans::key(&context, request.headers()) {
                Ok(v) => v,
                Err(e) => return *e,
            };
            let body = match requests::body(&context, request).await {
                Ok(v) => v,
                Err(e) => return e,
            };
            if matches!(operation, Operation::Connect) {
                let Ok(input) = serde_json::from_slice::<ConnectRequest>(&body) else {
                    return context.error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "Supply requested_capabilities and optional lifetime_seconds.",
                        false,
                    );
                };
                (
                    StatusCode::CREATED,
                    state
                        .store
                        .create_connection_session(&token, &key, &id, &input)
                        .await,
                )
            } else {
                let Ok(input) = serde_json::from_slice::<ConnectionHeartbeat>(&body) else {
                    return context.error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "Supply expected_revision, activity and visibility.",
                        false,
                    );
                };
                (
                    StatusCode::OK,
                    state
                        .store
                        .heartbeat_connection_session(&token, &key, &id, &input)
                        .await,
                )
            }
        }
    };
    match result {
        Ok(v) => (status, Json(v)).into_response(),
        Err(e) => context.store_error(e),
    }
}

pub(crate) async fn connect(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Connect).await
}
pub(crate) async fn get(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Get).await
}
pub(crate) async fn heartbeat(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Heartbeat).await
}
pub(crate) async fn close(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Close).await
}
