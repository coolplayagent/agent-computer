//! Mount identity for trusted local routing. This is deliberately not a seal proof.
use agent_computer_storage::Prepared;
use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountReference {
    pub version: u32,
    pub instance: String,
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub boot_id: String,
    pub mount_namespace: u64,
    pub prepared: Prepared,
}
impl MountReference {
    pub(crate) fn observe(path: &Path, instance: &str, prepared: &Prepared) -> io::Result<Self> {
        let file = open(path)?;
        let m = file.metadata()?;
        let result = Self {
            version: 1,
            instance: instance.into(),
            path: path.into(),
            device: m.dev(),
            inode: m.ino(),
            boot_id: boot_id()?,
            mount_namespace: namespace()?,
            prepared: prepared.clone(),
        };
        result.verify()?;
        Ok(result)
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1
            || self.instance.len() != 64
            || !self
                .instance
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.path.to_str().is_none_or(|p| {
                !p.starts_with('/')
                    || p.ends_with('/')
                    || p.split('/').skip(1).any(|s| {
                        s.is_empty()
                            || s == "."
                            || s == ".."
                            || !s
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                    })
            })
            || self.inode != 1
            || self.device == 0
            || self.mount_namespace == 0
            || self.boot_id.len() != 36
            || self.prepared.version != 1
        {
            return Err(io::Error::other("invalid Candidate mount reference"));
        }
        Ok(())
    }
    /// Checks a currently accessible mount in this boot and mount namespace.
    /// Authority to use it still requires the private operator registration.
    pub fn verify(&self) -> io::Result<File> {
        self.validate()?;
        if boot_id()? != self.boot_id || namespace()? != self.mount_namespace {
            return Err(io::Error::other(
                "Candidate mount boot or namespace changed",
            ));
        }
        let f = open(&self.path)?;
        self.verify_file(&f)?;
        Ok(f)
    }
    pub fn verify_file(&self, file: &File) -> io::Result<()> {
        let m = file.metadata()?;
        if m.dev() != self.device
            || m.ino() != self.inode
            || !m.is_dir()
            || m.uid() != 1000
            || m.gid() != 1000
            || m.mode() & 0o7777 != 0o700
            || rustix::fs::fstatfs(file)?.f_type != 0x6573_5546
        {
            return Err(io::Error::other("Candidate front mount identity changed"));
        }
        Ok(())
    }
}
fn open(path: &Path) -> io::Result<File> {
    Ok(File::from(openat2(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )?))
}
pub(crate) fn boot_id() -> io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .into())
}
fn namespace() -> io::Result<u64> {
    Ok(std::fs::metadata("/proc/self/ns/mnt")?.ino())
}
