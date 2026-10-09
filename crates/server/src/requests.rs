use crate::{error::RequestContext, http::ServiceState};
use agent_computer_definitions::{Format, MAX_DOCUMENT_BYTES, ValidatedDefinition, validate_bytes};
use agent_computer_store::auth::ServiceScope;
use axum::{
    body::to_bytes,
    extract::Request,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::error::Error as _;

pub(crate) async fn authorize(
    state: &ServiceState,
    context: &RequestContext,
    headers: &HeaderMap,
    scope: ServiceScope,
) -> Result<String, Response> {
    let token = if headers.get_all("authorization").iter().count() == 1 {
        headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, token)| token)
    } else {
        None
    };
    let token =
        token.ok_or_else(|| context.store_error(agent_computer_store::Error::Unauthenticated))?;
    state
        .store
        .authorize_service(token, scope)
        .await
        .map_err(|e| context.store_error(e))?;
    if headers.contains_key("origin") {
        return Err(context.error(
            StatusCode::FORBIDDEN,
            "browser_auth_unavailable",
            "Browser authentication is not available on this endpoint.",
            false,
        ));
    }
    Ok(token.into())
}

pub(crate) async fn body(context: &RequestContext, request: Request) -> Result<Vec<u8>, Response> {
    let media = request
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if !media.eq_ignore_ascii_case("application/json")
        || request.headers().get_all("content-type").iter().count() != 1
        || request.headers().contains_key("content-encoding")
    {
        return Err(context.error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Use an uncompressed application/json body.",
            false,
        ));
    }
    to_bytes(request.into_body(), MAX_DOCUMENT_BYTES)
        .await
        .map(|v| v.to_vec())
        .map_err(|error| {
            if error
                .source()
                .is_some_and(|s| s.is::<http_body_util::LengthLimitError>())
            {
                context.error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    "The body exceeds 1 MiB.",
                    false,
                )
            } else {
                context.error(
                    StatusCode::BAD_REQUEST,
                    "invalid_body",
                    "Unable to read the request body.",
                    false,
                )
            }
        })
}

pub(crate) async fn definition(
    state: &ServiceState,
    context: &RequestContext,
    request: Request,
) -> Result<ValidatedDefinition, Response> {
    let body = body(context, request).await?;
    let permit = state.validators.clone().try_acquire_owned().map_err(|_| {
        context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "Too many validations are in progress.",
            true,
        )
    })?;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        validate_bytes(&body, Format::Json)
    })
    .await
    {
        Ok(Ok(definition)) => Ok(definition),
        Ok(Err(report)) => Err(context.details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_definition",
            "The declaration is invalid.",
            false,
            serde_json::json!({"validation":report}),
        )),
        Err(_) => Err(context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "The service is unavailable.",
            true,
        )),
    }
}
