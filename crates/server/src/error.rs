use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) struct RequestContext(pub String);

impl RequestContext {
    pub fn error(
        &self,
        status: StatusCode,
        code: &str,
        message: &str,
        retryable: bool,
    ) -> Response {
        self.details(status, code, message, retryable, json!({}))
    }

    pub fn details(
        &self,
        status: StatusCode,
        code: &str,
        message: &str,
        retryable: bool,
        details: Value,
    ) -> Response {
        (status, Json(json!({"code":code, "message":message, "retryable":retryable, "request_id":self.0, "details":details}))).into_response()
    }

    pub fn store_error(&self, error: agent_computer_store::Error) -> Response {
        use agent_computer_store::Error;
        match error {
            Error::PreconditionRequired => self.error(
                StatusCode::PRECONDITION_REQUIRED,
                "precondition_required",
                "Existing definitions require expectedRevision.",
                false,
            ),
            Error::RevisionConflict => self.error(
                StatusCode::PRECONDITION_FAILED,
                "revision_conflict",
                "A definition or reference changed; create a new plan.",
                false,
            ),
            Error::IdempotencyConflict => self.error(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "The key has different request input.",
                false,
            ),
            Error::IdempotencyGone => self.error(
                StatusCode::GONE,
                "idempotency_gone",
                "The request key is retired.",
                false,
            ),
            Error::PlanDigestMismatch => self.error(
                StatusCode::CONFLICT,
                "plan_digest_mismatch",
                "The plan digest does not match.",
                false,
            ),
            Error::PlanExpired => self.error(
                StatusCode::GONE,
                "plan_expired",
                "The plan expired; create a new plan.",
                false,
            ),
            Error::PlanNotFound => self.error(
                StatusCode::NOT_FOUND,
                "not_found",
                "The plan or operation is unavailable.",
                false,
            ),
            Error::ReferenceUnavailable => self.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "reference_unavailable",
                "A reference is unavailable or incompatible.",
                false,
            ),
            Error::PlanTooLarge => self.error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "plan_too_large",
                "The plan exceeds its size or dependency limit.",
                false,
            ),
            Error::UnsupportedChange => self.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_change",
                "This change requires a separate data migration.",
                false,
            ),
            Error::Unauthenticated => {
                let mut response = self.error(
                    StatusCode::UNAUTHORIZED,
                    "unauthenticated",
                    "A valid service credential is required.",
                    false,
                );
                response.headers_mut().insert(
                    "www-authenticate",
                    "Bearer realm=\"agent-computer\"".parse().unwrap(),
                );
                response
            }
            Error::Forbidden => self.error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "The credential does not grant this operation.",
                false,
            ),
            _ => self.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "The service is unavailable.",
                true,
            ),
        }
    }
}
