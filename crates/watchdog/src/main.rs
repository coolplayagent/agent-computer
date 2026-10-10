use agent_computer_watchdog::{Error, MAX_REQUEST_BYTES, Observation, Request, run, write_frame};
use std::{fs::File, io::Read, process::ExitCode};

fn main() -> ExitCode {
    // Rust's default ignored SIGPIPE allows a lost acknowledgement reader to
    // enter the immediate-kill path instead of terminating this process.
    match execute() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) | Err(_) => ExitCode::from(2),
    }
}

fn execute() -> Result<bool, Error> {
    // The node adapter may exit after receiving the receipt. Give the guard its
    // own session so the controller's terminal/session lifecycle cannot stop it.
    let pid = rustix::process::getpid();
    if rustix::process::getsid(None).map_err(|_| Error::Setup)? != pid {
        rustix::process::setsid().map_err(|_| Error::Setup)?;
    }
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 || args[1] != "--request" {
        return Err(Error::InvalidRequest);
    }
    let mut bytes = Vec::new();
    File::open(&args[2])
        .map_err(|_| Error::InvalidRequest)?
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidRequest)?;
    let output = std::io::stdout();
    let report = run(Request::parse(&bytes)?, &output)?;
    write_frame(&output, &report)?;
    Ok(report.observation == Observation::EmptyObserved)
}
