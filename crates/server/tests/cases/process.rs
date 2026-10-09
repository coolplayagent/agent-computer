use crate::support::document;
use agent_computer_test_support::Postgres;
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

fn binary() -> PathBuf {
    PathBuf::from(
        option_env!("CARGO_BIN_EXE_agent-computer-server")
            .or(option_env!("AGENT_COMPUTER_SERVER_BIN"))
            .expect("test binary path"),
    )
}
fn command(args: &[&str], database_file: &Path) -> Output {
    Command::new(binary())
        .args(args)
        .arg("--database-url-file")
        .arg(database_file)
        .output()
        .unwrap()
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, Value) {
    http_with_key(address, method, path, token, None, body)
}

fn http_with_key(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
    body: &str,
) -> (u16, Value) {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let authentication = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let idempotency = key
        .map(|key| format!("Idempotency-Key: {key}\r\n"))
        .unwrap_or_default();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{authentication}{idempotency}\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

#[tokio::test]
async fn operator_commands_and_real_http_reject_revoked_credentials() {
    let database = Postgres::new().await;
    let files = tempfile::tempdir().unwrap();
    let database_file = files.path().join("database-url");
    let token_file = files.path().join("credential");
    fs::write(&database_file, database.connection_url()).unwrap();
    fs::set_permissions(&database_file, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(command(&["migrate"], &database_file).status.success());
    let issue = [
        "credential-issue",
        "--organization",
        "acme",
        "--principal",
        "worker",
        "--kind",
        "agent",
        "--scopes",
        "definitions.validate,definitions.manage,runtime.read,runtime.app.use,runtime.connect,runtime.modify,runtime.activate",
        "--ttl-seconds",
        "3600",
        "--output",
        token_file.to_str().unwrap(),
    ];
    let issued = command(&issue, &database_file);
    assert!(
        issued.status.success(),
        "{}",
        String::from_utf8_lossy(&issued.stderr)
    );
    let metadata: Value = serde_json::from_slice(&issued.stdout).unwrap();
    let token = fs::read_to_string(&token_file).unwrap();
    let token = token.trim();
    assert_eq!(
        fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!String::from_utf8_lossy(&issued.stdout).contains(token));
    assert!(!String::from_utf8_lossy(&issued.stderr).contains(token));
    assert!(!command(&issue, &database_file).status.success()); // Cannot overwrite an existing secret.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM service_credentials")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let mut server = Server(
        Command::new(binary())
            .args(["serve", "--database-url-file"])
            .arg(&database_file)
            .args(["--listen", "127.0.0.1:0"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = server.0.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stderr).read_line(&mut line).unwrap();
        let _ = sender.send(line);
    });
    let line = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("server startup");
    let address: SocketAddr = line
        .trim()
        .strip_prefix("agent-computer-server listening on ")
        .expect("startup address")
        .parse()
        .unwrap();
    assert_eq!(http(address, "GET", "/ready", None, "").0, 200);
    assert_eq!(
        http(
            address,
            "POST",
            "/v1alpha1/definitions/validate",
            None,
            "invalid"
        )
        .0,
        401
    );
    assert_eq!(
        http(
            address,
            "POST",
            "/v1alpha1/definitions/validate",
            Some(token),
            &document().to_string()
        )
        .0,
        200
    );
    for kind in ["declaration", "agent"] {
        let granted = command(
            &[
                "definition-grant",
                "--organization",
                "acme",
                "--principal",
                "worker",
                "--kind",
                kind,
                "--name",
                "*",
                "--permission",
                "create",
            ],
            &database_file,
        );
        assert!(
            granted.status.success(),
            "{}",
            String::from_utf8_lossy(&granted.stderr)
        );
    }
    let (status, plan) = http_with_key(
        address,
        "POST",
        "/v1alpha1/plans",
        Some(token),
        Some("tcp-plan"),
        &document().to_string(),
    );
    assert_eq!(status, 201, "{plan}");
    let path = format!(
        "/v1alpha1/plans/{}/apply",
        plan["plan_id"].as_str().unwrap()
    );
    let apply = serde_json::json!({"plan_digest":plan["plan_digest"]}).to_string();
    let (status, operation) = http_with_key(
        address,
        "POST",
        &path,
        Some(token),
        Some("tcp-apply"),
        &apply,
    );
    assert_eq!(status, 202, "{operation}");
    assert_eq!(operation["state"], "Queued");
    assert_eq!(
        http_with_key(
            address,
            "POST",
            &path,
            Some(token),
            Some("tcp-apply"),
            &apply
        )
        .1,
        operation
    );
    let catalog = command(
        &[
            "catalog-register",
            "--organization",
            "acme",
            "--kind",
            "storage_class",
            "--name",
            "test-storage",
        ],
        &database_file,
    );
    assert!(catalog.status.success());
    let catalog: Value = serde_json::from_slice(&catalog.stdout).unwrap();
    assert_eq!(catalog["scope"], "reference-metadata");
    assert!(
        command(
            &[
                "catalog-disable",
                "--organization",
                "acme",
                "--resource-id",
                catalog["resource_id"].as_str().unwrap()
            ],
            &database_file
        )
        .status
        .success()
    );
    let operation_id = operation["operation_id"].as_str().unwrap();
    let inspect = command(
        &[
            "reconciliation-inspect",
            "--organization",
            "acme",
            "--operation",
            operation_id,
        ],
        &database_file,
    );
    assert!(inspect.status.success());
    let status: Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(status["progress"][0]["state"], "Pending");
    let store = agent_computer_store::Store::new(database.pool.clone());
    let computer = super::connections::provision(&store, token, "worker").await;
    let connect_path = format!("/v1alpha1/computers/{computer}/connection-sessions");
    let (code, session) = http_with_key(
        address,
        "POST",
        &connect_path,
        Some(token),
        Some("tcp-connect"),
        r#"{"requested_capabilities":["connect","read","modify"]}"#,
    );
    assert_eq!(code, 201, "{session}");
    assert_eq!(
        session["capabilities"],
        serde_json::json!(["connect", "read"])
    );
    let session_path = format!(
        "/v1alpha1/connection-sessions/{}",
        session["session_id"].as_str().unwrap()
    );
    let (code, heartbeat) = http_with_key(
        address,
        "POST",
        &format!("{session_path}/heartbeat"),
        Some(token),
        Some("tcp-heartbeat"),
        r#"{"expected_revision":1,"activity":"active","visibility":"visible"}"#,
    );
    assert_eq!(code, 200, "{heartbeat}");
    assert_eq!(heartbeat["expires_at_ms"], session["expires_at_ms"]);
    assert_eq!(
        http(address, "DELETE", &session_path, Some(token), "").1["state"],
        "Closed"
    );
    use agent_computer_store::reconciliation::{
        ClaimOutcome, ReconcileOutcome, ReconcileReason, WorkerId,
    };
    let ClaimOutcome::Claimed(lease) = store
        .claim_reconciliation(
            &agent_computer_core::identity::OrganizationId::new("acme").unwrap(),
            &WorkerId::new("process-test").unwrap(),
            Duration::from_secs(30),
        )
        .await
        .unwrap()
    else {
        panic!("expected claim")
    };
    store
        .finish_reconciliation(
            &lease,
            ReconcileOutcome::Blocked {
                reason: ReconcileReason::BackendUnavailable,
            },
        )
        .await
        .unwrap();
    let resumed = command(
        &[
            "reconciliation-resume",
            "--organization",
            "acme",
            "--operation",
            operation_id,
        ],
        &database_file,
    );
    assert!(resumed.status.success());
    let resumed: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(resumed["progress"][0]["state"], "Pending");
    let ClaimOutcome::Claimed(lease) = store
        .claim_reconciliation(
            &agent_computer_core::identity::OrganizationId::new("acme").unwrap(),
            &WorkerId::new("process-test").unwrap(),
            Duration::from_secs(30),
        )
        .await
        .unwrap()
    else {
        panic!("expected claim")
    };
    store
        .finish_reconciliation(
            &lease,
            ReconcileOutcome::Blocked {
                reason: ReconcileReason::BackendUnavailable,
            },
        )
        .await
        .unwrap();
    let abandoned = command(
        &[
            "reconciliation-abandon",
            "--organization",
            "acme",
            "--operation",
            operation_id,
        ],
        &database_file,
    );
    assert!(abandoned.status.success());
    let abandoned: Value = serde_json::from_slice(&abandoned.stdout).unwrap();
    assert_eq!(abandoned["state"], "Failed");
    assert_eq!(abandoned["progress"][0]["reason"], "operator_abandoned");
    assert_eq!(
        http(
            address,
            "GET",
            &format!("/v1alpha1/operations/{operation_id}"),
            Some(token),
            ""
        )
        .1["state"],
        "Failed"
    );
    let start = super::writers::prepared(&store, &database.pool, token, &computer, "worker").await;
    let (code, writer_session) = http_with_key(
        address,
        "POST",
        &connect_path,
        Some(token),
        Some("tcp-writer-connect"),
        r#"{"requested_capabilities":["connect","read","modify"]}"#,
    );
    assert_eq!(code, 201, "{writer_session}");
    let (code, writer) = http_with_key(address, "POST", &format!("/v1alpha1/computers/{computer}/leases"), Some(token), Some("tcp-writer"), &serde_json::json!({"scope":"modify","connection_session_id":writer_session["session_id"],"candidate_id":start.candidate_id,"generation":start.generation}).to_string());
    assert_eq!(code, 201, "{writer}");
    let writer_session_path = format!(
        "/v1alpha1/connection-sessions/{}",
        writer_session["session_id"].as_str().unwrap()
    );
    assert_eq!(
        http(address, "DELETE", &writer_session_path, Some(token), "").0,
        200
    );
    let reconciled = command(
        &[
            "writer-lease-reconcile",
            "--organization",
            "acme",
            "--lease-id",
            writer["lease_id"].as_str().unwrap(),
        ],
        &database_file,
    );
    assert!(
        reconciled.status.success(),
        "{}",
        String::from_utf8_lossy(&reconciled.stderr)
    );
    let released: Value = serde_json::from_slice(&reconciled.stdout).unwrap();
    assert_eq!(released["state"], "Released");
    assert_eq!(released["release_proof"], "no_dispatch");
    let profile = command(
        &[
            "catalog-register",
            "--organization",
            "acme",
            "--kind",
            "browser_profile",
            "--name",
            "private-profile",
        ],
        &database_file,
    );
    assert!(profile.status.success());
    let profile: Value = serde_json::from_slice(&profile.stdout).unwrap();
    let profile_id = profile["resource_id"].as_str().unwrap();
    let access_path = format!("/v1alpha1/runtime-access/browser_profile/{profile_id}");
    assert_eq!(http(address, "GET", &access_path, Some(token), "").0, 404);
    for permission in ["read", "app.use"] {
        let granted = command(
            &[
                "runtime-grant",
                "--organization",
                "acme",
                "--principal",
                "worker",
                "--kind",
                "browser_profile",
                "--resource-id",
                profile_id,
                "--permission",
                permission,
            ],
            &database_file,
        );
        assert!(
            granted.status.success(),
            "{}",
            String::from_utf8_lossy(&granted.stderr)
        );
    }
    let (status, access) = http(address, "GET", &access_path, Some(token), "");
    assert_eq!(status, 200);
    assert_eq!(
        access["permissions"],
        serde_json::json!(["app.use", "read"])
    );
    let revoked = command(
        &[
            "runtime-revoke",
            "--organization",
            "acme",
            "--principal",
            "worker",
            "--kind",
            "browser_profile",
            "--resource-id",
            profile_id,
            "--permission",
            "read",
        ],
        &database_file,
    );
    assert!(revoked.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&revoked.stdout).unwrap()["process_termination_confirmed"],
        false
    );
    assert_eq!(http(address, "GET", &access_path, Some(token), "").0, 404);
    let revoke = command(
        &[
            "credential-revoke",
            "--organization",
            "acme",
            "--credential",
            metadata["credential_id"].as_str().unwrap(),
        ],
        &database_file,
    );
    assert!(revoke.status.success());
    assert_eq!(
        http(
            address,
            "POST",
            "/v1alpha1/definitions/validate",
            Some(token),
            &document().to_string()
        )
        .0,
        401
    );
    assert!(
        Command::new("kill")
            .args(["-TERM", &server.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "graceful shutdown did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn private_database_files_and_cli_arguments_fail_closed() {
    let files = tempfile::tempdir().unwrap();
    let secret = files.path().join("database-url");
    fs::write(
        &secret,
        "postgresql://do-not-print:this-secret@localhost/missing",
    )
    .unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    let result = command(&["migrate"], &secret);
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("this-secret"));
    assert!(result.stdout.is_empty());
    let link = files.path().join("symlink");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    assert!(!command(&["migrate"], &link).status.success());
    let output = Command::new(binary())
        .args(["serve", "--database-url", "do-not-print-this"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("do-not-print-this"));
}
