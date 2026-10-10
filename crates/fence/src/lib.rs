//! Trusted, revocable Candidate filesystem. All tenant IO must traverse this mount.
//! This component supplies an IO barrier, not process termination or database admission.
#![forbid(unsafe_code)]
mod backend;
mod filesystem;
use agent_computer_storage::{Prepared, files::CandidateDirectory};
use backend::{Result as FsResult, State};
use fuser::{BackgroundSession, Config, Errno, MountOption, SessionACL};
use serde::Serialize;
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Gate {
    closed: AtomicBool,
    active_mutation: AtomicBool,
    state: Mutex<State>,
    prepared: Prepared,
    instance: String,
}
impl Gate {
    fn access<T>(
        &self,
        uid: u32,
        mutating: bool,
        f: impl FnOnce(&mut State) -> FsResult<T>,
    ) -> FsResult<T> {
        let mut state = self.state.lock().map_err(|_| Errno::EIO)?;
        if uid != 0 && uid != state.uid {
            return Err(Errno::EACCES);
        }
        if mutating && self.closed.load(Ordering::SeqCst) {
            return Err(Errno::EROFS);
        }
        if mutating && state.failed {
            return Err(Errno::EIO);
        }
        self.active_mutation.store(mutating, Ordering::SeqCst);
        struct Active<'a>(&'a AtomicBool);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _active = Active(&self.active_mutation);
        let result = f(&mut state);
        if mutating && result.is_ok() {
            state.mutations = state.mutations.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        }
        result
    }
    fn seal(self: &Arc<Self>) -> std::io::Result<SealedFence> {
        // First deny all queued/future mutations, then join the operation already
        // holding the lock. A stalled backend cannot yield a positive barrier.
        self.closed.store(true, Ordering::SeqCst);
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("IO barrier poisoned"))?;
        if state.failed {
            return Err(std::io::Error::other("persistence is unconfirmed"));
        }
        state
            .sync_node(1)
            .map_err(|_| std::io::Error::other("Candidate directory sync failed"))?;
        let evidence = Evidence {
            version: 1,
            instance: self.instance.clone(),
            prepared: self.prepared.clone(),
            accepted_mutating_requests: state.mutations,
        };
        Ok(SealedFence {
            gate: self.clone(),
            evidence,
        })
    }
}
/// Audit metadata. Deserializing or storing JSON never recreates a live barrier.
#[derive(Clone, Debug, Serialize)]
pub struct Evidence {
    pub version: u32,
    pub instance: String,
    pub prepared: Prepared,
    pub accepted_mutating_requests: u64,
}
/// Only a successful barrier constructs this handle. It is not a process fence,
/// and does not cover pre-existing direct CSI mounts or other backing-file writers.
pub struct SealedFence {
    gate: Arc<Gate>,
    evidence: Evidence,
}
impl SealedFence {
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }
    pub fn is_sealed(&self) -> bool {
        self.gate.closed.load(Ordering::SeqCst)
            && self
                .gate
                .state
                .lock()
                .is_ok_and(|s| !s.failed && s.mutations == self.evidence.accepted_mutating_requests)
    }
}
pub struct MountedFence {
    gate: Arc<Gate>,
    session: BackgroundSession,
    _mountpoint: File,
}
impl MountedFence {
    pub fn seal(&self) -> std::io::Result<SealedFence> {
        self.gate.seal()
    }
    pub fn instance(&self) -> &str {
        &self.gate.instance
    }
    /// Diagnostic only: neither flag constitutes a successful drain receipt.
    pub fn status(&self) -> Status {
        Status {
            closing: self.gate.closed.load(Ordering::SeqCst),
            active_mutation: self.gate.active_mutation.load(Ordering::SeqCst),
        }
    }
    pub fn unmount(self) -> std::io::Result<()> {
        self.gate.closed.store(true, Ordering::SeqCst);
        self.session.umount_and_join()
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Status {
    pub closing: bool,
    pub active_mutation: bool,
}
impl Drop for Gate {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Operator-only local mount. The original Candidate must have exclusive writer
/// admission, and workloads must receive only this mount, never its backing path.
/// No passthrough FD, writeback cache, shared mmap, devices or tenant mount options exist.
pub fn mount(candidate: CandidateDirectory, mountpoint: &Path) -> std::io::Result<MountedFence> {
    let (root, prepared, uid, gid) = candidate.into_parts();
    mount_inner(root, prepared, uid, gid, mountpoint)
}
fn mount_inner(
    root: File,
    prepared: Prepared,
    uid: u32,
    gid: u32,
    mountpoint: &Path,
) -> std::io::Result<MountedFence> {
    if !rustix::process::geteuid().is_root() {
        return Err(std::io::Error::other("trusted root adapter required"));
    }
    let target = trusted_mountpoint(mountpoint)?;
    let mut nonce = [0; 32];
    getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
    let instance = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let gate = Arc::new(Gate {
        closed: AtomicBool::new(false),
        active_mutation: AtomicBool::new(false),
        state: Mutex::new(State::new(root, uid, gid)?),
        prepared,
        instance,
    });
    let mut config = Config::default();
    config.acl = SessionACL::All;
    config.mount_options = vec![
        MountOption::FSName(format!("agent-computer-{}", gate.instance)),
        MountOption::Subtype("agent-computer".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::RW,
    ];
    config.n_threads = Some(2);
    let session = fuser::spawn_mount(filesystem::Filesystem(gate.clone()), mountpoint, &config)?;
    Ok(MountedFence {
        gate,
        session,
        _mountpoint: target,
    })
}
fn trusted_mountpoint(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    use std::path::Component;
    if !path.is_absolute() || path.components().count() < 2 {
        return Err(std::io::Error::other(
            "absolute private mountpoint required",
        ));
    }
    let mut file = File::open("/")?;
    for c in path.components().skip(1) {
        let Component::Normal(name) = c else {
            return Err(std::io::Error::other("invalid mountpoint"));
        };
        file = File::from(openat2(
            &file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
        )?);
        let m = file.metadata()?;
        if m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(std::io::Error::other("untrusted mountpoint ancestor"));
        }
    }
    for entry in rustix::fs::Dir::read_from(&file)? {
        let entry = entry?;
        if entry.file_name().to_bytes() != b"." && entry.file_name().to_bytes() != b".." {
            return Err(std::io::Error::other("mountpoint must be empty"));
        }
    }
    Ok(file)
}
#[cfg(test)]
mod tests;
