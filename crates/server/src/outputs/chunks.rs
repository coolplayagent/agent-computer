//! Pollable verified prefixes remain readable after transport interruption.
use super::*;
use axum::Json;

fn cursor(query: Option<&str>) -> Option<(u32, u32)> {
    let Some(query) = query else {
        return Some((0, 16));
    };
    if query.is_empty() || query.len() > 128 {
        return None;
    }
    let (mut after, mut limit) = (None, None);
    for field in query.split('&') {
        let (key, value) = field.split_once('=')?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value: u32 = value.parse().ok()?;
        match key {
            "after_sequence" if after.is_none() && value <= 8192 => after = Some(value),
            "limit" if limit.is_none() && (1..=32).contains(&value) => limit = Some(value),
            _ => return None,
        }
    }
    Some((after.unwrap_or(0), limit.unwrap_or(16)))
}

pub(crate) async fn list(
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
        Ok(token) => token,
        Err(error) => return error,
    };
    let bad = || {
        context.error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Use after_sequence from 0 to 8192 and limit from 1 to 32; ranges are unsupported.",
            false,
        )
    };
    let id = match path {
        Ok(Path(id)) if ComputerId::new(&id).is_ok() => id,
        _ => return bad(),
    };
    let Some((after, limit)) = cursor(request.uri().query()) else {
        return bad();
    };
    if request.headers().contains_key("range") {
        return bad();
    }
    match state
        .store
        .candidate_execution_chunks(&token, &id, after, limit)
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(error) => context.store_error(error),
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
            "Select a chunk sequence from 1 to 8192; ranges and query parameters are unsupported.",
            false,
        )
    };
    let (id, sequence) = match path {
        Ok(Path((id, sequence)))
            if ComputerId::new(&id).is_ok()
                && !sequence.is_empty()
                && sequence.bytes().all(|b| b.is_ascii_digit()) =>
        {
            let Ok(sequence) = sequence.parse::<u32>() else {
                return bad();
            };
            if !(1..=8192).contains(&sequence) {
                return bad();
            }
            (id, sequence)
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
    match state
        .store
        .download_candidate_execution_chunk(&token, &id, sequence, &gateway.client)
        .await
    {
        Ok(download) => {
            let m = download.chunk;
            let mut headers = HeaderMap::new();
            for (name, value) in [
                ("content-type", "application/octet-stream".to_owned()),
                ("content-length", download.bytes.len().to_string()),
                (
                    "content-disposition",
                    format!("attachment; filename=\"chunk-{sequence}.bin\""),
                ),
                ("accept-ranges", "none".to_owned()),
                ("x-output-sequence", sequence.to_string()),
                (
                    "x-output-stream",
                    match m.stream {
                        OutputStream::Stdout => "stdout",
                        OutputStream::Stderr => "stderr",
                    }
                    .to_owned(),
                ),
                ("x-output-offset", m.offset.to_string()),
                ("x-output-sha256", m.sha256),
                ("x-output-chunk-digest", m.chunk_digest),
                ("x-output-observed-bytes", m.observed_bytes.to_string()),
                ("x-output-truncated", m.truncated.to_string()),
                ("x-output-eof", m.eof.to_string()),
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
