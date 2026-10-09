use crate::{
    Entry, Error, PrepareRequest, Prepared, Result,
    directory::{Dir, bounded, owner},
    model,
    quota::Quota,
};
use rustix::fs::{self, RenameFlags};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

/// Input authorization and immutable Artifact selection belong to the caller.
/// A digest alone is not permission to read another organization's object.
pub trait ObjectSource {
    fn open(&self, digest: &str) -> Result<Box<dyn Read>>;
}
/// Private staging cache populated by an authorized object-store reader.
pub struct ObjectCache(Dir);
impl ObjectCache {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self(Dir::root(path)?))
    }
}
impl ObjectSource for ObjectCache {
    fn open(&self, digest: &str) -> Result<Box<dyn Read>> {
        if !model::is_digest(digest) {
            return Err(Error::InvalidRequest);
        }
        Ok(Box::new(self.0.file(&digest[7..])?))
    }
}

/// Trusted full-filesystem mount; applications may only receive a prepared data leaf.
pub struct MountedVolume {
    root: Dir,
    filesystem_uuid: String,
    volume_uid: String,
    volume_path: String,
    uid: u32,
    gid: u32,
}
impl MountedVolume {
    pub fn open(
        mount: &Path,
        volume_path: &str,
        filesystem_uuid: &str,
        volume_uid: &str,
        uid: u32,
        gid: u32,
    ) -> Result<Self> {
        if !model::relative(volume_path)
            || !model::identifier(filesystem_uuid)
            || !model::identifier(volume_uid)
            || uid == 0
            || uid == u32::MAX
            || gid == 0
            || gid == u32::MAX
        {
            return Err(Error::InvalidRequest);
        }
        let mount = Dir::root(mount)?;
        if fs::fstatfs(&mount.0)?.f_type != 0x65735546 {
            return Err(Error::UnsupportedBackend);
        }
        let config: serde_json::Value =
            serde_json::from_slice(&bounded(mount.file(".config")?, 65_536)?)
                .map_err(|_| Error::UnsupportedBackend)?;
        validate_mount(&config, filesystem_uuid)?;
        // The provisioned volume directory must already exist. Its identity and
        // exclusive ownership are operator preconditions, not tenant input.
        let root = mount.open(volume_path)?;
        root.private()?;
        Ok(Self {
            root,
            filesystem_uuid: filesystem_uuid.into(),
            volume_uid: volume_uid.into(),
            volume_path: volume_path.into(),
            uid,
            gid,
        })
    }
    #[cfg(test)]
    pub(crate) fn local(path: &Path) -> Result<Self> {
        Ok(Self {
            root: Dir::root(path)?,
            filesystem_uuid: "test-fs".into(),
            volume_uid: "volume-1".into(),
            volume_path: "volume".into(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
        })
    }
    #[cfg(test)]
    pub(crate) fn with_test_owner(mut self, uid: u32) -> Self {
        self.uid = uid;
        self
    }
    pub fn prepare(
        &self,
        request: &PrepareRequest,
        source: &impl ObjectSource,
        quota: &impl Quota,
    ) -> Result<Prepared> {
        let directories = request.validate()?;
        if request.volume_uid != self.volume_uid {
            return Err(Error::IdentityConflict);
        }
        let request_digest = model::digest(
            "agent-computer/candidate-preparation-v1",
            &(request, &self.volume_path, self.uid, self.gid),
        )?;
        let parent = self.root.ensure(&request.parent())?;
        if let Some(existing) = self.existing(&parent, request, &request_digest, quota)? {
            return Ok(existing);
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| Error::Io)?;
        let staging_name = format!(
            "staging_{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let staging = parent.mkdir(&staging_name)?;
        let data = staging.mkdir("data")?;
        self.quota(
            quota,
            &format!("{}/{staging_name}/data", request.parent()),
            request.quota_bytes,
        )?;
        for path in &directories {
            data.ensure(path)?;
        }
        for (path, entry) in &request.manifest.entries {
            if let Entry::File {
                sha256,
                size,
                executable,
            } = entry
            {
                let input = source.open(sha256)?;
                let mut output = data.create(path)?;
                copy_verified(input, &mut output, *size, sha256)?;
                owner(
                    &output,
                    self.uid,
                    self.gid,
                    if *executable { 0o700 } else { 0o600 },
                )?;
                output.sync_all()?;
            }
        }
        // Children before parents. The staging container remains private to the
        // worker even after assigning the data files to the future application.
        for path in directories.iter().rev() {
            let dir = data.open(path)?;
            owner(&dir.0, self.uid, self.gid, 0o700)?;
            dir.sync()?;
        }
        owner(&data.0, self.uid, self.gid, 0o700)?;
        data.sync()?;
        let receipt = Prepared {
            version: 1,
            request_digest,
            filesystem_uuid: self.filesystem_uuid.clone(),
            volume_uid: self.volume_uid.clone(),
            path_ref: request.path_ref(),
            data_inode: data.inode()?,
            manifest_digest: request.manifest_digest.clone(),
            quota_bytes: request.quota_bytes,
        };
        let mut file = staging.create("receipt.json")?;
        file.write_all(&serde_json::to_vec(&receipt).map_err(|_| Error::Io)?)?;
        fs::fchmod(&file, fs::Mode::from_raw_mode(0o400))?;
        file.sync_all()?;
        staging.sync()?;
        match fs::renameat_with(
            &parent.0,
            &staging_name,
            &parent.0,
            request.generation.to_string(),
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        // A competing publisher may have won; inspect its binding. Quota must
        // succeed at the final path, including retries after an uncertain result.
        self.existing(&parent, request, &receipt.request_digest, quota)?
            .ok_or(Error::Io)
    }
    fn quota(&self, quota: &impl Quota, path: &str, bytes: u64) -> Result<()> {
        quota.ensure(
            &self.filesystem_uuid,
            &format!("/{}/{path}", self.volume_path),
            bytes,
        )
    }
    fn existing(
        &self,
        parent: &Dir,
        request: &PrepareRequest,
        digest: &str,
        quota: &impl Quota,
    ) -> Result<Option<Prepared>> {
        let Some(dir) = parent.try_open(&request.generation.to_string())? else {
            return Ok(None);
        };
        let data = dir.open("data")?;
        let receipt: Prepared = serde_json::from_slice(&bounded(dir.file("receipt.json")?, 8192)?)
            .map_err(|_| Error::IdentityConflict)?;
        if receipt.version != 1
            || receipt.request_digest != digest
            || receipt.filesystem_uuid != self.filesystem_uuid
            || receipt.volume_uid != self.volume_uid
            || receipt.path_ref != request.path_ref()
            || receipt.data_inode != data.inode()?
            || receipt.manifest_digest != request.manifest_digest
            || receipt.quota_bytes != request.quota_bytes
        {
            return Err(Error::IdentityConflict);
        }
        self.quota(quota, &receipt.path_ref, receipt.quota_bytes)?;
        dir.sync()?;
        parent.sync()?;
        Ok(Some(receipt))
    }
}

fn copy_verified(
    mut input: Box<dyn Read>,
    output: &mut File,
    size: u64,
    expected: &str,
) -> Result<()> {
    let mut remaining = size;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65_536];
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let count = input.read(&mut buffer[..limit])?;
        if count == 0 {
            return Err(Error::InputMismatch);
        }
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    if input.read(&mut buffer[..1])? != 0 || format!("sha256:{:x}", hash.finalize()) != expected {
        return Err(Error::InputMismatch);
    }
    Ok(())
}

pub(crate) fn validate_mount(config: &serde_json::Value, uuid: &str) -> Result<()> {
    let subdir = config.get("Subdir").and_then(|v| v.as_str()).unwrap_or("");
    if config.pointer("/Format/UUID").and_then(|v| v.as_str()) != Some(uuid)
        || config.pointer("/Format/Storage").and_then(|v| v.as_str()) != Some("s3")
        || !config
            .get("Version")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "1.4.1" || s.starts_with("1.4.1+"))
        || !["", "/"].contains(&subdir)
        || config.pointer("/Chunk/Writeback").and_then(|v| v.as_bool()) != Some(false)
        || config
            .pointer("/FuseOpts/EnableWriteback")
            .and_then(|v| v.as_bool())
            != Some(false)
        || config.pointer("/Meta/ReadOnly").and_then(|v| v.as_bool()) != Some(false)
        || config
            .pointer("/FuseOpts/Options")
            .and_then(|v| v.as_array())
            .is_none_or(|a| {
                a.iter().any(|v| {
                    v.as_str().is_none_or(|s| {
                        s.split(',')
                            .any(|part| part.split('=').next() == Some("writeback_cache"))
                    })
                })
            })
    {
        return Err(Error::UnsupportedBackend);
    }
    Ok(())
}
