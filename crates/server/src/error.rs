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
