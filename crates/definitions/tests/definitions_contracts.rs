use agent_computer_definitions::*;
use serde_json::{Value, json};

const EXAMPLE: &[u8] = include_bytes!("../../../examples/research.computer.yaml");

fn example() -> Value {
    serde_yaml_ng::from_slice(EXAMPLE).unwrap()
}
fn check(value: &Value) -> Result<ValidatedDefinition, Box<ValidationReport>> {
    validate_bytes(&serde_json::to_vec(value).unwrap(), Format::Json)
}
fn invalid(value: &Value, code: &str) -> Box<ValidationReport> {
    let report = check(value).unwrap_err();
    assert!(!report.valid);
    assert!(report.definition_digest.is_none());
    assert!(
        report.diagnostics.iter().any(|d| d.code == code),
        "{report:?}"
    );
    report
}
fn web_app(value: &mut Value) {
    value["spec"]["apps"][0] = json!({
        "name":"browser", "driver":"web-application", "sandboxRef":"browser-env",
        "argv":["node", "server.js"], "cwd":"/app",
        "health":{"port":3000,"path":"/healthz","startupTimeoutSeconds":120},
        "statePaths":["/data/state"], "exportPaths":["/data/state"]
    });
}

#[path = "cases/canonical.rs"]
mod canonical;
#[path = "cases/parsing.rs"]
mod parsing;
#[path = "cases/references.rs"]
mod references;
#[path = "cases/sandbox.rs"]
mod sandbox;
