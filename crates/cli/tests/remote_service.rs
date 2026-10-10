#[path = "support/runtime.rs"]
mod runtime;

use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{
    Store,
    auth::{IssueCredential, PrincipalKind, ServiceScope},
};
use agent_computer_test_support::Postgres;
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

fn cli(
    endpoint: &str,
    token_file: &std::path::Path,
    args: &[&str],
    body: Option<Value>,
    key: Option<&str>,
    exit: i32,
) -> Value {
    let mut command = Command::new(
        option_env!("CARGO_BIN_EXE_agent-computer")
            .or(option_env!("AGENT_COMPUTER_BIN"))
            .unwrap(),
    );
    command
        .args(args)
        .arg("--json")
        .env_remove("BUILD_WORKING_DIRECTORY")
        .env("AGENT_COMPUTER_ENDPOINT", endpoint)
        .env("AGENT_COMPUTER_TOKEN_FILE", token_file);
    if let Some(key) = key {
        command.args(["--request", "-", "--idempotency-key", key]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(body) = body {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&body).unwrap())
            .unwrap();
    }
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(exit), "{result:?}");
    if exit == 0 {
        assert!(result.stderr.is_empty());
        serde_json::from_slice(&result.stdout).unwrap()
    } else {
        assert!(result.stdout.is_empty());
        serde_json::from_slice(&result.stderr).unwrap()
    }
}

// The HTTP listener must keep running while the real CLI subprocess is awaited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_cli_http_postgres_connect_lease_submit_retry_disconnect_cancel_and_revocation() {
    let database = Postgres::new().await;
    let store = Store::new(database.pool.clone());
    store.migrate().await.unwrap();
    let credential = store
        .issue_credential(IssueCredential {
            organization: &OrganizationId::new("acme").unwrap(),
            principal: &PrincipalId::new("alice").unwrap(),
            kind: PrincipalKind::Agent,
            scopes: &ServiceScope::ALL,
            lifetime: Duration::from_secs(3600),
        })
        .await
        .unwrap();
    let token = credential.expose_token();
    let computer = runtime::provision_with_sandbox(&store, token, "alice", true).await;
    let start = runtime::prepared(&store, &database.pool, token, &computer, "alice").await;
    let sandbox: String =
        sqlx::query_scalar("SELECT resource_id FROM resource_definitions WHERE kind='sandbox'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let token_file = directory.path().join("token");
    std::fs::write(&token_file, token).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = agent_computer_server::router(store.clone());
    let service = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let call = |args: &[&str], body, key, exit| cli(&endpoint, &token_file, args, body, key, exit);
    let capabilities = call(&["doctor"], None, None, 0);
    assert_eq!(
        capabilities["capabilities"]["execution.admission"],
        "bounded-queued"
    );
    let status = call(&["computer", "show", &computer], None, None, 0);
    assert_eq!(status["start_state"], "Prepared");
    assert_eq!(status["ready"], false);
    let connect =
        json!({"requested_capabilities":["connect","read","modify"],"lifetime_seconds":900});
    let session = call(
        &["connect", &computer],
        Some(connect.clone()),
        Some("cli-connect"),
        0,
    );
    assert_eq!(
        call(
            &["connect", &computer],
            Some(connect),
            Some("cli-connect"),
            0
        )["session_id"],
        session["session_id"]
    );
    let session_id = session["session_id"].as_str().unwrap();
    let lease = call(
        &["lease", "acquire", &computer],
        Some(
            json!({"scope":"modify","connection_session_id":session_id,"candidate_id":start.candidate_id,"generation":start.generation,"duration_seconds":30}),
        ),
        Some("cli-lease"),
        0,
    );
    let input = json!({"lease_id":lease["lease_id"],"lease":{"connection_session_id":session_id,"generation":lease["generation"],"epoch":lease["epoch"],"expected_revision":lease["revision"]},"sandbox_id":sandbox,"command":{"argv":["/bin/echo","private-argv"],"cwd":"","timeout_seconds":10,"term_grace_ms":100,"output_limit_bytes":100}});
    let queued = call(
        &["exec", &computer],
        Some(input.clone()),
        Some("cli-exec"),
        0,
    );
    assert_eq!(queued["state"], "Queued");
    assert_eq!(queued["dispatch_started"], false);
    assert_eq!(queued["lifetime"], "background");
    assert!(!queued.to_string().contains("private-argv"));
    assert_eq!(
        call(
            &["exec", &computer],
            Some(input.clone()),
            Some("cli-exec"),
            0
        ),
        queued
    );
    let mut different = input.clone();
    different["command"]["argv"] = json!(["/bin/false"]);
    assert_eq!(
        call(&["exec", &computer], Some(different), Some("cli-exec"), 1)["http_status"],
        409
    );
    assert_eq!(
        call(&["disconnect", session_id], None, None, 0)["state"],
        "Closed"
    );
    let execution_id = queued["execution_id"].as_str().unwrap();
    assert_eq!(call(&["status", execution_id], None, None, 0), queued);
    assert_eq!(call(&["logs", execution_id], None, None, 0), Value::Null);
    let denied = call(
        &["exec", &computer],
        Some(input.clone()),
        Some("cli-new-after-close"),
        1,
    );
    assert!(!denied["http_status"].is_null());
    let cancel = json!({"expected_revision":queued["revision"]});
    let cancelled = call(
        &["cancel", execution_id],
        Some(cancel.clone()),
        Some("cli-cancel"),
        0,
    );
    assert_eq!(cancelled["state"], "Cancelled");
    assert_eq!(
        call(
            &["cancel", execution_id],
            Some(cancel),
            Some("cli-cancel"),
            0
        ),
        cancelled
    );
    assert_eq!(
        call(&["exec", &computer], Some(input), Some("cli-exec"), 0),
        cancelled
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_dispatch_intents")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_requests")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        1
    );
    // Revocation is authoritative even when the CLI still holds its original token file.
    store
        .disable_principal(
            &OrganizationId::new("acme").unwrap(),
            &PrincipalId::new("alice").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        call(&["status", execution_id], None, None, 1)["http_status"],
        401
    );
    service.abort();
    let _ = service.await;
}
