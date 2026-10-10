use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

fn executable() -> &'static str {
    option_env!("CARGO_BIN_EXE_agent-computer")
        .or(option_env!("AGENT_COMPUTER_BIN"))
        .unwrap()
}

fn invoke(endpoint: &str, args: &[&str], input: &[u8]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "private-test-token\n").unwrap();
    let mut child = Command::new(executable())
        .args(args)
        .arg("--json")
        .env("AGENT_COMPUTER_ENDPOINT", endpoint)
        .env("AGENT_COMPUTER_TOKEN_FILE", "token")
        .env("BUILD_WORKING_DIRECTORY", dir.path())
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
}

fn received(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut block = [0; 4096];
    loop {
        let n = stream.read(&mut block).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&block[..n]);
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .map(|v| v.parse::<usize>().unwrap())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return String::from_utf8(bytes).unwrap();
            }
        }
        assert!(bytes.len() < 100_000);
    }
}

fn serve(args: &[&str], input: &[u8], response: Vec<u8>) -> (Output, String, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(v) => break v,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("client failed to connect: {e}"),
            }
        };
        let request = received(&mut stream);
        let _ = stream.write_all(&response);
        drop(stream);
        (request, listener)
    });
    let result = invoke(&endpoint, args, input);
    let (request, listener) = server.join().unwrap();
    let mut unexpected = 0;
    while listener.accept().is_ok() {
        unexpected += 1;
    }
    (result, request, unexpected)
}

fn response(status: &str, media: &str, extra: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 {status}\r\nContent-Type: {media}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn result(output: &Output, exit: i32) -> Value {
    assert_eq!(output.status.code(), Some(exit), "{output:?}");
    if exit == 0 {
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    } else {
        assert!(output.stdout.is_empty());
        serde_json::from_slice(&output.stderr).unwrap()
    }
}

#[test]
fn sends_one_authenticated_request_preserving_argv_revisions_and_key() {
    let body = br#"{"command":{"argv":["/bin/echo","$(touch forbidden)","two words"]},"lease":{"generation":7,"epoch":2,"expected_revision":3}}"#;
    let (out, request, extra) = serve(
        &[
            "exec",
            "cmp_test",
            "--request",
            "-",
            "--idempotency-key",
            "submit_1",
        ],
        body,
        response(
            "202 Accepted",
            "application/json",
            "",
            br#"{"execution_id":"exec_1","state":"Queued"}"#,
        ),
    );
    assert_eq!(result(&out, 0)["state"], "Queued");
    assert!(request.starts_with("POST /v1alpha1/computers/cmp_test/executions HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer private-test-token\r\n"));
    assert!(request.contains("idempotency-key: submit_1\r\n"));
    assert!(request.ends_with(std::str::from_utf8(body).unwrap()));
    assert_eq!(extra, 0);
}

#[test]
fn routes_explicit_runtime_commands_without_implicit_mutations() {
    for (words, verb, path) in [
        (vec!["doctor"], "GET", "capabilities"),
        (vec!["computer", "show", "c"], "GET", "computers/c/runtime"),
        (vec!["computer", "start", "c"], "POST", "computers/c/start"),
        (
            vec!["computer", "cancel-start", "c"],
            "POST",
            "computers/c/start/cancel",
        ),
        (vec!["computer", "stop", "c"], "POST", "computers/c/stop"),
        (
            vec!["computer", "checkpoint-stop", "c"],
            "POST",
            "computers/c/checkpoint-stop",
        ),
        (
            vec!["connect", "c"],
            "POST",
            "computers/c/connection-sessions",
        ),
        (
            vec!["connection", "show", "s"],
            "GET",
            "connection-sessions/s",
        ),
        (
            vec!["connection", "heartbeat", "s"],
            "POST",
            "connection-sessions/s/heartbeat",
        ),
        (vec!["disconnect", "s"], "DELETE", "connection-sessions/s"),
        (vec!["lease", "acquire", "c"], "POST", "computers/c/leases"),
        (vec!["lease", "show", "l"], "GET", "leases/l"),
        (vec!["lease", "renew", "l"], "POST", "leases/l/renew"),
        (vec!["lease", "release", "l"], "POST", "leases/l/release"),
        (vec!["status", "e"], "GET", "executions/e"),
        (vec!["cancel", "e"], "POST", "executions/e/cancel"),
        (vec!["logs", "e"], "GET", "executions/e/output"),
    ] {
        let mut args = words;
        if verb == "POST" {
            args.extend(["--request", "-", "--idempotency-key", "key"]);
        }
        let (out, request, extra) = serve(
            &args,
            b"{}",
            response("200 OK", "application/json", "", b"null"),
        );
        assert_eq!(result(&out, 0), Value::Null);
        assert!(
            request.starts_with(&format!("{verb} /v1alpha1/{path} HTTP/1.1\r\n")),
            "{request}"
        );
        assert_eq!(extra, 0);
    }
}

#[test]
fn rejection_preserves_conflict_details_and_transport_loss_reports_uncertainty() {
    let args = [
        "exec",
        "c",
        "--request",
        "-",
        "--idempotency-key",
        "original_key",
    ];
    let (out, _, extra) = serve(&args, b"{}", response("409 Conflict", "application/json", "", br#"{"code":"idempotency_conflict","message":"Different input","request_id":"req_1","retryable":false,"details":{}}"#));
    let report = result(&out, 1);
    assert_eq!(report["code"], "idempotency_conflict");
    assert_eq!(report["http_status"], 409);
    assert_eq!(report["request_id"], "req_1");
    assert!(report["request_may_have_been_applied"].is_null());
    assert_eq!(extra, 0);
    for status in ["408 Request Timeout", "503 Service Unavailable"] {
        let (out, _, extra) = serve(
            &args,
            b"{}",
            response(
                status,
                "application/json",
                "",
                br#"{"code":"unavailable","message":"Unavailable","retryable":true}"#,
            ),
        );
        let report = result(&out, 1);
        assert_eq!(report["request_may_have_been_applied"], true);
        assert_eq!(report["retryable"], true);
        assert_eq!(extra, 0);
    }
    let (out, _, extra) = serve(&args, b"{}", Vec::new());
    let report = result(&out, 2);
    assert_eq!(report["request_may_have_been_applied"], true);
    assert_eq!(report["retryable"], false);
    assert!(!report.to_string().contains("private-test-token"));
    assert_eq!(extra, 0);
}

#[test]
fn redirects_never_forward_credentials_or_replay_mutations() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let location = format!(
        "Location: http://{}/collect\r\n",
        trap.local_addr().unwrap()
    );
    for status in ["302 Found", "307 Temporary Redirect"] {
        let (out, _, extra) = serve(
            &["exec", "c", "--request", "-", "--idempotency-key", "k"],
            b"{}",
            response(status, "text/html", &location, b"redirect"),
        );
        assert_eq!(result(&out, 1)["code"], "http_error");
        assert_eq!(extra, 0);
        assert_eq!(
            trap.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn rejects_unsafe_configuration_and_invalid_input_before_contacting_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    for url in [
        "http://example.com",
        "http://localhost",
        "ftp://127.0.0.1",
        "https://user:password@example.com",
        "https://example.com/api",
        "https://example.com?token=private",
        "https://example.com#secret",
    ] {
        assert_eq!(
            result(&invoke(url, &["status", "x"], b""), 2)["code"],
            "invalid_endpoint"
        );
    }
    for args in [
        vec!["exec", "c"],
        vec!["exec", "../c", "--request", "-", "--idempotency-key", "k"],
        vec!["status", "c", "--request", "-"],
        vec!["logs", "e", "--stream", "stdout"],
        vec!["logs", "e", "--stream", "stdout", "--output", "-"],
        vec!["status", "c", "--json"],
        vec![
            "exec",
            "c",
            "--request",
            "-",
            "--idempotency-key",
            "bad\nkey",
        ],
    ] {
        result(&invoke(&endpoint, &args, b"{}"), 2);
    }
    for body in [
        b"not-json private secret".to_vec(),
        b"[]".to_vec(),
        vec![b' '; 65537],
    ] {
        let report = result(
            &invoke(
                &endpoint,
                &["exec", "c", "--request", "-", "--idempotency-key", "k"],
                &body,
            ),
            2,
        );
        assert!(!report.to_string().contains("private secret"));
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

fn output_response(bytes: &[u8], truncated: bool, eof: bool) -> Vec<u8> {
    response(
        "200 OK",
        "application/octet-stream",
        &format!(
            "X-Output-Sha256: sha256:{:x}\r\nX-Output-Manifest-Digest: sha256:{}\r\nX-Output-Observed-Bytes: {}\r\nX-Output-Truncated: {truncated}\r\nX-Output-Eof: {eof}\r\n",
            Sha256::digest(bytes),
            "a".repeat(64),
            bytes.len() + usize::from(truncated)
        ),
        bytes,
    )
}

#[test]
fn downloads_verified_binary_and_empty_streams_preserving_collection_limits() {
    let dir = tempfile::tempdir().unwrap();
    for (i, bytes) in [vec![0, 255, 27, 10], vec![], vec![b'x'; 1024 * 1024]]
        .iter()
        .enumerate()
    {
        let file = dir.path().join(format!("{i}.bin"));
        let (out, request, _) = serve(
            &[
                "logs",
                "exec_1",
                "--stream",
                "stdout",
                "--output",
                file.to_str().unwrap(),
            ],
            b"",
            output_response(bytes, i == 0, i != 0),
        );
        let report = result(&out, 0);
        assert_eq!(std::fs::read(file).unwrap(), *bytes);
        assert_eq!(report["truncated"], i == 0);
        assert_eq!(report["eof"], i != 0);
        assert!(request.starts_with("GET /v1alpha1/executions/exec_1/output/stdout "));
    }
}

#[test]
fn corrupt_incomplete_oversized_and_existing_outputs_are_never_published_or_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("result.bin");
    let args = [
        "logs",
        "e",
        "--stream",
        "stderr",
        "--output",
        file.to_str().unwrap(),
    ];
    let mut corrupt = output_response(b"valid", false, true);
    *corrupt.last_mut().unwrap() = b'!';
    let mut incomplete = output_response(b"valid", false, true);
    incomplete.pop();
    for response in [
        corrupt,
        incomplete,
        output_response(&vec![0; 1024 * 1024 + 1], false, true),
        response("200 OK", "text/html", "", b"not output"),
    ] {
        let (out, _, _) = serve(&args, b"", response);
        result(&out, 2);
        assert!(!file.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    std::fs::write(&file, "keep me").unwrap();
    let (out, _, _) = serve(&args, b"", output_response(b"new", false, true));
    assert_eq!(result(&out, 2)["code"], "output_unavailable");
    assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn malformed_and_unbounded_json_responses_do_not_produce_success_output() {
    for reply in [
        response("200 OK", "application/json", "", b"{broken"),
        response("200 OK", "text/html", "", b"private upstream text"),
        response(
            "200 OK",
            "application/json",
            "",
            &vec![b' '; 4 * 1024 * 1024 + 1],
        ),
    ] {
        let (out, _, _) = serve(&["status", "e"], b"", reply);
        result(&out, 2);
    }
}
