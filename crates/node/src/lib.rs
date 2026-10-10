//! Trusted local node adapter. Tenant input cannot select binaries, paths or IDs.
#![forbid(unsafe_code)]
mod command;
mod drain;
pub use drain::{RecordedSeal, read_recorded_seal};
mod guard;
mod journals;
pub use journals::{JournalObservations, JournalStatus, observe_journals};
mod observation;
mod termination;
use agent_computer_kubernetes::{NodeIdentity, PodRuntimeIdentity};
use agent_computer_watchdog::{Request, boottime_ms};
pub use command::Executable;
pub use guard::{ArmedGuard, Evidence};
use serde::Deserialize;
use std::{
    os::unix::fs::MetadataExt,
    path::PathBuf,
    time::{Duration, Instant},
};
pub use termination::{SealEvidence, SealedExecution};

/// Qualified local K3s/containerd/runsc adapter. All paths and hashes are private
/// operator configuration, never caller-provided runtime API fields.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub node: NodeIdentity,
    pub k3s: Executable,
    pub watchdog: Executable,
    pub runtime_socket: PathBuf,
    pub spool: PathBuf,
}

/// Check local operator bindings before a queue worker consumes any admissions.
/// Reaper availability and the actual runtime identity still require per-start arming.
pub fn validate_local_configuration(config: &Configuration) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        return Err(Error::RootRequired);
    }
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| Error::Configuration)?;
    if config.node.boot_id != boot.trim_end() {
        return Err(Error::IdentityMismatch);
    }
    config.k3s.open()?;
    config.watchdog.open()?;
    let spool = command::trusted_file(&config.spool, true)?;
    if spool.metadata().map_err(|_| Error::Configuration)?.mode() & 0o077 != 0 {
        return Err(Error::Configuration);
    }
    command::trusted_file(
        config.runtime_socket.parent().ok_or(Error::Configuration)?,
        true,
    )?;
    Ok(())
}

pub struct CandidateIdentity {
    pub data_inode: u64,
    pub volume_path: String,
}

pub struct ExecutionLease {
    pub deadline_boottime_ms: u64,
    pub renewal: Option<agent_computer_watchdog::renewal::Policy>,
}
impl From<u64> for ExecutionLease {
    fn from(deadline_boottime_ms: u64) -> Self {
        Self {
            deadline_boottime_ms,
            renewal: None,
        }
    }
}

pub fn arm(
    config: &Configuration,
    identity: PodRuntimeIdentity,
    execution: &str,
    command: &serde_json::Value,
    candidate: &CandidateIdentity,
    deadline_boottime_ms: u64,
) -> Result<ArmedGuard> {
    arm_inner(
        config,
        identity,
        execution,
        command,
        candidate,
        deadline_boottime_ms.into(),
        None,
    )
}
pub fn arm_fenced(
    config: &Configuration,
    identity: PodRuntimeIdentity,
    execution: &str,
    command: &serde_json::Value,
    candidate: &CandidateIdentity,
    lease: impl Into<ExecutionLease>,
    fence: &agent_computer_fence::MountedFence,
) -> Result<ArmedGuard> {
    arm_inner(
        config,
        identity,
        execution,
        command,
        candidate,
        lease.into(),
        Some(fence),
    )
}
fn arm_inner(
    config: &Configuration,
    identity: PodRuntimeIdentity,
    execution: &str,
    command: &serde_json::Value,
    candidate: &CandidateIdentity,
    lease: ExecutionLease,
    fence: Option<&agent_computer_fence::MountedFence>,
) -> Result<ArmedGuard> {
    let deadline_boottime_ms = lease.deadline_boottime_ms;
    if !rustix::process::geteuid().is_root() {
        return Err(Error::RootRequired);
    }
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| Error::Configuration)?;
    if &config.node != identity.node() || config.node.boot_id != boot.trim_end() {
        return Err(Error::IdentityMismatch);
    }
    let budget = deadline_boottime_ms.saturating_sub(boottime_ms());
    if budget == 0 || budget > 30000 {
        return Err(Error::Deadline);
    }
    let deadline = Instant::now() + Duration::from_millis(budget.min(10_000));
    let k3s = config.k3s.open()?;
    let watchdog = config.watchdog.open()?;
    let spool = command::trusted_file(&config.spool, true)?;
    if spool.metadata().map_err(|_| Error::Configuration)?.mode() & 0o077 != 0 {
        return Err(Error::Configuration);
    }
    let socket = config.runtime_socket.to_str().ok_or(Error::Configuration)?;
    let parent = config.runtime_socket.parent().ok_or(Error::Configuration)?;
    command::trusted_file(parent, true)?;
    let metadata =
        std::fs::symlink_metadata(&config.runtime_socket).map_err(|_| Error::Configuration)?;
    use std::os::unix::fs::FileTypeExt;
    if !metadata.file_type().is_socket()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o007 != 0
    {
        return Err(Error::Configuration);
    }
    let endpoint = format!("unix://{socket}");
    let container = command::read_json(
        &k3s,
        "k3s",
        &[
            "crictl",
            "--runtime-endpoint",
            &endpoint,
            "--timeout",
            "3s",
            "inspect",
            identity.container_id(),
        ],
        deadline,
    )?;
    let sid = observation::sandbox_id(&container)?;
    let sandbox = command::read_json(
        &k3s,
        "k3s",
        &[
            "crictl",
            "--runtime-endpoint",
            &endpoint,
            "--timeout",
            "3s",
            "inspectp",
            sid,
        ],
        deadline,
    )?;
    let metadata = command::read_json(
        &k3s,
        "k3s",
        &[
            "ctr",
            "--address",
            socket,
            "--namespace",
            "k8s.io",
            "containers",
            "info",
            identity.container_id(),
        ],
        deadline,
    )?;
    if candidate.volume_path.is_empty()
        || candidate.volume_path.len() > 128
        || !candidate
            .volume_path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(Error::Configuration);
    }
    let fields = observation::fields(
        &identity,
        command,
        &candidate.volume_path,
        &container,
        &sandbox,
        &metadata,
        fence.is_some(),
    )
    .map_err(|_| Error::RuntimeBinding)?;
    let start = observation::process(fields.pid, &fields.sandbox, &fields.parent)
        .map_err(|_| Error::ProcessBinding)?;
    let runtime_processes =
        observation::runtime_processes(&fields, identity.container_id(), deadline)
            .map_err(|_| Error::ProcessBinding)?;
    let path = PathBuf::from("/sys/fs/cgroup").join(&fields.parent);
    let group = command::trusted_file(&path, true)?;
    let inode = group
        .metadata()
        .map_err(|_| Error::RuntimeUnavailable)?
        .ino();
    let mount = std::fs::File::from(
        rustix::fs::openat2(
            rustix::fs::CWD,
            &fields.workspace,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| Error::IdentityMismatch)?,
    );
    let stat = mount.metadata().map_err(|_| Error::IdentityMismatch)?;
    if candidate.data_inode == 0
        || (fence.is_none() && stat.ino() != candidate.data_inode)
        || stat.uid() != 1000
        || stat.gid() != 1000
        || stat.mode() & 0o777 != 0o700
        || rustix::fs::fstatfs(&mount)
            .map_err(|_| Error::IdentityMismatch)?
            .f_type
            != 0x6573_5546
    {
        return Err(Error::WorkspaceBinding);
    }
    if let Some(fence) = fence {
        let reference = fence.reference();
        if reference.boot_id != config.node.boot_id
            || reference.prepared.data_inode != candidate.data_inode
        {
            return Err(Error::WorkspaceBinding);
        }
        fence.verify().map_err(|_| Error::WorkspaceBinding)?;
        reference
            .verify_file(&mount)
            .map_err(|_| Error::WorkspaceBinding)?;
    }
    if observation::process(fields.pid, &fields.sandbox, &fields.parent)? != start {
        return Err(Error::IdentityMismatch);
    }
    let request = Request {
        version: if lease.renewal.is_some() { 2 } else { 1 },
        renewal: lease.renewal,
        execution_id: execution.into(),
        boot_id: config.node.boot_id.clone(),
        cgroup_path: fields.parent.clone(),
        cgroup_inode: inode,
        deadline_boottime_ms,
    };
    let runtime = observation::RuntimeObservation {
        identity,
        sandbox_id: fields.sandbox,
        sentry_pid: fields.pid,
        sentry_start_ticks: start,
        cgroup_path: fields.parent,
        cgroup_inode: inode,
        workspace_inode: candidate.data_inode,
        workspace_mount: fence.map(|f| f.reference().clone()),
        volume_path: candidate.volume_path.clone(),
        runtime_processes,
    };
    let termination = termination::ProcessDomain::pin(
        &request,
        group.metadata().map_err(|_| Error::ProcessBinding)?.dev(),
        &runtime,
    )?;
    guard::launch(
        &watchdog,
        &config.spool,
        request,
        runtime,
        deadline,
        termination,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum Error {
    Configuration,
    RootRequired,
    IdentityMismatch,
    RuntimeBinding,
    ProcessBinding,
    WorkspaceBinding,
    RuntimeUnavailable,
    InvalidObservation,
    ResponseLimit,
    Deadline,
    WatchdogUnavailable,
    ReaperUnavailable,
    TerminationUnconfirmed,
    IoUnconfirmed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node adapter: {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
