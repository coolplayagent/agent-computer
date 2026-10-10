//! Descriptor-relative capture after the caller has permanently sealed writers.
use crate::{Entry, Error, Manifest, MountedVolume, Prepared, Result, directory::Dir, model};
use rustix::fs::{self, FileType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, os::unix::fs::MetadataExt};

pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub sha256: String,
    pub size: u64,
}
pub trait CaptureSink {
    /// Persist this bounded chunk before returning; capture does not retain bytes.
    fn record(&mut self, chunk: &Chunk, bytes: &[u8]) -> Result<()>;
}
/// Only the storage traversal constructs this receipt. It proves capture, not
/// admission/fencing; those must be established before calling capture().
pub struct CapturedTree {
    prepared: Prepared,
    manifest: Manifest,
    chunks: BTreeMap<String, Vec<Chunk>>,
}
impl CapturedTree {
    pub fn prepared(&self) -> &Prepared {
        &self.prepared
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn chunks(&self) -> &BTreeMap<String, Vec<Chunk>> {
        &self.chunks
    }
}
fn names(dir: &Dir) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for item in fs::Dir::read_from(&dir.0)? {
        let item = item?;
        let name = item
            .file_name()
            .to_str()
            .map_err(|_| Error::InvalidFilesystemObject)?;
        if name == "." || name == ".." {
            continue;
        }
        if !model::relative(name)
            || name.starts_with(".agent-computer-write-")
            || names.len() >= 10_000
        {
            return Err(Error::InvalidFilesystemObject);
        }
        names.push(name.to_owned());
    }
    names.sort();
    Ok(names)
}
fn walk(
    dir: &Dir,
    prefix: &str,
    tree: &mut CapturedTree,
    sink: &mut impl CaptureSink,
    total: &mut u64,
) -> Result<()> {
    let before = fs::fstat(&dir.0)?;
    let entries = names(dir)?;
    for name in &entries {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if !model::relative(&path) || tree.manifest.entries.len() >= 10_000 {
            return Err(Error::InvalidRequest);
        }
        let stat = fs::statat(&dir.0, name.as_str(), fs::AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let child = dir.open(name)?;
                if child.inode()? != stat.st_ino {
                    return Err(Error::IdentityConflict);
                }
                tree.manifest.entries.insert(path.clone(), Entry::Directory);
                walk(&child, &path, tree, sink, total)?;
            }
            FileType::RegularFile => {
                let mut file = dir.file(name)?;
                let meta = file.metadata()?;
                if meta.ino() != stat.st_ino
                    || meta.dev() != stat.st_dev
                    || meta.nlink() != 1
                    || meta.mode() & 0o7000 != 0
                {
                    return Err(Error::InvalidFilesystemObject);
                }
                *total = total.checked_add(meta.len()).ok_or(Error::InvalidRequest)?;
                if *total > tree.prepared.quota_bytes {
                    return Err(Error::InvalidRequest);
                }
                file.sync_all()?;
                let mut remaining = meta.len();
                let mut hash = Sha256::new();
                let mut chunks = Vec::new();
                while remaining > 0 {
                    let mut bytes = vec![0; remaining.min(CHUNK_BYTES as u64) as usize];
                    file.read_exact(&mut bytes)?;
                    hash.update(&bytes);
                    let chunk = Chunk {
                        sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
                        size: bytes.len() as u64,
                    };
                    sink.record(&chunk, &bytes)?;
                    chunks.push(chunk);
                    remaining -= bytes.len() as u64;
                }
                let mut extra = [0];
                if file.read(&mut extra)? != 0 {
                    return Err(Error::InputMismatch);
                }
                let after = file.metadata()?;
                if (
                    meta.ino(),
                    meta.dev(),
                    meta.len(),
                    meta.mode(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                    meta.ctime(),
                    meta.ctime_nsec(),
                    meta.nlink(),
                ) != (
                    after.ino(),
                    after.dev(),
                    after.len(),
                    after.mode(),
                    after.mtime(),
                    after.mtime_nsec(),
                    after.ctime(),
                    after.ctime_nsec(),
                    after.nlink(),
                ) {
                    return Err(Error::InputMismatch);
                }
                tree.manifest.entries.insert(
                    path.clone(),
                    Entry::File {
                        sha256: format!("sha256:{:x}", hash.finalize()),
                        size: meta.len(),
                        executable: meta.mode() & 0o111 != 0,
                    },
                );
                tree.chunks.insert(path, chunks);
            }
            _ => return Err(Error::InvalidFilesystemObject),
        }
    }
    dir.sync()?;
    let after = fs::fstat(&dir.0)?;
    if names(dir)? != entries
        || (
            before.st_ino,
            before.st_mtime,
            before.st_mtime_nsec,
            before.st_ctime,
            before.st_ctime_nsec,
        ) != (
            after.st_ino,
            after.st_mtime,
            after.st_mtime_nsec,
            after.st_ctime,
            after.st_ctime_nsec,
        )
    {
        return Err(Error::InputMismatch);
    }
    Ok(())
}
impl MountedVolume {
    pub fn capture(
        &self,
        prepared: &Prepared,
        sink: &mut impl CaptureSink,
    ) -> Result<CapturedTree> {
        let data = self.prepared_data(prepared)?;
        let mut tree = CapturedTree {
            prepared: prepared.clone(),
            manifest: Manifest::default(),
            chunks: BTreeMap::new(),
        };
        walk(&data, "", &mut tree, sink, &mut 0)?;
        tree.manifest.validate(prepared.quota_bytes)?;
        if self.prepared_data(prepared)?.inode()? != data.inode()? {
            return Err(Error::IdentityConflict);
        }
        Ok(tree)
    }
}
