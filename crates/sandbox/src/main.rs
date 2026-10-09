#![forbid(unsafe_code)]
use agent_computer_sandbox::{Bootstrap, MAX_REQUEST_BYTES, Request, run, startup};
use std::io::{Read, Write};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = execute().await {
        eprintln!("{error}");
        std::process::exit(125);
    }
}

async fn execute() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 || (args[0] != "--request" && args[0] != "--startup") {
        return Err(
            "usage: agent-computer-sandbox --request PATH | --startup BOOTSTRAP_PATH".into(),
        );
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&args[1])?
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if args[0] == "--startup" {
        write_report(&startup(Bootstrap::parse(&bytes)?).await?)
    } else {
        write_report(&run(Request::parse(&bytes)?).await?)
    }
}

fn write_report(report: &impl serde::Serialize) -> Result<(), Box<dyn std::error::Error>> {
    // Local bounded component output. Durable chunk/object upload belongs to the
    // trusted collector, which must not infer fencing from this JSON or exit code.
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, report)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
