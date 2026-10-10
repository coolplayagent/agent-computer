//! Root-owned local evidence. Reading a journal never rearms or signals anything.
use crate::{Error, Observation, Report, Request, Result, Trigger};
use rustix::fs::{Mode, OFlags, RenameFlags, ResolveFlags, openat2, renameat_with};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

const MAX_RECORD: u64 = 8192;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub id: String,
    pub device: u64,
    pub inode: u64,
    pub intent_digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub version: u8,
    pub request: Request,
    pub watchdog_pid: u32,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub intent: Intent,
    pub report: Option<Report>,
    pub enrollment: Option<Enrollment>,
    pub recovery: Option<Report>,
    pub renewal: Option<crate::renewal::Receipt>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub version: u8,
    pub journal: Reference,
    pub cgroup_device: u64,
}

pub struct Journal {
    directory: File,
    reference: Reference,
    intent: Intent,
}

impl Journal {
    /// The caller creates a fresh private directory. Existing intent/results are
    /// never replaced. No receipt may be acknowledged before this returns.
    pub fn create(path: &Path, request: &Request) -> Result<Self> {
        root()?;
        let parent = directory(path.parent().ok_or(Error::UntrustedJournal)?, false)?;
        let dir = directory(path, true)?;
        let id = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or(Error::UntrustedJournal)?;
        valid_id(id)?;
        Request::parse(&serde_json::to_vec(request).map_err(|_| Error::InvalidRequest)?)?;
        let intent = Intent {
            version: 1,
            request: request.clone(),
            watchdog_pid: std::process::id(),
        };
        let bytes = serde_json::to_vec(&intent).map_err(|_| Error::InvalidJournal)?;
        publish(&dir, "intent.json", "intent.pending", &bytes)?;
        // Persist the directory entry too, not just the files within it.
        parent.sync_all().map_err(|_| Error::JournalUnavailable)?;
        let meta = dir.metadata().map_err(|_| Error::JournalUnavailable)?;
        Ok(Self {
            reference: Reference {
                id: id.into(),
                device: meta.dev(),
                inode: meta.ino(),
                intent_digest: digest(&bytes),
            },
            directory: dir,
            intent,
        })
    }
    pub fn reference(&self) -> &Reference {
        &self.reference
    }
    pub fn request(&self) -> &Request {
        &self.intent.request
    }

    /// Opens a fixed reference, including after a reboot, for observation only.
    pub fn read(spool: &Path, reference: &Reference) -> Result<Snapshot> {
        let journal = Self::open_reference(spool, reference)?;
        let report = journal.report()?;
        let enrollment = journal.enrollment()?;
        let recovery = journal.recovery()?;
        let renewal = journal.latest_renewal()?;
        Ok(Snapshot {
            intent: journal.intent,
            report,
            enrollment,
            recovery,
            renewal,
        })
    }

    pub(crate) fn duplicate(&self) -> Result<Self> {
        Ok(Self {
            directory: self
                .directory
                .try_clone()
                .map_err(|_| Error::JournalUnavailable)?,
            reference: self.reference.clone(),
            intent: self.intent.clone(),
        })
    }

    /// Single original watchdog writer only. The timer has already accepted
    /// this bounded window. IO may delay acknowledgment but never the timer.
    pub(crate) fn renew(&self, receipt: &crate::renewal::Receipt) -> Result<()> {
        receipt
            .validate(self.request(), self.reference())
            .map_err(|_| Error::InvalidJournal)?;
        let previous = self.latest_renewal()?;
        if receipt.command.sequence != previous.as_ref().map_or(1, |r| r.command.sequence + 1)
            || receipt.previous_deadline_boottime_ms
                != previous
                    .as_ref()
                    .map_or(self.request().deadline_boottime_ms, |r| {
                        r.command.deadline_boottime_ms
                    })
            || self.report()?.is_some()
            || self.recovery()?.is_some()
        {
            return Err(Error::InvalidJournal);
        }
        let bytes = serde_json::to_vec(receipt).map_err(|_| Error::InvalidJournal)?;
        let name = format!("renewal-{:08}.json", receipt.command.sequence);
        let pending = format!("renewal-{:08}.pending", receipt.command.sequence);
        publish(&self.directory, &name, &pending, &bytes)?;
        let head_pending = format!("renewal-head-{:08}.pending", receipt.command.sequence);
        publish_inner(
            &self.directory,
            "renewal.json",
            &head_pending,
            &bytes,
            RenameFlags::empty(),
        )
    }

    /// Read-only expiry evidence. The original intent and each accepted receipt
    /// remain immutable; the atomic head merely selects an existing receipt.
    /// This does not authorize renewals or reconstruct a live execution seal.
    pub fn latest_renewal(&self) -> Result<Option<crate::renewal::Receipt>> {
        let Some(bytes) = read(&self.directory, "renewal.json")? else {
            return Ok(None);
        };
        let receipt: crate::renewal::Receipt =
            serde_json::from_slice(&bytes).map_err(|_| Error::InvalidJournal)?;
        receipt
            .validate(self.request(), self.reference())
            .map_err(|_| Error::InvalidJournal)?;
        let name = format!("renewal-{:08}.json", receipt.command.sequence);
        if read(&self.directory, &name)?.as_ref() != Some(&bytes) {
            return Err(Error::InvalidJournal);
        }
        if receipt.command.sequence > 1 {
            let previous = format!("renewal-{:08}.json", receipt.command.sequence - 1);
            let previous: crate::renewal::Receipt = serde_json::from_slice(
                &read(&self.directory, &previous)?.ok_or(Error::InvalidJournal)?,
            )
            .map_err(|_| Error::InvalidJournal)?;
            previous
                .validate(self.request(), self.reference())
                .map_err(|_| Error::InvalidJournal)?;
            if previous.command.sequence + 1 != receipt.command.sequence
                || previous.command.deadline_boottime_ms != receipt.previous_deadline_boottime_ms
                || previous.accepted_boottime_ms > receipt.accepted_boottime_ms
            {
                return Err(Error::InvalidJournal);
            }
        }
        Ok(Some(receipt))
    }

    fn open_reference(spool: &Path, reference: &Reference) -> Result<Self> {
        root()?;
        valid_id(&reference.id)?;
        let dir = directory(&spool.join(&reference.id), true)?;
        let meta = dir.metadata().map_err(|_| Error::JournalUnavailable)?;
        if meta.dev() != reference.device || meta.ino() != reference.inode {
            return Err(Error::InvalidJournal);
        }
        let bytes = read(&dir, "intent.json")?.ok_or(Error::InvalidJournal)?;
        if digest(&bytes) != reference.intent_digest {
            return Err(Error::InvalidJournal);
        }
        let intent: Intent = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidJournal)?;
        if intent.version != 1 || intent.watchdog_pid == 0 {
            return Err(Error::InvalidJournal);
        }
        Request::parse(&serde_json::to_vec(&intent.request).map_err(|_| Error::InvalidJournal)?)?;
        Ok(Self {
            directory: dir,
            reference: reference.clone(),
            intent,
        })
    }

    pub(crate) fn enroll(&self, cgroup_device: u64) -> Result<()> {
        let enrollment = Enrollment {
            version: 1,
            journal: self.reference.clone(),
            cgroup_device,
        };
        publish(
            &self.directory,
            "enrollment.json",
            "enrollment.pending",
            &serde_json::to_vec(&enrollment).map_err(|_| Error::InvalidJournal)?,
        )
    }

    pub(crate) fn open_enrolled(spool: &Path, id: &str) -> Result<Option<(Self, Enrollment)>> {
        root()?;
        valid_id(id)?;
        let dir = directory(&spool.join(id), true)?;
        let Some(bytes) = read(&dir, "enrollment.json")? else {
            return Ok(None);
        };
        let enrollment: Enrollment =
            serde_json::from_slice(&bytes).map_err(|_| Error::InvalidJournal)?;
        if enrollment.journal.id != id {
            return Err(Error::InvalidJournal);
        }
        let journal = Self::open_reference(spool, &enrollment.journal)?;
        journal.validate_enrollment(&enrollment)?;
        Ok(Some((journal, enrollment)))
    }

    fn validate_enrollment(&self, value: &Enrollment) -> Result<()> {
        if value.version != 1 || value.journal != self.reference || value.cgroup_device == 0 {
            return Err(Error::InvalidJournal);
        }
        Ok(())
    }

    fn enrollment(&self) -> Result<Option<Enrollment>> {
        read(&self.directory, "enrollment.json")?
            .map(|bytes| {
                let value = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidJournal)?;
                self.validate_enrollment(&value)?;
                Ok(value)
            })
            .transpose()
    }

    pub(crate) fn recovery(&self) -> Result<Option<Report>> {
        read(&self.directory, "recovery.json")?
            .map(|bytes| {
                let value = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidJournal)?;
                self.validate_recovery(&value)?;
                Ok(value)
            })
            .transpose()
    }

    fn validate_recovery(&self, report: &Report) -> Result<()> {
        self.validate_common(report)?;
        let enrollment = self.enrollment()?.ok_or(Error::InvalidJournal)?;
        if report.trigger != Trigger::Recovery
            || report.kill_boottime_ms < report.request.deadline_boottime_ms
            || report.cgroup_device != enrollment.cgroup_device
            || report.observation != Observation::EmptyObserved
        {
            return Err(Error::InvalidJournal);
        }
        Ok(())
    }

    pub(crate) fn complete_recovery(&self, report: &Report) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        self.validate_recovery(report)?;
        // An interrupted recovery must not reserve the only temporary name.
        // The final record remains immutable; old partial files are retained.
        let pending = format!(
            "recovery-{}-{}-{}.pending",
            std::process::id(),
            crate::boottime_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        publish(
            &self.directory,
            "recovery.json",
            &pending,
            &serde_json::to_vec(report).map_err(|_| Error::InvalidJournal)?,
        )
    }

    pub(crate) fn complete(&self, report: &Report) -> Result<()> {
        self.validate_report(report)?;
        let bytes = serde_json::to_vec(report).map_err(|_| Error::InvalidJournal)?;
        publish(&self.directory, "report.json", "report.pending", &bytes)
    }

    /// None means unconfirmed, even after the deadline. It never means Running,
    /// successfully stopped, or permission to launch a replacement.
    pub fn report(&self) -> Result<Option<Report>> {
        let Some(bytes) = read(&self.directory, "report.json")? else {
            return Ok(None);
        };
        let report = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidJournal)?;
        self.validate_report(&report)?;
        Ok(Some(report))
    }

    fn validate_report(&self, report: &Report) -> Result<()> {
        if report.trigger == Trigger::Recovery {
            return Err(Error::InvalidJournal);
        }
        self.validate_common(report)
    }

    fn validate_common(&self, report: &Report) -> Result<()> {
        if let Some(renewal) = &report.renewal {
            renewal
                .validate(self.request(), self.reference())
                .map_err(|_| Error::InvalidJournal)?;
            if report.trigger == Trigger::Deadline
                && report.kill_boottime_ms < renewal.command.deadline_boottime_ms
            {
                return Err(Error::InvalidJournal);
            }
        }
        if report.version != 1
            || report.request != self.intent.request
            || report.journal.as_ref() != Some(&self.reference)
            || report.cgroup_device == 0
            || report.kill_boottime_ms < report.armed_boottime_ms
            || report.observed_boottime_ms < report.kill_boottime_ms
            || (report.trigger == Trigger::Deadline
                && report.kill_boottime_ms < report.request.deadline_boottime_ms)
            || ((report.observation == Observation::EmptyObserved) != report.error.is_none())
        {
            return Err(Error::InvalidJournal);
        }
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
fn root() -> Result<()> {
    if rustix::process::geteuid().is_root() {
        Ok(())
    } else {
        Err(Error::RootRequired)
    }
}
fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        Err(Error::UntrustedJournal)
    } else {
        Ok(())
    }
}

pub(crate) fn directory(path: &Path, private: bool) -> Result<File> {
    let parts: Vec<_> = path.components().collect();
    if !path.is_absolute()
        || parts
            .iter()
            .skip(1)
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return Err(Error::UntrustedJournal);
    }
    let mut parent = File::open("/").map_err(|_| Error::JournalUnavailable)?;
    for part in parts.iter().skip(1) {
        parent = File::from(
            openat2(
                &parent,
                part.as_os_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
            )
            .map_err(|_| Error::UntrustedJournal)?,
        );
        let meta = parent.metadata().map_err(|_| Error::JournalUnavailable)?;
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(Error::UntrustedJournal);
        }
    }
    let meta = parent.metadata().map_err(|_| Error::JournalUnavailable)?;
    if private && (parts.len() < 2 || meta.mode() & 0o077 != 0) {
        return Err(Error::UntrustedJournal);
    }
    Ok(parent)
}

fn publish(dir: &File, name: &str, pending: &str, bytes: &[u8]) -> Result<()> {
    publish_inner(dir, name, pending, bytes, RenameFlags::NOREPLACE)
}
fn publish_inner(
    dir: &File,
    name: &str,
    pending: &str,
    bytes: &[u8],
    flags: RenameFlags,
) -> Result<()> {
    if bytes.len() as u64 > MAX_RECORD {
        return Err(Error::InvalidJournal);
    }
    let mut file = File::from(
        openat2(
            dir,
            pending,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|_| Error::JournalUnavailable)?,
    );
    file.write_all(bytes)
        .map_err(|_| Error::JournalUnavailable)?;
    file.sync_all().map_err(|_| Error::JournalUnavailable)?;
    renameat_with(dir, pending, dir, name, flags).map_err(|_| Error::JournalUnavailable)?;
    dir.sync_all().map_err(|_| Error::JournalUnavailable)?;
    Ok(())
}

fn read(dir: &File, name: &str) -> Result<Option<Vec<u8>>> {
    let fd = match openat2(
        dir,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(Error::UntrustedJournal),
    };
    let file = File::from(fd);
    let meta = file.metadata().map_err(|_| Error::JournalUnavailable)?;
    if !meta.is_file()
        || meta.uid() != 0
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > MAX_RECORD
    {
        return Err(Error::UntrustedJournal);
    }
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_RECORD + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::JournalUnavailable)?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(Error::InvalidJournal);
    }
    // A reader may arrive between rename and the writer's directory fsync.
    // Complete the same durability boundary before reporting a recorded value.
    file.sync_all().map_err(|_| Error::JournalUnavailable)?;
    dir.sync_all().map_err(|_| Error::JournalUnavailable)?;
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests;
