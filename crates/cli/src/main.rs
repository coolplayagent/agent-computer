#![forbid(unsafe_code)]

use agent_computer_core::{API_VERSION, VERSION};
use std::process::ExitCode;

mod remote;
mod validate;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["help" | "--help" | "-h"] => {
            println!(
                "agent-computer {VERSION}\n\nUsage: agent-computer <command> [--json]\n\nCommands:\n  version                Build and API version\n  capabilities           Client capability status\n  validate <file|->       Static ComputerSet validation (YAML or JSON)\n  schema computer-set    Export the structural JSON Schema\n\n{}\nValidation: [--format yaml|json] [--json]",
                remote::HELP
            );
        }
        ["version" | "--version" | "-V"] => println!("agent-computer {VERSION}"),
        ["version", "--json"] => {
            println!("{{\"version\":\"{VERSION}\",\"api_version\":\"{API_VERSION}\"}}");
        }
        ["capabilities", "--json"] => {
            println!(
                "{{\"api_version\":\"{API_VERSION}\",\"stage\":\"development\",\"capabilities\":{{\"definitions.validate\":\"static\",\"definitions.schema\":\"supported\",\"definitions.plan\":\"unsupported\",\"definitions.apply\":\"unsupported\",\"computer\":\"unsupported\",\"browser\":\"unsupported\",\"execution\":\"unsupported\",\"client.execution\":\"authenticated-http\",\"artifacts\":\"unsupported\",\"presentation\":\"unsupported\",\"deployment\":\"unsupported\",\"mcp\":\"unsupported\",\"evaluation\":\"unsupported\"}}}}"
            );
        }
        ["capabilities"] => {
            println!(
                "Static ComputerSet validation, schema export and an authenticated HTTP execution client are available. Remote service capabilities are deployment-dependent; use doctor. Full Computer/browser runtime support remains incomplete."
            );
        }
        ["validate", rest @ ..] => return ExitCode::from(validate::run(rest)),
        [
            "computer" | "connect" | "disconnect" | "connection" | "lease" | "exec" | "status"
            | "logs" | "cancel" | "doctor",
            ..,
        ] => return ExitCode::from(remote::run(&words)),
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
