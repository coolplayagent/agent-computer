use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_store::{auth::ServiceScope, runtime::RuntimeKind};
use axum::{
    Extension, Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};

pub(crate) async fn access(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<(String, String)>, PathRejection>,
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
        Ok(token) => token,
        Err(response) => return response,
    };
    let Ok(Path((kind, id))) = path else {
        return context.error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid resource path.",
            false,
        );
    };
    let Ok(kind) = kind.parse::<RuntimeKind>() else {
        return context.error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid resource kind.",
            false,
        );
    };
    match state.store.runtime_access(&token, kind, &id).await {
        Ok(access) => Json(access).into_response(),
        Err(error) => context.store_error(error),
    }
}
