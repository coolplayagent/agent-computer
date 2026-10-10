use crate::{Error, Output, Request, Result};
use rustix::{
    fd::AsRawFd,
    fs::{self, Mode, OFlags, ResolveFlags},
    process::{self, DumpableBehavior, Pid, Signal, WaitOptions, WaitStatus},
};
use serde::{Deserialize, Serialize};
use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    SpawnFailed,
    Cancelled,
    TimedOut,
    LeaseExpired,
    DescendantsTerminated,
    Unknown,
}

/// Local observation only: never a database completion receipt or writer drain proof.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub version: u32,
    pub execution_id: String,
    pub generation: u64,
    pub request_digest: String,
    pub outcome: Outcome,
    pub main_exit_code: Option<i32>,
    pub main_signal: Option<i32>,
    pub reaped_processes: u64,
    pub children_reaped: bool,
    pub kill_sent: bool,
    pub elapsed_ms: u64,
    pub stdout: Output,
    pub stderr: Output,
}

// This type and its constructor are private. There is no host-execution/test bypass.
pub(crate) struct NamespaceInit;
static STARTED: AtomicBool = AtomicBool::new(false);
impl NamespaceInit {
    pub(crate) fn check() -> Result<Self> {
        if process::getpid() != Pid::INIT
            || process::getuid().as_raw() != 1000
            || process::geteuid().as_raw() != 1000
            || process::getgid().as_raw() != 1000
            || process::getegid().as_raw() != 1000
            || std::fs::read_link("/proc/self").ok().as_deref() != Some(std::path::Path::new("1"))
        {
            return Err(Error::IsolationRequired);
        }
        let status =
            std::fs::read_to_string("/proc/self/status").map_err(|_| Error::IsolationRequired)?;
        for (key, expected) in [
            ("NoNewPrivs:", "1"),
            ("CapEff:", "0000000000000000"),
            ("CapPrm:", "0000000000000000"),
            ("CapBnd:", "0000000000000000"),
        ] {
            if !status
                .lines()
                .any(|l| l.strip_prefix(key).is_some_and(|v| v.trim() == expected))
            {
                return Err(Error::IsolationRequired);
            }
        }
        for entry in std::fs::read_dir("/proc").map_err(|_| Error::IsolationRequired)? {
            let name = entry.map_err(|_| Error::IsolationRequired)?.file_name();
            if name
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|pid| pid != 1)
            {
                return Err(Error::IsolationRequired);
            }
        }
        // Same-UID children must not access init's memory or output descriptors via /proc.
        process::set_dumpable_behavior(DumpableBehavior::NotDumpable).map_err(|_| Error::Setup)?;
        if STARTED.swap(true, Ordering::SeqCst) {
            return Err(Error::IsolationRequired);
        }
        Ok(Self)
    }

    fn signal_all(&self, signal: Signal) -> std::result::Result<(), rustix::io::Errno> {
        // kill(-1) reaches descendants which used setsid/setpgid as well. PID 1 is
        // excluded by Linux; calling this anywhere outside our namespace is forbidden.
        match process::kill_process_group(Pid::INIT, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Drop for NamespaceInit {
    fn drop(&mut self) {
        // Cancellation of the Rust future must also request termination. This
        // best-effort signal is not reported as a successful reap or fence.
        let _ = self.signal_all(Signal::KILL);
    }
}

/// Run exactly one command as PID 1 of an already isolated, credential-free container.
/// The caller must independently provide gVisor, mounts, resource/network limits,
/// admission, a conservative lease budget and an external runtime watchdog.
pub async fn run(request: Request) -> Result<Report> {
    request.validate()?;
    let namespace = NamespaceInit::check()?;
    run_anchored(request, namespace, Instant::now()).await
}

pub(crate) async fn run_anchored(
    request: Request,
    namespace: NamespaceInit,
    start: Instant,
) -> Result<Report> {
    run_controlled(request, namespace, start, None).await
}

pub(crate) async fn run_controlled(
    request: Request,
    namespace: NamespaceInit,
    start: Instant,
    mut control: Option<&mut crate::renewal::Channel>,
) -> Result<Report> {
    let process_start = Instant::now();
    let mut report = Report {
        version: 1,
        execution_id: request.execution_id.clone(),
        generation: request.generation,
        request_digest: request.digest()?,
        outcome: Outcome::Unknown,
        main_exit_code: None,
        main_signal: None,
        reaped_processes: 0,
        children_reaped: false,
        kill_sent: false,
        elapsed_ms: 0,
        stdout: Output::default(),
        stderr: Output::default(),
    };
    let mut term = signal(SignalKind::terminate()).map_err(|_| Error::Setup)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|_| Error::Setup)?;
    let mut hangup = signal(SignalKind::hangup()).map_err(|_| Error::Setup)?;
    let workspace = fs::open(
        "/workspace",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| Error::Setup)?;
    let cwd = if request.cwd.is_empty() {
        workspace
    } else {
        fs::openat2(
            &workspace,
            &request.cwd,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|_| Error::Setup)?
    };
    let timeout = Duration::from_secs(request.timeout_seconds.into());
    let lease = Duration::from_millis(request.lease_budget_ms.into());
    let command_deadline = process_start + timeout;
    let mut lease_deadline = start + lease;
    let deadline_outcome = if lease_deadline <= command_deadline {
        Outcome::LeaseExpired
    } else {
        Outcome::TimedOut
    };
    if Instant::now() >= command_deadline.min(lease_deadline) {
        report.outcome = deadline_outcome;
        report.children_reaped = true;
        report.elapsed_ms = elapsed(start);
        return Ok(report);
    }
    let mut command = Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/tmp")
        .env("LANG", "C")
        .current_dir(format!("/proc/self/fd/{}", cwd.as_raw_fd()))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            report.outcome = Outcome::SpawnFailed;
            report.children_reaped = true;
            report.elapsed_ms = elapsed(start);
            return Ok(report);
        }
    };
    let main_pid = child.id();
    let mut stdout = child.stdout.take().ok_or(Error::Setup)?;
    let mut stderr = child.stderr.take().ok_or(Error::Setup)?;
    // From this point every error follows the bounded termination/reap path.
    let nonblock = |fd| fs::fcntl_setfl(fd, OFlags::NONBLOCK);
    let pipe_setup_failed =
        nonblock(&stdout).is_err() || fs::fcntl_setfl(&stderr, OFlags::NONBLOCK).is_err();
    let mut stop: Option<(Outcome, Instant)> = None;
    let mut main_status: Option<WaitStatus> = None;
    let grace = Duration::from_millis(request.term_grace_ms.into());
    let mut fault = pipe_setup_failed;
    loop {
        let mut no_children = false;
        // Bound reaping work as well as output draining under a fork/output flood.
        for _ in 0..1024 {
            match process::wait(WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => {
                    report.reaped_processes += 1;
                    if pid.as_raw_nonzero().get() as u32 == main_pid && main_status.is_none() {
                        report.main_exit_code = status.exit_status();
                        report.main_signal = status.terminating_signal();
                        main_status = Some(status);
                    }
                }
                Ok(None) => break,
                Err(rustix::io::Errno::CHILD) => {
                    no_children = true;
                    break;
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    fault = true;
                    break;
                }
            }
        }
        if !pipe_setup_failed {
            fault |= report
                .stdout
                .drain(&mut stdout, request.output_limit_bytes)
                .is_err();
            fault |= report
                .stderr
                .drain(&mut stderr, request.output_limit_bytes)
                .is_err();
        }
        if stop.is_none()
            && let Some(control) = control.as_mut()
        {
            match control.poll() {
                Ok(()) | Err(Error::LeaseExpired) => {}
                Err(_) => fault = true,
            }
            lease_deadline = control.window.deadline();
        }
        let now = Instant::now();
        // Deadline has priority over an exit first observed after it expired.
        if stop.is_none() {
            let reason = if fault {
                Some(Outcome::Unknown)
            } else if now >= command_deadline.min(lease_deadline) {
                Some(if lease_deadline <= command_deadline {
                    Outcome::LeaseExpired
                } else {
                    Outcome::TimedOut
                })
            } else if main_status.is_some() && !no_children {
                Some(Outcome::DescendantsTerminated)
            } else {
                None
            };
            if let Some(reason) = reason {
                stop = Some((reason, now));
            }
        }
        if no_children && (pipe_setup_failed || (report.stdout.eof && report.stderr.eof)) {
            report.children_reaped = true;
            report.outcome = if fault {
                Outcome::Unknown
            } else if let Some((reason, _)) = stop {
                reason
            } else if main_status.is_some_and(|s| s.exit_status() == Some(0)) {
                Outcome::Succeeded
            } else {
                Outcome::Failed
            };
            break;
        }
        if let Some((_, stopped_at)) = stop {
            let sig = if now.duration_since(stopped_at) >= grace {
                report.kill_sent = true;
                Signal::KILL
            } else {
                Signal::TERM
            };
            fault |= namespace.signal_all(sig).is_err();
            if now.duration_since(stopped_at) >= grace + Duration::from_secs(2) {
                report.outcome = Outcome::Unknown;
                break;
            }
        }
        tokio::select! {
            _ = term.recv() => { stop.get_or_insert((Outcome::Cancelled, Instant::now())); },
            _ = interrupt.recv() => { stop.get_or_insert((Outcome::Cancelled, Instant::now())); },
            _ = hangup.recv() => { stop.get_or_insert((Outcome::Cancelled, Instant::now())); },
            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
    }
    // std::process::Child does not kill on Drop. All reaping is performed above;
    // never call Child::wait after wait(2) has already consumed the status.
    drop(child);
    report.elapsed_ms = elapsed(start);
    Ok(report)
}

fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn refuses_host_execution_without_spawning_a_child() {
        assert_ne!(process::getpid(), Pid::INIT);
        let request = Request {
            execution_id: "refused".into(),
            generation: 1,
            argv: vec!["/bin/false".into()],
            cwd: String::new(),
            timeout_seconds: 1,
            lease_budget_ms: 1000,
            term_grace_ms: 0,
            output_limit_bytes: 0,
        };
        assert_eq!(run(request).await.unwrap_err(), Error::IsolationRequired);
    }
}
