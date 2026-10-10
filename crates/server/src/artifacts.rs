use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_store::{auth::ServiceScope, runtime::artifacts::CommitArtifact};
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
    match path {
        Ok(Path(id)) if ComputerId::new(&id).is_ok() => Ok(id),
        _ => Err(Box::new(context.error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid artifact path.",
            false,
        ))),
    }
}
pub(crate) async fn commit(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::RuntimePublish,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let workspace = match id(&context, path) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let key = match crate::plans::key(&context, request.headers()) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let bytes = match requests::body(&context, request).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Ok(input) = serde_json::from_slice::<CommitArtifact>(&bytes) else {
        return context.error(StatusCode::BAD_REQUEST,"invalid_request","Supply request_id, expected_revision, base_revision, base_manifest and publish_current.",false);
    };
    match state
        .store
        .commit_workspace_artifact(&token, &key, &workspace, &input)
        .await
    {
        Ok(v) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Err(e) => context.store_error(e),
    }
}
pub(crate) async fn get(
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
    match state.store.workspace_artifact(&token, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => context.store_error(e),
    }
}

pub(crate) async fn manifest(
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
    match state.store.artifact_manifest(&token, &id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => context.store_error(e),
    }
}
