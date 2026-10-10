use agent_computer_watchdog::{
    Error, MAX_REQUEST_BYTES, Observation, Request, journal::Journal, run, run_journaled,
    write_frame,
};
use std::{fs::File, io::Read, process::ExitCode};
mod service;

fn main() -> ExitCode {
    // Rust's default ignored SIGPIPE allows a lost acknowledgement reader to
    // enter the immediate-kill path instead of terminating this process.
    match execute() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
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
    if args.len() == 4 && (args[1] == "--reap" || args[1] == "--reap-once") && args[2] == "--spool"
    {
        return service::run(std::path::Path::new(&args[3]), args[1] == "--reap-once");
    }
    if !matches!(args.len(), 3 | 5)
        || args[1] != "--request"
        || (args.len() == 5 && args[3] != "--journal")
    {
        return Err(Error::InvalidRequest);
    }
    let mut bytes = Vec::new();
    File::open(&args[2])
        .map_err(|_| Error::InvalidRequest)?
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidRequest)?;
    let output = std::io::stdout();
    let request = Request::parse(&bytes)?;
    let report = if args.len() == 5 {
        let journal = Journal::create(std::path::Path::new(&args[4]), &request)?;
        run_journaled(request, &output, journal)?
    } else {
        run(request, &output)?
    };
    write_frame(&output, &report)?;
    Ok(report.observation == Observation::EmptyObserved)
}
