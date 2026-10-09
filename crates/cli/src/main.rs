#![forbid(unsafe_code)]

use agent_computer_core::{API_VERSION, VERSION};
use std::process::ExitCode;

mod validate;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["help" | "--help" | "-h"] => {
            println!(
                "agent-computer {VERSION}\n\nUsage: agent-computer <command> [--json]\n\nCommands:\n  version                Build and API version\n  capabilities           Implemented capability status\n  validate <file|->      Static ComputerSet validation (YAML or JSON)\n  schema computer-set    Export the structural JSON Schema\n\nValidation: [--format yaml|json] [--json]\nNo running Computer service is included yet."
            );
        }
        ["version" | "--version" | "-V"] => println!("agent-computer {VERSION}"),
        ["version", "--json"] => {
            println!("{{\"version\":\"{VERSION}\",\"api_version\":\"{API_VERSION}\"}}");
        }
        ["capabilities", "--json"] => {
            println!(
                "{{\"api_version\":\"{API_VERSION}\",\"stage\":\"development\",\"capabilities\":{{\"definitions.validate\":\"static\",\"definitions.schema\":\"supported\",\"definitions.plan\":\"unsupported\",\"definitions.apply\":\"unsupported\",\"computer\":\"unsupported\",\"browser\":\"unsupported\",\"execution\":\"unsupported\",\"artifacts\":\"unsupported\",\"presentation\":\"unsupported\",\"deployment\":\"unsupported\",\"mcp\":\"unsupported\",\"evaluation\":\"unsupported\"}}}}"
            );
        }
        ["capabilities"] => {
            println!(
                "Static ComputerSet validation and schema export are available. Plan/apply, Computer, browser, execution, artifacts, presentation, deployment, MCP and evaluation runtime capabilities are unsupported."
            );
        }
        ["validate", rest @ ..] => return ExitCode::from(validate::run(rest)),
        ["schema", "computer-set"] | ["schema", "computer-set", "--json"] => {
            println!(
                "{}",
                serde_json::to_string_pretty(&agent_computer_definitions::schema())
                    .expect("schemas are JSON-serializable")
            );
        }
        _ => {
            eprintln!("unsupported command or arguments; run agent-computer --help");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
