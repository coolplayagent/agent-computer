use crate::{Error, Result, command, observation::RuntimeObservation};
#[cfg(test)]
mod tests;
use agent_computer_watchdog::{Request, boottime_ms};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    process::{Child, ChildStdin, ChildStdout, Stdio},
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<agent_computer_watchdog::journal::Reference>,
}

#[derive(Debug, Serialize)]
pub struct Evidence {
    pub version: u8,
    pub runtime: RuntimeObservation,
    pub armed: ArmedReceipt,
    pub backup_armed: ArmedReceipt,
    pub watchdog_pids: [u32; 2],
    pub reaper: agent_computer_watchdog::admission::Receipt,
    pub observed_boottime_ms: u64,
}

/// Live, non-cloneable handle. Serialized evidence alone cannot recreate this.
/// Dropping it detaches the children. Fixed guards keep their original deadline;
/// renewable guards terminate when their control pipes close.
#[derive(Debug)]
pub struct ArmedGuard {
    evidence: Evidence,
    children: [DetachedChild; 2],
    stdout: [ChildStdout; 2],
    stdin: [Option<ChildStdin>; 2],
    renewals: Option<[agent_computer_watchdog::renewal::Receipt; 2]>,
    failed: bool,
    reaper: agent_computer_watchdog::admission::Client,
    pub(super) termination: crate::termination::ProcessDomain,
}
impl ArmedGuard {
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }
    pub fn remaining_budget_ms(&mut self) -> Result<u32> {
        if self.failed {
            return Err(Error::WatchdogUnavailable);
        }
        let result = self.remaining_inner();
        self.failed = result.is_err();
        result
    }
    fn remaining_inner(&mut self) -> Result<u32> {
        self.reaper.check().map_err(|_| Error::ReaperUnavailable)?;
        for child in &mut self.children {
            child.require_running()?;
        }
        let remaining = self.deadline_boottime_ms().saturating_sub(boottime_ms());
        if remaining == 0 {
            return Err(Error::Deadline);
        }
        u32::try_from(remaining).map_err(|_| Error::Deadline)
    }
    pub fn deadline_boottime_ms(&self) -> u64 {
        self.renewals
            .as_ref()
            .map_or(self.evidence.armed.request.deadline_boottime_ms, |r| {
                r.iter()
                    .map(|r| r.command.deadline_boottime_ms)
                    .min()
                    .unwrap()
            })
    }
    pub fn renewal_evidence(&self) -> Option<&[agent_computer_watchdog::renewal::Receipt; 2]> {
        self.renewals.as_ref()
    }

    /// Both original processes must durably acknowledge one identical command.
    /// Any partial/lost response permanently closes this handle to further work.
    pub fn renew(&mut self, command: &agent_computer_watchdog::renewal::Command) -> Result<()> {
        self.remaining_budget_ms()?;
        let result = self.renew_inner(command);
        if result.is_err() {
            self.failed = true;
            self.stdin = [None, None];
        }
        result
    }
    fn renew_inner(&mut self, command: &agent_computer_watchdog::renewal::Command) -> Result<()> {
        let request = &self.evidence.armed.request;
        let old_deadline = self.deadline_boottime_ms();
        let expected_sequence = self
            .renewals
            .as_ref()
            .map_or(1, |r| r[0].command.sequence + 1);
        if command.sequence != expected_sequence
            || request.renewal.is_none()
            || command.request_digest
                != agent_computer_watchdog::renewal::request_digest(request)
                    .map_err(|_| Error::InvalidObservation)?
            || command.deadline_boottime_ms <= old_deadline
        {
            return Err(Error::InvalidObservation);
        }
        let deadline = Instant::now()
            + Duration::from_millis(old_deadline.saturating_sub(boottime_ms()).min(2000));
        let sent_boottime_ms = boottime_ms();
        for stdin in &self.stdin {
            agent_computer_watchdog::write_frame(
                stdin.as_ref().ok_or(Error::WatchdogUnavailable)?,
                command,
            )
            .map_err(|_| Error::WatchdogUnavailable)?;
        }
        let mut receipts = Vec::new();
        for index in 0..2 {
            let receipt: agent_computer_watchdog::renewal::Receipt = read_receipt(
                &mut self.stdout[index],
                self.children[index].process(),
                deadline,
            )?;
            let reference = if index == 0 {
                &self.evidence.armed.journal
            } else {
                &self.evidence.backup_armed.journal
            };
            receipt
                .validate(
                    request,
                    reference.as_ref().ok_or(Error::InvalidObservation)?,
                )
                .map_err(|_| Error::InvalidObservation)?;
            if receipt.command != *command
                || receipt.previous_deadline_boottime_ms != old_deadline
                || receipt.accepted_boottime_ms > boottime_ms()
                || receipt.accepted_boottime_ms < sent_boottime_ms
                || boottime_ms() >= old_deadline
            {
                return Err(Error::Deadline);
            }
            self.children[index].require_running()?;
            receipts.push(receipt);
        }
        let receipts: [_; 2] = receipts.try_into().map_err(|_| Error::InvalidObservation)?;
        self.reaper
            .renewed(receipts.clone())
            .map_err(|_| Error::ReaperUnavailable)?;
        if boottime_ms() >= old_deadline {
            return Err(Error::Deadline);
        }
        self.renewals = Some(receipts);
        self.remaining_budget_ms()?;
        Ok(())
    }
}

/// Reap without signalling the independent timer, including partial setup errors.
#[derive(Debug)]
struct DetachedChild(Option<Child>);
impl DetachedChild {
    fn require_running(&mut self) -> Result<()> {
        use rustix::process::{WaitId, WaitIdOptions, waitid};
        let pid = rustix::process::Pid::from_child(self.process());
        loop {
            match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED
                    | WaitIdOptions::STOPPED
                    | WaitIdOptions::NOHANG
                    | WaitIdOptions::NOWAIT,
            ) {
                Ok(None) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                _ => return Err(Error::WatchdogUnavailable),
            }
        }
    }
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
    termination: crate::termination::ProcessDomain,
) -> Result<ArmedGuard> {
    let [
        (armed, primary, primary_stdout, primary_stdin),
        (backup_armed, backup, backup_stdout, backup_stdin),
    ] = launch_pair(executable, spool, &request, deadline)?;
    let watchdog_pids = [
        primary.0.as_ref().unwrap().id(),
        backup.0.as_ref().unwrap().id(),
    ];
    let reaper = agent_computer_watchdog::admission::Client::connect(
        spool,
        &request,
        [
            armed.journal.clone().ok_or(Error::InvalidObservation)?,
            backup_armed
                .journal
                .clone()
                .ok_or(Error::InvalidObservation)?,
        ],
        armed.cgroup_device,
    )
    .map_err(|_| Error::ReaperUnavailable)?;
    let mut guard = ArmedGuard {
        evidence: Evidence {
            version: 2,
            runtime,
            armed,
            backup_armed,
            watchdog_pids,
            reaper: reaper.receipt().clone(),
            observed_boottime_ms: boottime_ms(),
        },
        children: [primary, backup],
        stdout: [primary_stdout, backup_stdout],
        stdin: [primary_stdin, backup_stdin],
        renewals: None,
        failed: false,
        reaper,
        termination,
    };
    guard.remaining_budget_ms()?;
    Ok(guard)
}

type ArmedProcess = (ArmedReceipt, DetachedChild, ChildStdout, Option<ChildStdin>);

fn launch_pair(
    executable: &File,
    spool: &std::path::Path,
    request: &Request,
    deadline: Instant,
) -> Result<[ArmedProcess; 2]> {
    let mut primary = launch_one(executable, spool, request, deadline)?;
    // Partial setup failure detaches an already armed timer; never disarm it.
    let mut backup = launch_one(executable, spool, request, deadline)?;
    if primary.0.cgroup_device != backup.0.cgroup_device {
        return Err(Error::IdentityMismatch);
    }
    primary.1.require_running()?;
    backup.1.require_running()?;
    Ok([primary, backup])
}

fn launch_one(
    executable: &File,
    spool: &std::path::Path,
    request: &Request,
    deadline: Instant,
) -> Result<ArmedProcess> {
    if Instant::now() >= deadline {
        return Err(Error::Deadline);
    }
    let mut input = tempfile::NamedTempFile::new_in(spool).map_err(|_| Error::Configuration)?;
    let expected = serde_json::to_value(request).map_err(|_| Error::Configuration)?;
    let bytes = serde_json::to_vec(request).map_err(|_| Error::Configuration)?;
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
    // Retain even partial evidence. Only explicit trusted retention may remove it.
    let journal_path = tempfile::Builder::new()
        .prefix("journal-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(spool)
        .map_err(|_| Error::Configuration)?
        .keep();
    // Do not create a child process group here: the CLI calls setsid itself.
    let mut child = DetachedChild(Some(
        command::command(executable, "agent-computer-watchdog")
            .stdin(if request.renewal.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .args([
                std::ffi::OsStr::new("--request"),
                input.path().as_os_str(),
                std::ffi::OsStr::new("--journal"),
                journal_path.as_os_str(),
            ])
            .spawn()
            .map_err(|_| Error::WatchdogUnavailable)?,
    ));
    let stdin = child.process().stdin.take();
    if let Some(stdin) = &stdin {
        rustix::fs::fcntl_setfl(stdin, rustix::fs::OFlags::NONBLOCK)
            .map_err(|_| Error::WatchdogUnavailable)?;
    }
    let mut stdout = child
        .process()
        .stdout
        .take()
        .ok_or(Error::WatchdogUnavailable)?;
    rustix::fs::fcntl_setfl(&stdout, rustix::fs::OFlags::NONBLOCK)
        .map_err(|_| Error::WatchdogUnavailable)?;
    // On any error close this reader, never kill or reset the independent guard.
    let armed: ArmedReceipt = read_receipt(&mut stdout, child.process(), deadline)?;
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
    let reference = armed.journal.as_ref().ok_or(Error::InvalidObservation)?;
    if Some(reference.id.as_str()) != journal_path.file_name().and_then(|v| v.to_str()) {
        return Err(Error::IdentityMismatch);
    }
    let durable = agent_computer_watchdog::journal::Journal::read(spool, reference)
        .map_err(|_| Error::InvalidObservation)?;
    if durable.intent.request != *request
        || durable.intent.watchdog_pid != child.process().id()
        || durable
            .enrollment
            .as_ref()
            .is_none_or(|v| v.cgroup_device != armed.cgroup_device)
    {
        return Err(Error::IdentityMismatch);
    }
    child.require_running()?;
    Ok((armed, child, stdout, stdin))
}

fn read_receipt<T: serde::de::DeserializeOwned>(
    stdout: &mut ChildStdout,
    child: &mut Child,
    deadline: Instant,
) -> Result<T> {
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
