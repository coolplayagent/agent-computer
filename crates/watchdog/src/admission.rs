//! Root-only, bounded local liveness challenge. No workload or writer authority.
use crate::{
    Error, Request, Result, boottime_ms, cgroup,
    journal::{self, Journal, Reference},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{ErrorKind, Read},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixDatagram,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 4096;
pub const PROBE_MS: u64 = 200;
const SOCKET: &str = "reaper.sock";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    version: u8,
    nonce: String,
    request: Request,
    journals: [Reference; 2],
    cgroup_device: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    renewals: Option<[crate::renewal::Receipt; 2]>,
}
impl Probe {
    fn deadline(&self) -> u64 {
        self.renewals
            .as_ref()
            .map_or(self.request.deadline_boottime_ms, |r| {
                r.iter()
                    .map(|r| r.command.deadline_boottime_ms)
                    .min()
                    .unwrap()
            })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u8,
    pub instance: String,
    pub nonce: String,
    pub request: Request,
    pub journals: [Reference; 2],
    pub cgroup_device: u64,
    pub pid: u32,
    pub spool_device: u64,
    pub spool_inode: u64,
    pub observed_boottime_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renewals: Option<[crate::renewal::Receipt; 2]>,
}

#[derive(Debug)]
pub struct Client {
    spool: PathBuf,
    initial: Receipt,
    failed: bool,
    renewals: Option<[crate::renewal::Receipt; 2]>,
}
impl Client {
    pub fn connect(
        spool: &Path,
        request: &Request,
        journals: [Reference; 2],
        cgroup_device: u64,
    ) -> Result<Self> {
        let initial = probe(spool, request, &journals, cgroup_device, None)?;
        Ok(Self {
            spool: spool.into(),
            initial,
            failed: false,
            renewals: None,
        })
    }
    pub fn receipt(&self) -> &Receipt {
        &self.initial
    }
    pub fn check(&mut self) -> Result<()> {
        if self.failed {
            return Err(Error::ReaperUnavailable);
        }
        let result = self.check_inner();
        self.failed = result.is_err();
        result
    }
    /// Called only after both original live guards acknowledge the same grant.
    /// Failure is sticky, including a lost service response or restart.
    pub fn renewed(&mut self, receipts: [crate::renewal::Receipt; 2]) -> Result<()> {
        if self.failed {
            return Err(Error::ReaperUnavailable);
        }
        for (index, receipt) in receipts.iter().enumerate() {
            if receipt
                .validate(&self.initial.request, &self.initial.journals[index])
                .is_err()
                || receipt.command.sequence
                    != self
                        .renewals
                        .as_ref()
                        .map_or(1, |r| r[index].command.sequence + 1)
            {
                self.failed = true;
                return Err(Error::InvalidJournal);
            }
        }
        self.renewals = Some(receipts);
        self.check()
    }
    fn check_inner(&self) -> Result<()> {
        let next = probe(
            &self.spool,
            &self.initial.request,
            &self.initial.journals,
            self.initial.cgroup_device,
            self.renewals.as_ref(),
        )?;
        if next.instance != self.initial.instance
            || next.pid != self.initial.pid
            || next.spool_device != self.initial.spool_device
            || next.spool_inode != self.initial.spool_inode
        {
            return Err(Error::ReaperUnavailable);
        }
        Ok(())
    }
}

fn random() -> Result<String> {
    let mut bytes = [0; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| Error::Setup)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn frame(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|_| Error::InvalidRequest)?;
    if bytes.len() > MAX_FRAME {
        return Err(Error::InvalidRequest);
    }
    Ok(bytes)
}
fn socket_metadata(path: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(path).map_err(|_| Error::ReaperUnavailable)?;
    if !m.file_type().is_socket() || m.uid() != 0 || m.mode() & 0o077 != 0 || m.nlink() != 1 {
        return Err(Error::UntrustedJournal);
    }
    Ok(m)
}
struct ProbeDirectory(PathBuf);
impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("s"));
        let _ = fs::remove_dir(&self.0);
    }
}
fn probe(
    spool: &Path,
    request: &Request,
    journals: &[Reference; 2],
    cgroup_device: u64,
    renewals: Option<&[crate::renewal::Receipt; 2]>,
) -> Result<Receipt> {
    if !rustix::process::geteuid().is_root() {
        return Err(Error::RootRequired);
    }
    let start = boottime_ms();
    let deadline = Instant::now() + Duration::from_millis(PROBE_MS);
    let dir = journal::directory(spool, true)?;
    let meta = dir.metadata().map_err(|_| Error::ReaperUnavailable)?;
    socket_metadata(&spool.join(SOCKET))?;
    let query = Probe {
        version: 1,
        nonce: random()?,
        request: request.clone(),
        journals: journals.clone(),
        cgroup_device,
        renewals: renewals.cloned(),
    };
    let path = spool.join(format!("probe-{}", &query.nonce[..16]));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|_| Error::ReaperUnavailable)?;
    let temporary = ProbeDirectory(path);
    let socket = UnixDatagram::bind(temporary.0.join("s")).map_err(|_| Error::ReaperUnavailable)?;
    fs::set_permissions(temporary.0.join("s"), fs::Permissions::from_mode(0o600))
        .map_err(|_| Error::ReaperUnavailable)?;
    socket
        .set_nonblocking(true)
        .map_err(|_| Error::ReaperUnavailable)?;
    socket
        .connect(spool.join(SOCKET))
        .map_err(|_| Error::ReaperUnavailable)?;
    socket
        .send(&frame(&query)?)
        .map_err(|_| Error::ReaperUnavailable)?;
    loop {
        if Instant::now() >= deadline || boottime_ms() >= query.deadline() {
            return Err(Error::ReaperUnavailable);
        }
        let mut bytes = [0; MAX_FRAME + 1];
        match socket.recv(&mut bytes) {
            Ok(n) if n <= MAX_FRAME => {
                let reply: Result<Receipt> =
                    serde_json::from_slice(&bytes[..n]).map_err(|_| Error::InvalidJournal)?;
                let reply = reply?;
                validate(&query, &reply, start, boottime_ms(), meta.dev(), meta.ino())?;
                return Ok(reply);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            _ => return Err(Error::ReaperUnavailable),
        }
    }
}
fn validate(
    query: &Probe,
    reply: &Receipt,
    start: u64,
    now: u64,
    device: u64,
    inode: u64,
) -> Result<()> {
    if reply.version != 1
        || !hex(&reply.instance)
        || reply.nonce != query.nonce
        || reply.request != query.request
        || reply.journals != query.journals
        || reply.cgroup_device != query.cgroup_device
        || reply.renewals != query.renewals
        || reply.pid == 0
        || reply.spool_device != device
        || reply.spool_inode != inode
        || reply.observed_boottime_ms < start
        || reply.observed_boottime_ms > now
        || now.saturating_sub(start) > PROBE_MS
        || now >= query.deadline()
    {
        return Err(Error::InvalidJournal);
    }
    Ok(())
}

pub(crate) struct Server {
    spool: PathBuf,
    socket: UnixDatagram,
    instance: String,
    device: u64,
    inode: u64,
    socket_inode: u64,
}
impl Server {
    /// The enclosing Reaper holds the exclusive spool lock throughout this life.
    pub(crate) fn bind(spool: &Path) -> Result<Self> {
        let directory = journal::directory(spool, true)?;
        let meta = directory.metadata().map_err(|_| Error::Setup)?;
        let path = spool.join(SOCKET);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                socket_metadata(&path)?;
                fs::remove_file(&path).map_err(|_| Error::Setup)?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(Error::Setup),
        }
        let socket = UnixDatagram::bind(&path).map_err(|_| Error::Setup)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|_| Error::Setup)?;
        socket.set_nonblocking(true).map_err(|_| Error::Setup)?;
        let socket_inode = socket_metadata(&path)?.ino();
        Ok(Self {
            spool: spool.into(),
            socket,
            instance: random()?,
            device: meta.dev(),
            inode: meta.ino(),
            socket_inode,
        })
    }
    pub(crate) fn poll(&self) -> Result<()> {
        for _ in 0..8 {
            let mut bytes = [0; MAX_FRAME + 1];
            let (n, address) = match self.socket.recv_from(&mut bytes) {
                Ok(value) => value,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error::ReaperUnavailable),
            };
            let Some(path) = address.as_pathname() else {
                continue;
            };
            if !self.allowed_reply(path) {
                continue;
            }
            let result = if n > MAX_FRAME {
                Err(Error::InvalidRequest)
            } else {
                self.answer(&bytes[..n])
            };
            // A vanished/full reader cannot block expiry enforcement.
            let _ = self.socket.send_to(&frame(&result)?, path);
        }
        Ok(())
    }
    fn allowed_reply(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        path.file_name().is_some_and(|v| v == "s")
            && parent.parent() == Some(self.spool.as_path())
            && parent
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| {
                    v.starts_with("probe-")
                        && v.len() == 22
                        && v[6..].bytes().all(|b| b.is_ascii_hexdigit())
                })
            && journal::directory(parent, true).is_ok()
            && socket_metadata(path).is_ok()
    }
    fn answer(&self, bytes: &[u8]) -> Result<Receipt> {
        let query: Probe = serde_json::from_slice(bytes).map_err(|_| Error::InvalidRequest)?;
        if query.version != 1
            || !hex(&query.nonce)
            || query.journals[0] == query.journals[1]
            || query.cgroup_device == 0
        {
            return Err(Error::InvalidRequest);
        }
        let boot = cgroup::read_small("/proc/sys/kernel/random/boot_id")?;
        let own = cgroup::read_small("/proc/self/cgroup")?;
        query.request.check_node(
            boot.trim_end(),
            boottime_ms(),
            own.strip_prefix("0::")
                .ok_or(Error::CgroupRequired)?
                .trim_end_matches('\n'),
        )?;
        let mut pids = Vec::new();
        for (index, reference) in query.journals.iter().enumerate() {
            if !reference.id.starts_with("journal-") {
                return Err(Error::InvalidRequest);
            }
            let snapshot = Journal::read(&self.spool, reference)?;
            if snapshot.intent.request != query.request
                || snapshot
                    .enrollment
                    .as_ref()
                    .is_none_or(|v| v.cgroup_device != query.cgroup_device)
                || snapshot.report.is_some()
                || snapshot.recovery.is_some()
                || snapshot.renewal.as_ref() != query.renewals.as_ref().map(|r| &r[index])
            {
                return Err(Error::InvalidJournal);
            }
            pids.push(snapshot.intent.watchdog_pid);
        }
        if pids[0] == pids[1] {
            return Err(Error::InvalidJournal);
        }
        if let Some(receipts) = &query.renewals {
            if receipts[0].command != receipts[1].command {
                return Err(Error::InvalidJournal);
            }
            for (index, receipt) in receipts.iter().enumerate() {
                receipt.validate(&query.request, &query.journals[index])?;
            }
        }
        cgroup::Cgroup::verify(&query.request, query.cgroup_device)?;
        let observed = boottime_ms();
        if observed >= query.deadline() {
            return Err(Error::InvalidRequest);
        }
        Ok(Receipt {
            version: 1,
            instance: self.instance.clone(),
            nonce: query.nonce,
            request: query.request,
            journals: query.journals,
            cgroup_device: query.cgroup_device,
            pid: std::process::id(),
            spool_device: self.device,
            spool_inode: self.inode,
            observed_boottime_ms: observed,
            renewals: query.renewals,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let path = self.spool.join(SOCKET);
        if socket_metadata(&path).is_ok_and(|m| m.ino() == self.socket_inode) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests;
