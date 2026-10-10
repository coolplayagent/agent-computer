//! Durable receipt of an already completed live process/IO seal. Recovery can
//! publish that historical fact, but cannot arm, renew or reconstruct a guard.
use crate::{Error, Result, command};
use rustix::fs::{self, Mode, OFlags, RenameFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

const MAX_BYTES: usize = 65_536;
#[cfg(test)]
mod tests;

/// Created only by a private node-spool read, never by caller JSON or SQL rows.
pub struct RecordedSeal(Value);
impl RecordedSeal {
    pub fn evidence(&self) -> &Value {
        &self.0
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    digest: String,
    seal: Value,
}

fn hash(value: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|_| Error::InvalidObservation)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn name(arm: &Value) -> Result<String> {
    Ok(format!("drain-{}.json", hash(arm)?))
}

pub(crate) fn open(path: &Path) -> Result<File> {
    if !rustix::process::geteuid().is_root() {
        return Err(Error::RootRequired);
    }
    let file = command::trusted_file(path, true)?;
    if file.metadata().map_err(|_| Error::Configuration)?.mode() & 0o077 != 0 {
        return Err(Error::Configuration);
    }
    Ok(file)
}

/// Missing and incomplete receipts do not establish drain. Only the original
/// root-owned spool and exact registered arm identify a historical completion.
pub fn read_recorded_seal(path: &Path, arm: &Value) -> Result<Option<RecordedSeal>> {
    read(&open(path)?, arm)
}

fn read(directory: &File, arm: &Value) -> Result<Option<RecordedSeal>> {
    let file = match fs::openat(
        directory,
        name(arm)?,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(Error::InvalidObservation),
    };
    let meta = file.metadata().map_err(|_| Error::InvalidObservation)?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid()
            != directory
                .metadata()
                .map_err(|_| Error::Configuration)?
                .uid()
        || meta.mode() & 0o077 != 0
        || meta.len() > MAX_BYTES as u64
    {
        return Err(Error::InvalidObservation);
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidObservation)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::ResponseLimit);
    }
    let record: Record = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidObservation)?;
    if record.version != 1 || record.seal["arm"] != *arm || record.digest != hash(&record.seal)? {
        return Err(Error::IdentityMismatch);
    }
    validate(&record.seal)?;
    Ok(Some(RecordedSeal(record.seal)))
}

fn validate(seal: &Value) -> Result<()> {
    let mount = &seal["arm"]["runtime"]["workspace_mount"];
    if seal["version"] != 1
        || !matches!(seal["domain"].as_str(), Some("empty" | "removed"))
        || mount.is_null()
        || seal["io"]["version"] != 1
        || seal["io"]["instance"] != mount["instance"]
        || seal["io"]["prepared"] != mount["prepared"]
        || seal["observed_boottime_ms"]
            .as_u64()
            .zip(seal["arm"]["observed_boottime_ms"].as_u64())
            .is_none_or(|(a, b)| a < b)
    {
        return Err(Error::InvalidObservation);
    }
    Ok(())
}

/// Only termination::seal supplies this input in production. Publish while its
/// pinned process descriptors and sealed IO gate are still held. A failed write
/// cannot reopen either gate and cannot produce recovery authority.
pub(crate) fn record(directory: &File, seal: Value) -> Result<()> {
    validate(&seal)?;
    let target = name(&seal["arm"])?;
    let record = Record {
        version: 1,
        digest: hash(&seal)?,
        seal,
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| Error::InvalidObservation)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::ResponseLimit);
    }
    // An independently generated name plus O_EXCL avoids stealing a crashed
    // writer's partial file. Only this call's staging file may be removed.
    let mut random = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|_| Error::Configuration)?;
    let pending = format!("drain-{:x}.pending", Sha256::digest(random));
    let mut file = File::from(
        fs::openat(
            directory,
            &pending,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| Error::Configuration)?,
    );
    let result = (|| {
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Configuration)?;
        match fs::renameat_with(
            directory,
            &pending,
            directory,
            &target,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {
                if read(directory, &record.seal["arm"])?.is_none_or(|r| r.0 != record.seal) {
                    return Err(Error::IdentityMismatch);
                }
            }
            Err(_) => return Err(Error::Configuration),
        }
        directory.sync_all().map_err(|_| Error::Configuration)
    })();
    let _ = fs::unlinkat(directory, pending, fs::AtFlags::empty());
    result
}
