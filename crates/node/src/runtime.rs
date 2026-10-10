//! Shared trusted Node/CRI readback, before workload-specific mount checks.
use crate::{Configuration, Error, Result, command, observation};
use agent_computer_kubernetes::PodRuntimeIdentity;
use agent_computer_watchdog::boottime_ms;
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    time::{Duration, Instant},
};

pub(super) struct Readback {
    pub watchdog: File,
    pub deadline: Instant,
    pub container: serde_json::Value,
    pub sandbox: serde_json::Value,
    pub metadata: serde_json::Value,
}
pub(super) fn read(
    config: &Configuration,
    identity: &PodRuntimeIdentity,
    deadline_boottime_ms: u64,
) -> Result<Readback> {
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
    Ok(Readback {
        watchdog,
        deadline,
        container,
        sandbox,
        metadata,
    })
}
