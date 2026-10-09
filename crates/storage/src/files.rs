//! Bounded descriptor-relative file saves. Every mutator must use the exclusive
//! Candidate gateway; this is not fencing for arbitrary processes with a mount.
use crate::{
    Error, MountedVolume, Prepared, Result,
    directory::{Dir, bounded, owner},
    model,
};
use rustix::fs::{self, RenameFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Write, os::unix::fs::MetadataExt, time::Instant};

pub const MAX_FILE_BYTES: usize = 1 << 20;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileVersion {
    pub sha256: String,
    pub size: u64,
    pub executable: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEdit {
    pub path: String,
    /// None requires absence. Some requires exactly these content/mode values.
    pub expected: Option<FileVersion>,
    pub content: Vec<u8>,
    pub executable: bool,
}
impl FileEdit {
    pub fn validate(&self) -> Result<()> {
        if !model::relative(&self.path)
            || self.content.len() > MAX_FILE_BYTES
            || self
                .path
                .split('/')
                .any(|p| p.starts_with(".agent-computer-write-"))
            || self
                .expected
                .as_ref()
                .is_some_and(|v| !model::is_digest(&v.sha256) || v.size > MAX_FILE_BYTES as u64)
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
    pub fn digest(&self, prepared: &Prepared) -> Result<String> {
        self.validate()?;
        model::digest("agent-computer/candidate-file-edit-v1", &(prepared, self))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileEditState {
    Applied,
    Conflict,
    Expired,
    Unknown,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEditReport {
    pub state: FileEditState,
    pub version: Option<FileVersion>,
    pub drain_confirmed: bool,
}
/// Constructed only after this adapter's file handles have closed. Unknown IO
/// never supplies drain evidence, even though the local function has returned.
pub struct ClosedFileEdit {
    digest: String,
    report: FileEditReport,
}
impl ClosedFileEdit {
    pub fn input_digest(&self) -> &str {
        &self.digest
    }
    pub fn report(&self) -> &FileEditReport {
        &self.report
    }
}

fn snapshot(dir: &Dir, path: &str) -> Result<Option<FileVersion>> {
    let Some(file) = dir.try_file(path)? else {
        return Ok(None);
    };
    let stat = file.metadata()?;
    if stat.len() > MAX_FILE_BYTES as u64 {
        return Err(Error::InvalidRequest);
    }
    let bytes = bounded(file, MAX_FILE_BYTES as u64)?;
    if bytes.len() as u64 != stat.len() {
        return Err(Error::InputMismatch);
    }
    Ok(Some(FileVersion {
        sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
        size: bytes.len() as u64,
        executable: stat.mode() & 0o111 != 0,
    }))
}

impl MountedVolume {
    fn prepared_data(&self, prepared: &Prepared) -> Result<Dir> {
        if prepared.version != 1
            || prepared.filesystem_uuid != self.filesystem_uuid
            || prepared.volume_uid != self.volume_uid
            || !model::relative(&prepared.path_ref)
            || !prepared.path_ref.ends_with("/data")
        {
            return Err(Error::IdentityConflict);
        }
        let parent = self.root.open(
            prepared
                .path_ref
                .strip_suffix("/data")
                .ok_or(Error::IdentityConflict)?,
        )?;
        parent.private()?;
        let stored: Prepared =
            serde_json::from_slice(&bounded(parent.file("receipt.json")?, 8192)?)
                .map_err(|_| Error::IdentityConflict)?;
        let data = parent.open("data")?;
        let stat = fs::fstat(&data.0)?;
        if stored != *prepared
            || data.inode()? != prepared.data_inode
            || stat.st_uid != self.uid
            || stat.st_gid != self.gid
            || stat.st_mode & 0o777 != 0o700
        {
            return Err(Error::IdentityConflict);
        }
        Ok(data)
    }
    /// No task, child process or writable descriptor escapes this call. The
    /// caller must consume a durable dispatch permit first, and must not retry IO
    /// after an unknown result. Slow FUSE calls may outlast the local deadline.
    pub fn edit_file(
        &self,
        prepared: &Prepared,
        edit: &FileEdit,
        deadline: Instant,
    ) -> Result<ClosedFileEdit> {
        let digest = edit.digest(prepared)?;
        let report = self
            .edit_inner(prepared, edit, deadline)
            .unwrap_or(FileEditReport {
                state: FileEditState::Unknown,
                version: None,
                drain_confirmed: false,
            });
        Ok(ClosedFileEdit { digest, report })
    }
    fn edit_inner(
        &self,
        prepared: &Prepared,
        edit: &FileEdit,
        deadline: Instant,
    ) -> Result<FileEditReport> {
        let data = self.prepared_data(prepared)?;
        let (parent, name) = match edit.path.rsplit_once('/') {
            Some((parent, name)) => (data.open(parent)?, name),
            None => (data, edit.path.as_str()),
        };
        let before = snapshot(&parent, name)?;
        if before != edit.expected {
            return Ok(FileEditReport {
                state: FileEditState::Conflict,
                version: before,
                drain_confirmed: true,
            });
        }
        if Instant::now() >= deadline {
            return Ok(FileEditReport {
                state: FileEditState::Expired,
                version: before,
                drain_confirmed: true,
            });
        }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| Error::Io)?;
        let temporary = format!(
            ".agent-computer-write-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        // From this first mutation, every error is uncertain. Retain staging for
        // diagnosis rather than performing cleanup after a lost acknowledgement.
        let mut file = parent.create(&temporary)?;
        for chunk in edit.content.chunks(65_536) {
            if Instant::now() >= deadline {
                return Err(Error::Io);
            }
            file.write_all(chunk)?;
        }
        owner(
            &file,
            self.uid,
            self.gid,
            if edit.executable { 0o700 } else { 0o600 },
        )?;
        file.sync_all()?;
        drop(file);
        if Instant::now() >= deadline || snapshot(&parent, name)? != before {
            return Err(Error::Io);
        }
        // The exclusive gateway owns all Candidate writers. NOREPLACE adds a
        // filesystem-level absence check; replacement requires the same gateway
        // serialization and does not claim to exclude an unmanaged writer.
        fs::renameat_with(
            &parent.0,
            &temporary,
            &parent.0,
            name,
            if before.is_none() {
                RenameFlags::NOREPLACE
            } else {
                RenameFlags::empty()
            },
        )?;
        parent.sync()?;
        let expected = FileVersion {
            sha256: format!("sha256:{:x}", Sha256::digest(&edit.content)),
            size: edit.content.len() as u64,
            executable: edit.executable,
        };
        if snapshot(&parent, name)? != Some(expected.clone()) {
            return Err(Error::InputMismatch);
        }
        Ok(FileEditReport {
            state: FileEditState::Applied,
            version: Some(expected),
            drain_confirmed: true,
        })
    }
}

#[cfg(test)]
mod tests;
