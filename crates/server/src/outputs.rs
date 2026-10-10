//! Bounded downloads keep control-plane capacity available. No output URL is exposed.
use crate::{error::RequestContext, http::ServiceState, requests};
use agent_computer_core::identity::ComputerId;
use agent_computer_objects::{Client, Configuration};
use agent_computer_store::{auth::ServiceScope, runtime::writers::OutputStream};
use axum::{
    Extension,
    body::Body,
    extract::{Path, Request, State, rejection::PathRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::{BodyExt, Full};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const OUTPUT_DOWNLOAD_SLOTS: usize = 4;

#[derive(Clone)]
pub struct OutputGateway {
    client: Arc<Client>,
    slots: Arc<Semaphore>,
}
impl OutputGateway {
    pub fn new(config: &Configuration) -> agent_computer_objects::Result<Self> {
        Ok(Self {
            client: Arc::new(Client::new(config)?),
            slots: Arc::new(Semaphore::new(OUTPUT_DOWNLOAD_SLOTS)),
        })
    }
}

pub(crate) async fn download(
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
        Err(error) => return error,
    };
    let bad = || {
        context.error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Select stdout or stderr; ranges and query parameters are unsupported.",
            false,
        )
    };
    let (id, stream) = match path {
        Ok(Path((id, stream))) if ComputerId::new(&id).is_ok() => {
            let stream = match stream.as_str() {
                "stdout" => OutputStream::Stdout,
                "stderr" => OutputStream::Stderr,
                _ => return bad(),
            };
            (id, stream)
        }
        _ => return bad(),
    };
    if request.uri().query().is_some() || request.headers().contains_key("range") {
        return bad();
    }
    let Some(gateway) = state.outputs else {
        return context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "outputs_unavailable",
            "The output gateway is not configured.",
            false,
        );
    };
    let Ok(slot) = gateway.slots.clone().try_acquire_owned() else {
        return context.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "output_io_busy",
            "Output download capacity is unavailable.",
            true,
        );
    };
    // Unlike a blocking file operation, this bounded asynchronous GET is
    // cancelled with the request. No detached task or uncapped queue remains.
    match state
        .store
        .download_candidate_execution_output(&token, &id, stream, &gateway.client)
        .await
    {
        Ok(download) => {
            let metadata = match stream {
                OutputStream::Stdout => &download.output.stdout,
                OutputStream::Stderr => &download.output.stderr,
            };
            let mut headers = HeaderMap::new();
            for (name, value) in [
                ("content-type", "application/octet-stream".to_owned()),
                ("content-length", download.bytes.len().to_string()),
                (
                    "content-disposition",
                    match stream {
                        OutputStream::Stdout => "attachment; filename=\"stdout.bin\"",
                        OutputStream::Stderr => "attachment; filename=\"stderr.bin\"",
                    }
                    .to_owned(),
                ),
                ("accept-ranges", "none".to_owned()),
                ("x-output-sha256", metadata.sha256.clone()),
                ("x-output-manifest-digest", download.output.manifest_digest),
                (
                    "x-output-observed-bytes",
                    metadata.observed_bytes.to_string(),
                ),
                ("x-output-truncated", metadata.truncated.to_string()),
                ("x-output-eof", metadata.eof.to_string()),
            ] {
                let Ok(value) = value.parse() else {
                    return context.store_error(agent_computer_store::Error::InvalidStoredData);
                };
                headers.insert(name, value);
            }
            (headers, verified_body(download.bytes, slot)).into_response()
        }
        Err(error) => context.store_error(error),
    }
}

// Keep the slot with the verified buffer until the HTTP transport consumes or
// drops its body. No detached task survives a cancelled object fetch.
fn verified_body(bytes: Vec<u8>, slot: OwnedSemaphorePermit) -> Body {
    Body::new(
        Full::new(axum::body::Bytes::from(bytes)).map_frame(move |frame| {
            let _keep_slot = &slot;
            frame
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unconsumed_output_bodies_hold_capacity_and_drop_releases_it() {
        let slots = Arc::new(Semaphore::new(OUTPUT_DOWNLOAD_SLOTS));
        let mut bodies = (0..OUTPUT_DOWNLOAD_SLOTS)
            .map(|_| verified_body(vec![0, 255], slots.clone().try_acquire_owned().unwrap()))
            .collect::<Vec<_>>();
        assert!(slots.clone().try_acquire_owned().is_err());
        drop(bodies.pop());
        assert_eq!(slots.available_permits(), 1);
        let body = bodies.pop().unwrap();
        assert_eq!(
            axum::body::to_bytes(body, 2).await.unwrap().as_ref(),
            &[0, 255]
        );
        assert_eq!(slots.available_permits(), 2);
        drop(bodies);
        assert_eq!(slots.available_permits(), OUTPUT_DOWNLOAD_SLOTS);
    }
}
