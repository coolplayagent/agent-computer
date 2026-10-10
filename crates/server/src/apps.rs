use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_store::{
    auth::ServiceScope,
    runtime::apps::{AdmitApp, CancelApp},
};
use axum::{
    Extension, Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Clone, Copy)]
enum Operation {
    Admit,
    Get,
    Cancel,
}
async fn handle(
    state: ServiceState,
    context: RequestContext,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    operation: Operation,
) -> Response {
    let scope = match operation {
        Operation::Admit => ServiceScope::RuntimeActivate,
        Operation::Get => ServiceScope::RuntimeRead,
        Operation::Cancel => ServiceScope::RuntimeManage,
    };
    let token = match requests::authorize(&state, &context, request.headers(), scope).await {
        Ok(v) => v,
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
    let result = match operation {
        Operation::Get => state.store.app_instance(&token, &id).await,
        Operation::Admit | Operation::Cancel => {
            let key = match crate::plans::key(&context, request.headers()) {
                Ok(v) => v,
                Err(e) => return *e,
            };
            let body = match requests::bounded_body(&context, request, 4096).await {
                Ok(v) => v,
                Err(e) => return e,
            };
            match operation {
                Operation::Admit => {
                    let Ok(input) = serde_json::from_slice::<AdmitApp>(&body) else {
                        return context.error(StatusCode::BAD_REQUEST,"invalid_request","Supply connection_session_id, computer_generation, app_id, expected_app_revision and runtime_seconds.",false);
                    };
                    state.store.admit_app(&token, &key, &id, &input).await
                }
                Operation::Cancel => {
                    let Ok(input) = serde_json::from_slice::<CancelApp>(&body) else {
                        return context.error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request",
                            "Supply app_generation and expected_revision.",
                            false,
                        );
                    };
                    state.store.cancel_app(&token, &key, &id, &input).await
                }
                Operation::Get => unreachable!(),
            }
        }
    };
    match result {
        Ok(v) => (
            if matches!(operation, Operation::Admit) {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            Json(v),
        )
            .into_response(),
        Err(e) => context.store_error(e),
    }
}
pub(crate) async fn admit(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Admit).await
}
pub(crate) async fn get(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Get).await
}
pub(crate) async fn cancel(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Cancel).await
}
