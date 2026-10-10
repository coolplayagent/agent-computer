//! Trusted local node adapter. Tenant input cannot select binaries, paths or IDs.
#![forbid(unsafe_code)]
mod command;
mod guard;
mod observation;
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

/// Qualified local K3s/containerd/runsc adapter. All paths and hashes are private
/// operator configuration, never caller-provided runtime API fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub node: NodeIdentity,
    pub k3s: Executable,
    pub watchdog: Executable,
    pub runtime_socket: PathBuf,
    pub spool: PathBuf,
}

pub struct CandidateIdentity {
    pub data_inode: u64,
    pub volume_path: String,
}

pub fn arm(
    config: &Configuration,
    identity: PodRuntimeIdentity,
    execution: &str,
    command: &serde_json::Value,
    candidate: &CandidateIdentity,
    deadline_boottime_ms: u64,
) -> Result<ArmedGuard> {
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
        || stat.ino() != candidate.data_inode
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
    if observation::process(fields.pid, &fields.sandbox, &fields.parent)? != start {
        return Err(Error::IdentityMismatch);
    }
    let request = Request {
        version: 1,
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
        volume_path: candidate.volume_path.clone(),
        runtime_processes,
    };
    guard::launch(&watchdog, &config.spool, request, runtime, deadline)
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
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node adapter: {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
