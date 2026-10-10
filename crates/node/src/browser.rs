//! Browser-only process admission. No Candidate mount or IO seal is fabricated.
use crate::{
    ArmedGuard, Configuration, Error, Evidence, ExecutionLease, Result, command, guard,
    observation, runtime, termination,
};
use agent_computer_browser::Binding;
use agent_computer_kubernetes::{BrowserRuntimeIdentity, PodRuntimeIdentity};
use agent_computer_watchdog::{Request, boottime_ms, termination::EmptyDomain};
use serde::Serialize;
use std::{fs::File, os::unix::fs::MetadataExt, path::PathBuf};

const TMPFS_MAGIC: u64 = 0x0102_1994;

#[derive(Debug, Serialize)]
pub struct PrivateProfile {
    pub source: String,
    pub device: u64,
    pub inode: u64,
    pub filesystem: u64,
}

#[derive(Debug, Serialize)]
pub struct BrowserRuntimeObservation {
    kind: &'static str,
    pub identity: PodRuntimeIdentity,
    pub binding: Binding,
    pub sandbox_id: String,
    pub sentry_pid: u32,
    pub sentry_start_ticks: u64,
    pub cgroup_path: String,
    pub cgroup_inode: u64,
    pub profile: PrivateProfile,
    pub runtime_processes: Vec<observation::ProcessIdentity>,
    #[serde(skip)]
    profile_directory: File,
}
pub type BrowserGuard = ArmedGuard<BrowserRuntimeObservation>;

/// BrowserRuntimeIdentity can only be constructed by the Kubernetes adapter
/// after the exact approved Pod, successful init and live Node readback.
pub fn arm_browser(
    config: &Configuration,
    identity: BrowserRuntimeIdentity,
    lease: impl Into<ExecutionLease>,
) -> Result<BrowserGuard> {
    let lease = lease.into();
    let (identity, binding, expected_command) = identity.into_parts();
    binding.validate().map_err(|_| Error::IdentityMismatch)?;
    let readback = runtime::read(config, &identity, lease.deadline_boottime_ms)?;
    let fields = observation::browser_fields(
        &identity,
        &expected_command,
        &readback.container,
        &readback.sandbox,
        &readback.metadata,
    )
    .map_err(|_| Error::RuntimeBinding)?;
    let start = observation::process(fields.pid, &fields.sandbox, &fields.parent)?;
    let runtime_processes =
        observation::runtime_processes(&fields, identity.container_id(), readback.deadline)?;
    let group = command::trusted_file(&PathBuf::from("/sys/fs/cgroup").join(&fields.parent), true)?;
    let stat = group.metadata().map_err(|_| Error::ProcessBinding)?;
    let profile_directory = File::from(
        rustix::fs::openat2(
            rustix::fs::CWD,
            &fields.mount_source,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| Error::ProfileBinding)?,
    );
    let profile = profile(&profile_directory, fields.mount_source)?;
    if observation::process(fields.pid, &fields.sandbox, &fields.parent)? != start {
        return Err(Error::ProcessBinding);
    }
    let request = Request {
        version: if lease.renewal.is_some() { 2 } else { 1 },
        renewal: lease.renewal,
        // The watchdog's legacy field names one opaque workload, not a Candidate
        // writer. The typed Browser observation binds it to the App instance.
        execution_id: binding.scope.app_instance_id.clone(),
        boot_id: config.node.boot_id.clone(),
        cgroup_path: fields.parent.clone(),
        cgroup_inode: stat.ino(),
        deadline_boottime_ms: lease.deadline_boottime_ms,
    };
    let termination =
        termination::ProcessDomain::pin(&request, stat.dev(), &runtime_processes, &fields.sandbox)?;
    let runtime = BrowserRuntimeObservation {
        kind: "browser",
        identity,
        binding,
        sandbox_id: fields.sandbox,
        sentry_pid: fields.pid,
        sentry_start_ticks: start,
        cgroup_path: fields.parent,
        cgroup_inode: stat.ino(),
        profile,
        runtime_processes,
        profile_directory,
    };
    guard::launch(
        &readback.watchdog,
        &config.spool,
        request,
        runtime,
        readback.deadline,
        termination,
    )
}

fn profile(file: &File, source: String) -> Result<PrivateProfile> {
    let stat = file.metadata().map_err(|_| Error::ProfileBinding)?;
    let fs = rustix::fs::fstatfs(file).map_err(|_| Error::ProfileBinding)?;
    if !stat.is_dir()
        || stat.ino() == 0
        || stat.uid() != 1000
        || stat.gid() != 1000
        || stat.mode() & 0o777 != 0o700
        || fs.f_type as u64 != TMPFS_MAGIC
    {
        return Err(Error::ProfileBinding);
    }
    Ok(PrivateProfile {
        source,
        device: stat.dev(),
        inode: stat.ino(),
        filesystem: TMPFS_MAGIC,
    })
}

/// Keeps the original cgroup, pidfds and private profile directory pinned. This
/// is process termination evidence, not an encrypted checkpoint or a GUI grant.
pub struct StoppedBrowser {
    guard: BrowserGuard,
    domain: EmptyDomain,
    observed_boottime_ms: u64,
}
#[derive(Serialize)]
pub struct BrowserStopEvidence<'a> {
    pub version: u8,
    pub kind: &'static str,
    pub arm: &'a Evidence<BrowserRuntimeObservation>,
    pub domain: EmptyDomain,
    pub observed_boottime_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renewals: Option<&'a [agent_computer_watchdog::renewal::Receipt; 2]>,
}
impl StoppedBrowser {
    pub fn evidence(&self) -> BrowserStopEvidence<'_> {
        BrowserStopEvidence {
            version: 1,
            kind: "browser_process_stop",
            arm: self.guard.evidence(),
            domain: self.domain,
            observed_boottime_ms: self.observed_boottime_ms,
            renewals: self.guard.renewal_evidence(),
        }
    }
}
impl BrowserGuard {
    pub fn stop_browser(self) -> Result<StoppedBrowser> {
        self.termination.terminate()?;
        let runtime = &self.evidence().runtime;
        let actual = profile(&runtime.profile_directory, runtime.profile.source.clone())?;
        if actual.inode != runtime.profile.inode || actual.device != runtime.profile.device {
            return Err(Error::ProfileBinding);
        }
        let domain = self.termination.stopped()?;
        let stopped = StoppedBrowser {
            guard: self,
            domain,
            observed_boottime_ms: boottime_ms(),
        };
        crate::drain::record_browser(
            &stopped.guard.drain_spool,
            serde_json::to_value(stopped.evidence()).map_err(|_| Error::InvalidObservation)?,
        )?;
        Ok(stopped)
    }
}
