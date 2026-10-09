#![forbid(unsafe_code)]
use agent_computer_sandbox::{MAX_REQUEST_BYTES, Request, run};
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
    if args.len() != 2 || args[0] != "--request" {
        return Err("usage: agent-computer-sandbox --request PATH".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&args[1])?
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let report = run(Request::parse(&bytes)?).await?;
    // Local bounded component output. Durable chunk/object upload belongs to the
    // trusted collector, which must not infer fencing from this JSON or exit code.
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &report)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
