use serde_json::Value;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn executable() -> &'static str {
    option_env!("CARGO_BIN_EXE_agent-computer")
        .or(option_env!("AGENT_COMPUTER_BIN"))
        .expect("test binary location")
}

fn run(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(executable())
        .args(args)
        .env_remove("BUILD_WORKING_DIRECTORY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn bazel_launch_resolves_files_from_the_original_invocation_directory() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "agent-computer-cli-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(
        directory.join("declaration.yaml"),
        include_bytes!("../../../examples/research.computer.yaml"),
    )
    .unwrap();
    let result = Command::new(executable())
        .args(["validate", "declaration.yaml", "--json"])
        .env("BUILD_WORKING_DIRECTORY", &directory)
        .output()
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    assert!(result.status.success(), "{result:?}");
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["valid"], true);
}

#[test]
fn yaml_stdin_returns_a_static_digest_without_runtime_claims() {
    let result = run(
        &["validate", "-", "--json"],
        include_bytes!("../../../examples/research.computer.yaml"),
    );
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["valid"], true);
    assert_eq!(report["scope"], "static");
    assert_eq!(report["resource_count"], 7);
    assert!(
        report["definition_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(!report["external_references"].as_array().unwrap().is_empty());
}

#[test]
fn malformed_json_is_machine_readable_and_does_not_echo_source_secrets() {
    let result = run(
        &["validate", "--format", "json", "-", "--json"],
        b"{\"secret\": \"hidden-password\"",
    );
    assert_eq!(result.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert_eq!(report["diagnostics"][0]["code"], "invalid_document");
    assert!(!String::from_utf8_lossy(&result.stdout).contains("hidden-password"));
    assert!(result.stderr.is_empty());
}

#[test]
fn text_diagnostics_go_to_stderr_and_source_read_failures_exit_two() {
    let result = run(&["validate", "-", "--format", "json"], b"{}");
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("schema_violation"));
    let missing = run(
        &[
            "validate",
            "/nonexistent-agent-computer-fixture/declaration.yaml",
            "--json",
        ],
        b"",
    );
    assert_eq!(missing.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(report["diagnostics"][0]["code"], "input_unavailable");
}

#[test]
fn invalid_options_have_no_success_output() {
    for args in [
        vec!["validate", "--json"],
        vec!["validate", "-", "--format", "toml", "--json"],
        vec!["validate", "-", "--json", "--json"],
        vec!["validate", "-", "extra", "--json"],
    ] {
        let result = run(&args, b"");
        assert_eq!(result.status.code(), Some(2));
        let report: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["valid"], false);
        assert_eq!(report["diagnostics"][0]["code"], "usage");
    }
}

#[test]
fn capabilities_and_schema_distinguish_static_checks_from_runtime_support() {
    let capabilities = run(&["capabilities", "--json"], b"");
    assert!(capabilities.status.success());
    let capabilities: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(
        capabilities["capabilities"]["definitions.validate"],
        "static"
    );
    assert_eq!(
        capabilities["capabilities"]["definitions.apply"],
        "unsupported"
    );
    assert_eq!(capabilities["capabilities"]["computer"], "unsupported");
    let schema = run(&["schema", "computer-set", "--json"], b"");
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema["$defs"]["Computer"].is_object());
}

#[test]
fn oversized_stdin_is_rejected_before_parsing() {
    let result = run(&["validate", "-", "--json"], &vec![b' '; 1024 * 1024 + 1]);
    assert_eq!(result.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["diagnostics"][0]["code"], "document_too_large");
}
