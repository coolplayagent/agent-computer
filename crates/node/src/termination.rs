//! Completion authority combines the original kernel identities with a live
//! Candidate IO seal. Serialized metadata cannot recreate any of these handles.
use crate::{ArmedGuard, Error, Result, observation::RuntimeObservation};
use agent_computer_fence::{MountedFence, SealedFence};
use agent_computer_watchdog::{
    Request, boottime_ms,
    termination::{EmptyDomain, PinnedDomain},
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::OwnedFd,
    process::{Pid, PidfdFlags, pidfd_open},
};
use serde::Serialize;

#[derive(Debug)]
pub(super) struct ProcessDomain {
    group: PinnedDomain,
    processes: Vec<OwnedFd>,
}
impl ProcessDomain {
    pub fn pin(request: &Request, device: u64, runtime: &RuntimeObservation) -> Result<Self> {
        if runtime.runtime_processes.is_empty() || runtime.runtime_processes.len() > 32 {
            return Err(Error::ProcessBinding);
        }
        let group = PinnedDomain::pin(request, device).map_err(|_| Error::ProcessBinding)?;
        let membership = format!(
            "0::/{}/cri-containerd-{}.scope\n",
            runtime.cgroup_path, runtime.sandbox_id
        );
        let mut processes = Vec::new();
        for process in &runtime.runtime_processes {
            let pid = i32::try_from(process.pid)
                .ok()
                .and_then(Pid::from_raw)
                .ok_or(Error::ProcessBinding)?;
            // Keep the proc directory pinned across pidfd_open as well: a reused
            // numeric PID (even in the same clock tick) cannot replace that task.
            let directory = std::fs::File::open(format!("/proc/{}", process.pid))
                .map_err(|_| Error::ProcessBinding)?;
            verify_process(&directory, process.start_ticks, &membership)?;
            let fd = pidfd_open(pid, PidfdFlags::empty()).map_err(|_| Error::ProcessBinding)?;
            verify_process(&directory, process.start_ticks, &membership)?;
            if exited(&fd)? {
                return Err(Error::ProcessBinding);
            }
            processes.push(fd);
        }
        Ok(Self { group, processes })
    }
    fn stopped(&self) -> Result<EmptyDomain> {
        let observed = self
            .group
            .observe_empty()
            .map_err(|_| Error::TerminationUnconfirmed)?
            .ok_or(Error::TerminationUnconfirmed)?;
        for process in &self.processes {
            if !exited(process)? {
                return Err(Error::TerminationUnconfirmed);
            }
        }
        Ok(observed)
    }
    fn terminate(&self) -> Result<EmptyDomain> {
        self.group
            .terminate()
            .map_err(|_| Error::TerminationUnconfirmed)?;
        let deadline = boottime_ms().saturating_add(5000);
        loop {
            if let Ok(observed) = self.stopped() {
                return Ok(observed);
            }
            if boottime_ms() >= deadline {
                return Err(Error::TerminationUnconfirmed);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn verify_process(directory: &std::fs::File, start_ticks: u64, membership: &str) -> Result<()> {
    let stat = process_field(directory, "stat")?;
    let group = process_field(directory, "cgroup")?;
    if crate::observation::start_ticks(&stat)? != start_ticks || group != membership {
        return Err(Error::ProcessBinding);
    }
    Ok(())
}
fn process_field(directory: &std::fs::File, name: &str) -> Result<String> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    use std::io::Read;
    let file = std::fs::File::from(
        openat2(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| Error::ProcessBinding)?,
    );
    let mut bytes = Vec::new();
    file.take(8193)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::ProcessBinding)?;
    if bytes.len() > 8192 {
        return Err(Error::ProcessBinding);
    }
    String::from_utf8(bytes).map_err(|_| Error::ProcessBinding)
}
fn exited(fd: &OwnedFd) -> Result<bool> {
    let mut events = [PollFd::new(fd, PollFlags::IN)];
    poll(
        &mut events,
        Some(&Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }),
    )
    .map_err(|_| Error::ProcessBinding)?;
    let flags = events[0].revents();
    if flags.intersects(PollFlags::NVAL | PollFlags::ERR) {
        return Err(Error::ProcessBinding);
    }
    Ok(flags.intersects(PollFlags::IN | PollFlags::HUP))
}

/// This handle retains the pinned domain/process identities and the live IO gate.
/// It is neither cloneable nor deserializable; durable evidence is audit only.
pub struct SealedExecution {
    guard: ArmedGuard,
    io: SealedFence,
    domain: EmptyDomain,
    observed_boottime_ms: u64,
}
#[derive(Serialize)]
pub struct SealEvidence<'a> {
    pub version: u8,
    pub arm: &'a crate::Evidence,
    pub io: &'a agent_computer_fence::Evidence,
    pub domain: EmptyDomain,
    pub observed_boottime_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renewals: Option<&'a [agent_computer_watchdog::renewal::Receipt; 2]>,
}
impl SealedExecution {
    pub fn evidence(&self) -> SealEvidence<'_> {
        SealEvidence {
            version: 1,
            arm: self.guard.evidence(),
            io: self.io.evidence(),
            domain: self.domain,
            observed_boottime_ms: self.observed_boottime_ms,
            renewals: self.guard.renewal_evidence(),
        }
    }
}
impl ArmedGuard {
    /// Read-only observation of the original pinned cgroup and every runtime
    /// pidfd. This allows receiving a buffered report after normal process exit;
    /// it grants no execution/renewal authority and does not replace an IO seal.
    pub fn process_termination_observed(&self) -> bool {
        self.termination.stopped().is_ok()
    }
    /// Stop the admitted workload and join accepted Candidate mutations. The
    /// independent timers remain armed, including after any intermediate error.
    pub fn seal(self, fence: &MountedFence) -> Result<SealedExecution> {
        let expected = serde_json::to_value(&self.evidence().runtime.workspace_mount)
            .map_err(|_| Error::IdentityMismatch)?;
        let actual =
            serde_json::to_value(fence.reference()).map_err(|_| Error::IdentityMismatch)?;
        if expected != actual
            || self.evidence().runtime.identity.node().boot_id != fence.reference().boot_id
        {
            return Err(Error::WorkspaceBinding);
        }
        fence.close();
        self.termination.terminate()?;
        let io = fence.seal().map_err(|_| Error::IoUnconfirmed)?;
        let domain = self.termination.stopped()?;
        let sealed = SealedExecution {
            guard: self,
            io,
            domain,
            observed_boottime_ms: boottime_ms(),
        };
        crate::drain::record(
            &sealed.guard.drain_spool,
            serde_json::to_value(sealed.evidence()).map_err(|_| Error::InvalidObservation)?,
        )?;
        Ok(sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn pinned_proc_identity_rejects_foreign_start_ticks_and_cgroup() {
        let directory = std::fs::File::open("/proc/self").unwrap();
        let ticks =
            crate::observation::start_ticks(&process_field(&directory, "stat").unwrap()).unwrap();
        let membership = process_field(&directory, "cgroup").unwrap();
        verify_process(&directory, ticks, &membership).unwrap();
        assert!(verify_process(&directory, ticks + 1, &membership).is_err());
        assert!(verify_process(&directory, ticks, "0::/foreign\n").is_err());
    }
    #[test]
    fn pidfd_distinguishes_stopped_running_exited_and_reaped_processes() {
        let mut child = Child(
            std::process::Command::new("/usr/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let pid = Pid::from_child(&child.0);
        let fd = pidfd_open(pid, PidfdFlags::empty()).unwrap();
        assert!(!exited(&fd).unwrap());
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(!exited(&fd).unwrap());
        child.0.kill().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !exited(&fd).unwrap() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        child.0.wait().unwrap();
        assert!(exited(&fd).unwrap());
    }
}
