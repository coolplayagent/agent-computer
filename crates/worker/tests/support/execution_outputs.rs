//! Output recovery runs in a fresh operator process with unavailable Kubernetes credentials.
use super::*;

#[allow(clippy::too_many_arguments)]
pub async fn verify(
    config: &Value,
    private: &Value,
    store: &Store,
    token: &str,
    org: &OrganizationId,
    id: &str,
    case: &str,
    outcome: &Value,
) -> Value {
    let expected = matches!(
        case,
        "normal"
            | "renew-short"
            | "renew-long"
            | "command"
            | "output-store-failure"
            | "output-db-failure"
            | "output-truncated"
            | "output-failed"
            | "output-timeout"
            | "output-completion-retry"
    );
    let pending = matches!(case, "output-store-failure" | "output-db-failure");
    let before = store.candidate_execution_output(token, id).await.unwrap();
    if !expected {
        assert!(before.is_none());
        return json!({"state":"absent"});
    }
    let before = before.unwrap();
    assert_eq!(
        before.state,
        if pending {
            OutputState::Pending
        } else {
            OutputState::Verified
        }
    );
    assert_eq!(outcome["output_unconfirmed"], pending);
    let path =
        PathBuf::from(field(config, "result_file")).with_file_name("output-recovery-command.json");
    fs::write(&path, serde_json::to_vec(private).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let run = |command: &str| {
        std::process::Command::new(field(config, "server_binary"))
            .args([
                command,
                "--database-url-file",
                field(config, "database_url_file"),
                "--organization",
                org.as_str(),
                "--execution-id",
                id,
                "--config-file",
            ])
            .arg(&path)
            .output()
            .unwrap()
    };
    if pending {
        let read = run("execution-output-read");
        assert!(!read.status.success());
        assert!(read.stdout.is_empty());
    }
    let recovered = run("execution-output-recover");
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let recovered: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(recovered["state"], "verified");
    assert_eq!(recovered["manifest_digest"], before.manifest_digest);
    // Repeated publication must return the same receipt without a new execution.
    for _ in 0..2 {
        let retry = run("execution-output-recover");
        assert!(retry.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&retry.stdout).unwrap(),
            recovered
        );
    }
    let read = run("execution-output-read");
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    let report: Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(report["report"]["execution_id"], id);
    assert_eq!(
        report["report"]["outcome"],
        match case {
            "output-failed" => "failed",
            "output-timeout" => "timed_out",
            _ => "succeeded",
        }
    );
    if case != "command" {
        assert_eq!(report, outcome["raw_report"]);
    }
    let truncated = case == "output-truncated";
    for stream in ["stdout", "stderr"] {
        assert_eq!(recovered[stream]["truncated"], truncated);
        assert_eq!(recovered[stream]["eof"], true);
        if truncated {
            assert_eq!(recovered[stream]["retained_bytes"], 4096);
            assert_eq!(recovered[stream]["observed_bytes"], 5000);
            assert_eq!(
                report["report"][stream]["bytes"].as_array().unwrap().len(),
                4096
            );
        }
    }
    let http = output_http::verify(config, private, token, id, &report, &recovered).await;
    let pool = sqlx::PgPool::connect(
        fs::read_to_string(field(config, "database_url_file"))
            .unwrap()
            .trim(),
    )
    .await
    .unwrap();
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE organization=$1 AND payload->>'execution_id'=$2 AND kind IN ('execution.output_pending','execution.output_verified')")
        .bind(org.as_str()).bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 2);
    let grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM execution_startup_grants WHERE organization=$1 AND execution_id=$2",
    )
    .bind(org.as_str())
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grants, 1);
    json!({"http_downloads":http,"initial_state":before.state,"recovered":recovered,"fresh_process_read_matches":true,"recovery_retries":2,"output_events":events,"startup_grants":grants})
}
