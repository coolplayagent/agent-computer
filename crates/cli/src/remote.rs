//! Explicit, single-request client for the bounded runtime HTTP APIs.
mod options;
mod output;

use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};

pub const HELP: &str = "Remote service commands:
  doctor                                Read service capabilities
  computer show|start|cancel-start|stop|checkpoint-stop <computer-id>
  connect <computer-id>                  Open a logical connection
  connection show|heartbeat <session-id>
  disconnect <session-id>                Close the logical connection
  lease acquire <computer-id>            Acquire Candidate modification lease
  lease show|renew|release <lease-id>
  exec <computer-id>                     Submit structured execution JSON
  status|cancel <execution-id>
  logs <execution-id>                    Read output publication metadata
  logs <execution-id> --stream stdout|stderr --output <new-file>

Remote options: --endpoint <origin> --token-file <file> [--json]
  Environment defaults: AGENT_COMPUTER_ENDPOINT, AGENT_COMPUTER_TOKEN_FILE
  POST commands require --request <file|-> --idempotency-key <key>.
  Request fields follow /v1alpha1/openapi.json; revisions are explicit.
  HTTPS is required except for literal loopback HTTP. No redirects or retries.
  Exit 0 means the request succeeded, including queued work; inspect its state.
  Exit 1 means an HTTP rejection; exit 2 means usage or incomplete verification.
  Downloads verify SHA-256 and never replace an existing file.";

const MAX_REQUEST: usize = 65536;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
type Result<T> = std::result::Result<T, Failure>;

struct Failure {
    exit: u8,
    report: Value,
}
impl Failure {
    fn local(code: &str, message: &str) -> Self {
        Self {
            exit: 2,
            report: json!({"code":code,"message":message,"retryable":false}),
        }
    }
    fn incomplete(mut self, mutation: bool) -> Self {
        if mutation {
            self.report["request_may_have_been_applied"] = json!(true);
            self.report["recovery"] = json!(
                "Query authoritative state or explicitly retry identical input with the original idempotency key. Do not submit under a new key."
            );
        }
        self
    }
}

pub fn run(args: &[&str]) -> u8 {
    if args.iter().any(|arg| matches!(*arg, "--help" | "-h")) {
        println!("{HELP}");
        return 0;
    }
    let compact = args.contains(&"--json");
    let result = options::parse(args).and_then(|options| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                Failure::local(
                    "client_unavailable",
                    "Unable to initialize the HTTP client.",
                )
            })?;
        runtime.block_on(request(options))
    });
    let (value, exit) = match result {
        Ok(value) => (value, 0),
        Err(error) => (error.report, error.exit),
    };
    let text = if compact {
        serde_json::to_string(&value)
    } else {
        serde_json::to_string_pretty(&value)
    }
    .expect("JSON values are serializable");
    let written = if exit == 0 {
        writeln!(std::io::stdout().lock(), "{text}")
    } else {
        writeln!(std::io::stderr().lock(), "{text}")
    };
    if written.is_err() { 2 } else { exit }
}

fn local_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_relative()
        && let Some(directory) = std::env::var_os("BUILD_WORKING_DIRECTORY")
    {
        PathBuf::from(directory).join(path)
    } else {
        path
    }
}

fn read(path: &str, limit: usize, stdin: bool) -> Result<Vec<u8>> {
    let source: Box<dyn Read> =
        if stdin && path == "-" {
            Box::new(std::io::stdin())
        } else {
            Box::new(std::fs::File::open(local_path(path)).map_err(|_| {
                Failure::local("input_unavailable", "Unable to open the input file.")
            })?)
        };
    let mut bytes = Vec::new();
    source
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::local("input_unavailable", "Unable to read the input."))?;
    if bytes.len() > limit {
        return Err(Failure::local(
            "input_too_large",
            "Input exceeds the command's byte limit.",
        ));
    }
    Ok(bytes)
}

fn endpoint(value: &str) -> Result<(Url, bool)> {
    let bad = || {
        Failure::local(
            "invalid_endpoint",
            "Use an HTTPS origin, or literal loopback HTTP, without user information, path, query or fragment.",
        )
    };
    let url = Url::parse(value).map_err(|_| bad())?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(bad());
    }
    Ok((url, loopback))
}

async fn request(options: options::Options) -> Result<Value> {
    let (mut url, loopback) = endpoint(&options.endpoint)?;
    url.set_path(&options.path);
    let token = read(&options.token_file, 4096, false)?;
    let token = std::str::from_utf8(&token)
        .ok()
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_graphic()))
        .ok_or_else(|| {
            Failure::local(
                "invalid_credential",
                "The credential file must contain one nonempty ASCII bearer token.",
            )
        })?;
    let body = options
        .input
        .as_deref()
        .map(|path| {
            let bytes = read(path, MAX_REQUEST, true)?;
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                Failure::local("invalid_request", "The request must be one JSON object.")
            })?;
            if !value.is_object() {
                return Err(Failure::local(
                    "invalid_request",
                    "The request must be one JSON object.",
                ));
            }
            Ok(bytes)
        })
        .transpose()?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15));
    // A local endpoint must not disclose its credential to an ambient proxy.
    if loopback {
        builder = builder.no_proxy();
    }
    let client = builder.build().map_err(|_| {
        Failure::local(
            "client_unavailable",
            "Unable to initialize the HTTP client.",
        )
    })?;
    let mutation = options.method != Method::GET;
    let mut request = client.request(options.method, url).bearer_auth(token);
    if let Some(key) = options.key {
        request = request.header("idempotency-key", key);
    }
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body);
    }
    let response = request.send().await.map_err(|_| {
        Failure::local(
            "transport_error",
            "No verified response was received; the client did not retry.",
        )
        .incomplete(mutation)
    })?;
    response_value(response, options.output).await.map_err(|e| {
        // A gateway/server error (or redirect) is not proof that a write never
        // committed. Preserve the server's report and the original retry key.
        let uncertain_http = e.report["http_status"]
            .as_u64()
            .is_some_and(|status| status >= 500 || status == 408 || (300..400).contains(&status));
        if e.exit == 2 || uncertain_http {
            e.incomplete(mutation)
        } else {
            e
        }
    })
}

async fn response_value(mut response: reqwest::Response, output: Option<String>) -> Result<Value> {
    let status = response.status();
    let headers = response.headers().clone();
    let limit = if status.is_success() && output.is_some() {
        output::MAX_BYTES
    } else {
        MAX_RESPONSE
    };
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(Failure::local(
            "response_too_large",
            "The service response exceeds the command's byte limit.",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        Failure::local(
            "incomplete_response",
            "The service response was incomplete; the client did not retry.",
        )
    })? {
        if chunk.len() > limit - bytes.len() {
            return Err(Failure::local(
                "response_too_large",
                "The service response exceeds the command's byte limit.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let mut report = serde_json::from_slice::<Value>(&bytes).ok()
            .filter(|v| v.is_object() && v["code"].is_string() && v["message"].is_string())
            .unwrap_or_else(|| json!({"code":"http_error","message":"The service rejected the request; redirects are not followed.","retryable":false}));
        report["http_status"] = json!(status.as_u16());
        return Err(Failure { exit: 1, report });
    }
    if let Some(path) = output {
        if status != reqwest::StatusCode::OK {
            return Err(Failure::local(
                "invalid_output",
                "Expected a complete output download.",
            ));
        }
        return output::save(&path, &headers, &bytes);
    }
    if !headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
    {
        return Err(Failure::local(
            "invalid_response",
            "Expected a JSON service response.",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Failure::local("invalid_response", "Expected a JSON service response."))
}
