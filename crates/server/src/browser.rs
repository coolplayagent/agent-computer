//! Authenticated GUI requests. No driver connection or control credential is
//! exposed to callers; the original Browser worker consumes the durable queue.
use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_store::{
    auth::ServiceScope,
    runtime::apps::browser::control::{AcquireControl, ReleaseControl, SubmitBrowserCommand},
};
use axum::{
    Extension, Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
#[derive(Clone, Copy)]
enum Op {
    Acquire,
    Release,
    Control,
    Submit,
    Get,
}
async fn handle(
    state: ServiceState,
    context: RequestContext,
    path: Result<Path<String>, PathRejection>,
    request: Request,
    op: Op,
) -> Response {
    let scope = if matches!(op, Op::Acquire | Op::Release) {
        ServiceScope::RuntimeControl
    } else {
        ServiceScope::RuntimeRead
    };
    let token = match requests::authorize(&state, &context, request.headers(), scope).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let id = match path {
        Ok(Path(v)) if ComputerId::new(&v).is_ok() => v,
        _ => {
            return context.error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid resource path.",
                false,
            );
        }
    };
    let result = match op {
        Op::Control => state
            .store
            .browser_control(&token, &id)
            .await
            .and_then(json),
        Op::Get => state
            .store
            .browser_command(&token, &id)
            .await
            .and_then(json),
        _ => {
            let key = if matches!(op, Op::Acquire | Op::Submit) {
                match crate::plans::key(&context, request.headers()) {
                    Ok(v) => Some(v),
                    Err(e) => return *e,
                }
            } else {
                None
            };
            let body = match requests::bounded_body(&context, request, 65_536).await {
                Ok(v) => v,
                Err(e) => return e,
            };
            match op {
                Op::Acquire => match serde_json::from_slice::<AcquireControl>(&body) {
                    Ok(v) => state
                        .store
                        .acquire_browser_control(
                            &token,
                            key.as_ref().expect("acquire key"),
                            &id,
                            &v,
                        )
                        .await
                        .and_then(json),
                    Err(_) => return invalid(&context),
                },
                Op::Release => match serde_json::from_slice::<ReleaseControl>(&body) {
                    Ok(v) => state
                        .store
                        .release_browser_control(&token, &id, &v)
                        .await
                        .and_then(json),
                    Err(_) => return invalid(&context),
                },
                Op::Submit => match serde_json::from_slice::<SubmitBrowserCommand>(&body) {
                    Ok(v) => state
                        .store
                        .submit_browser_command(&token, key.as_ref().expect("submit key"), &id, &v)
                        .await
                        .and_then(json),
                    Err(_) => return invalid(&context),
                },
                _ => unreachable!(),
            }
        }
    };
    match result {
        Ok(v) => (
            if matches!(op, Op::Submit | Op::Release) {
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
fn json(v: impl serde::Serialize) -> agent_computer_store::Result<serde_json::Value> {
    serde_json::to_value(v).map_err(|_| agent_computer_store::Error::InvalidStoredData)
}
fn invalid(c: &RequestContext) -> Response {
    c.error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Supply the Browser operation fields without identity or authority overrides.",
        false,
    )
}
macro_rules! endpoint {
    ($name:ident,$op:ident) => {
        pub(crate) async fn $name(
            State(s): State<ServiceState>,
            Extension(c): Extension<RequestContext>,
            p: Result<Path<String>, PathRejection>,
            r: Request,
        ) -> Response {
            handle(s, c, p, r, Op::$op).await
        }
    };
}
endpoint!(acquire, Acquire);
endpoint!(release, Release);
endpoint!(control, Control);
endpoint!(submit, Submit);
endpoint!(get, Get);
