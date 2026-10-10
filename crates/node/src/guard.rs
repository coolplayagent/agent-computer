use crate::{Error, Result, command, observation::RuntimeObservation};
use agent_computer_watchdog::{Request, boottime_ms};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    process::{Child, ChildStdout},
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArmedReceipt {
    pub version: u8,
    pub event: String,
    pub request: Request,
    pub cgroup_device: u64,
    pub armed_boottime_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct Evidence {
    pub runtime: RuntimeObservation,
    pub armed: ArmedReceipt,
    pub observed_boottime_ms: u64,
}

/// Live, non-cloneable handle. Serialized evidence alone cannot recreate this.
/// Dropping it detaches; the independent watchdog keeps the original deadline.
#[derive(Debug)]
pub struct ArmedGuard {
    evidence: Evidence,
    child: DetachedChild,
    _stdout: ChildStdout,
}
impl ArmedGuard {
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }
    pub fn remaining_budget_ms(&mut self) -> Result<u32> {
        let remaining = self
            .evidence
            .armed
            .request
            .deadline_boottime_ms
            .saturating_sub(boottime_ms());
        if remaining == 0
            || self
                .child
                .process()
                .try_wait()
                .map_err(|_| Error::WatchdogUnavailable)?
                .is_some()
        {
            return Err(Error::Deadline);
        }
        u32::try_from(remaining).map_err(|_| Error::Deadline)
    }
}

/// Reap without signalling the independent timer, including partial setup errors.
#[derive(Debug)]
struct DetachedChild(Option<Child>);
impl DetachedChild {
    fn process(&mut self) -> &mut Child {
        self.0.as_mut().expect("child is present until drop")
    }
}
impl Drop for DetachedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = std::thread::Builder::new()
                .name("watchdog-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

pub(crate) fn launch(
    executable: &File,
    spool: &std::path::Path,
    request: Request,
    runtime: RuntimeObservation,
    deadline: Instant,
) -> Result<ArmedGuard> {
    let mut input = tempfile::NamedTempFile::new_in(spool).map_err(|_| Error::Configuration)?;
    let expected = serde_json::to_value(&request).map_err(|_| Error::Configuration)?;
    let bytes = serde_json::to_vec(&request).map_err(|_| Error::Configuration)?;
    // Also applies the watchdog's canonical path, ID and budget input bounds.
    Request::parse(&bytes).map_err(|_| Error::Configuration)?;
    input.write_all(&bytes).map_err(|_| Error::Configuration)?;
    input
        .as_file()
        .sync_all()
        .map_err(|_| Error::Configuration)?;
    if boottime_ms() >= request.deadline_boottime_ms {
        return Err(Error::Deadline);
    }
    // Do not create a child process group here: the CLI calls setsid itself.
    let mut child = DetachedChild(Some(
        command::command(executable, "agent-computer-watchdog")
            .args([std::ffi::OsStr::new("--request"), input.path().as_os_str()])
            .spawn()
            .map_err(|_| Error::WatchdogUnavailable)?,
    ));
    let mut stdout = child
        .process()
        .stdout
        .take()
        .ok_or(Error::WatchdogUnavailable)?;
    rustix::fs::fcntl_setfl(&stdout, rustix::fs::OFlags::NONBLOCK)
        .map_err(|_| Error::WatchdogUnavailable)?;
    // On any error close this reader, never kill or reset the independent guard.
    let armed = read_receipt(&mut stdout, child.process(), deadline)?;
    let now = boottime_ms();
    if armed.version != 1
        || armed.event != "armed"
        || armed.cgroup_device == 0
        || serde_json::to_value(&armed.request).map_err(|_| Error::InvalidObservation)? != expected
        || armed.armed_boottime_ms > now
        || armed.armed_boottime_ms >= request.deadline_boottime_ms
        || now >= request.deadline_boottime_ms
    {
        return Err(Error::IdentityMismatch);
    }
    let mut guard = ArmedGuard {
        evidence: Evidence {
            runtime,
            armed,
            observed_boottime_ms: now,
        },
        child,
        _stdout: stdout,
    };
    guard.remaining_budget_ms()?;
    Ok(guard)
}

fn read_receipt(
    stdout: &mut ChildStdout,
    child: &mut Child,
    deadline: Instant,
) -> Result<ArmedReceipt> {
    let mut bytes = Vec::new();
    loop {
        if Instant::now() >= deadline
            || child
                .try_wait()
                .map_err(|_| Error::WatchdogUnavailable)?
                .is_some()
        {
            return Err(Error::WatchdogUnavailable);
        }
        let mut byte = [0];
        match stdout.read(&mut byte) {
            Ok(0) => return Err(Error::WatchdogUnavailable),
            Ok(_) => {
                bytes.push(byte[0]);
                if bytes.len() > 4096 {
                    return Err(Error::ResponseLimit);
                }
                if byte[0] == b'\n' {
                    return serde_json::from_slice(&bytes).map_err(|_| Error::InvalidObservation);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Error::WatchdogUnavailable),
        }
    }
}
