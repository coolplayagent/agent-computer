use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_store::{auth::ServiceScope, runtime::files::ReadFileRequest};
use agent_computer_worker::files::{FileGateway, FileSlot, SaveRequest};
use axum::{
    Extension, Json,
    extract::{Path, Query, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};

fn bad(context: &RequestContext) -> Response {
    context.error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid file request.",
        false,
    )
}
async fn admit(
    state: &ServiceState,
    context: &RequestContext,
    headers: &axum::http::HeaderMap,
    path: Result<Path<String>, PathRejection>,
    scope: ServiceScope,
) -> Result<(String, String, FileGateway, FileSlot), Response> {
    let token = requests::authorize(state, context, headers, scope).await?;
    let id = match path {
        Ok(Path(id)) if ComputerId::new(&id).is_ok() => id,
        _ => return Err(bad(context)),
    };
    let gateway = state.files.clone().ok_or_else(|| {
        context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "files_unavailable",
            "The file gateway is not configured.",
            false,
        )
    })?;
    let slot = gateway.admit().map_err(|_| {
        context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "file_io_busy",
            "File IO capacity is unavailable.",
            true,
        )
    })?;
    Ok((token, id, gateway, slot))
}
pub(crate) async fn read(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let (token, id, gateway, slot) = match admit(
        &state,
        &context,
        request.headers(),
        path,
        ServiceScope::RuntimeRead,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let Ok(Query(input)) = Query::<ReadFileRequest>::try_from_uri(request.uri()) else {
        return bad(&context);
    };
    let result = slot
        .run(async move { gateway.read(&state.store, &token, &id, &input).await })
        .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => context.store_error(e),
    }
}
pub(crate) async fn save(
    State(state): State<ServiceState>,
    Extension(context): Extension<RequestContext>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let (token, id, gateway, slot) = match admit(
        &state,
        &context,
        request.headers(),
        path,
        ServiceScope::RuntimeModify,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let bytes = match requests::bounded_body(&context, request, 5 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    // Parsing a bounded binary byte array is CPU work; keep it off async workers.
    let input =
        match tokio::task::spawn_blocking(move || serde_json::from_slice::<SaveRequest>(&bytes))
            .await
        {
            Ok(Ok(v)) => v,
            _ => return bad(&context),
        };
    let result = slot
        .run(async move { gateway.save(&state.store, &token, &id, input).await })
        .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => context.store_error(e),
    }
}
