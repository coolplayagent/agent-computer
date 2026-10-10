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
    Submit,
    Get,
    Cancel,
}
async fn handle(
    state: ServiceState,
    context: RequestContext,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    op: Operation,
) -> Response {
    // Exact retries/metadata/cancellation remain available with RuntimeConnect;
    // first submission additionally requires read/modify in the store transaction.
    let token = match requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::RuntimeConnect,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return e,
    };
    let bad = || {
        context.error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid execution admission request.",
            false,
        )
    };
    let id = match path {
        Ok(Path(id)) if ComputerId::new(&id).is_ok() => id,
        _ => return bad(),
    };
    let result = if matches!(op, Operation::Get) {
        state.store.candidate_execution(&token, &id).await
    } else {
        let key = match crate::plans::key(&context, request.headers()) {
            Ok(k) => k,
            Err(e) => return *e,
        };
        let bytes = match requests::bounded_body(&context, request, 65536).await {
            Ok(b) => b,
            Err(e) => return e,
        };
        match op {
            Operation::Submit => {
                let Ok(input) = serde_json::from_slice::<SubmitExecution>(&bytes) else {
                    return bad();
                };
                state
                    .store
                    .submit_candidate_execution(&token, &key, &id, &input)
                    .await
            }
            Operation::Cancel => {
                let Ok(input) = serde_json::from_slice::<CancelExecution>(&bytes) else {
                    return bad();
                };
                state
                    .store
                    .cancel_candidate_execution(&token, &key, &id, &input)
                    .await
            }
            Operation::Get => unreachable!(),
        }
    };
    match result {
        Ok(v) => (
            if matches!(op, Operation::Submit) && v.state == ExecutionState::Queued {
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
pub(crate) async fn submit(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    handle(state, context, path, request, Operation::Submit).await
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

pub(crate) async fn output(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let token = match requests::authorize(
        &state,
        &context,
        request.headers(),
        ServiceScope::RuntimeConnect,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return e,
    };
    let id = match path {
        Ok(Path(id)) if ComputerId::new(&id).is_ok() => id,
        _ => {
            return context.error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid execution ID.",
                false,
            );
        }
    };
    match state.store.candidate_execution_output(&token, &id).await {
        Ok(output) => (StatusCode::OK, Json(output)).into_response(),
        Err(e) => context.store_error(e),
    }
}
