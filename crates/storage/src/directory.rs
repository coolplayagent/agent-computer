use crate::{Error, Result};
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::fs::{self, FileType, Mode, OFlags, ResolveFlags};
use std::{fs::File, io::Read, path::Path};

pub(crate) struct Dir(pub OwnedFd);
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);
impl Dir {
    /// The operator controls the absolute root and every ancestor of it.
    pub fn root(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::InvalidRequest);
        }
        Ok(Self(fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    }
    pub fn open(&self, path: &str) -> Result<Self> {
        self.try_open(path)?.ok_or(Error::InvalidFilesystemObject)
    }
    pub fn try_open(&self, path: &str) -> Result<Option<Self>> {
        match fs::openat2(
            &self.0,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            RESOLVE,
        ) {
            Ok(fd) => Ok(Some(Self(fd))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(_) => Err(Error::InvalidFilesystemObject),
        }
    }
    pub fn mkdir(&self, name: &str) -> Result<Self> {
        fs::mkdirat(&self.0, name, Mode::from_raw_mode(0o700))?;
        let dir = self.open(name)?;
        dir.sync()?;
        self.sync()?;
        Ok(dir)
    }
    pub fn ensure(&self, path: &str) -> Result<Self> {
        let mut current = Self(self.0.try_clone()?);
        current.private()?;
        for part in path.split('/') {
            match fs::mkdirat(&current.0, part, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            let next = current.open(part)?;
            next.private()?;
            // Also repair a previously lost directory-sync acknowledgement on retry.
            next.sync()?;
            current.sync()?;
            current = next;
        }
        Ok(current)
    }
    pub fn file(&self, path: &str) -> Result<File> {
        let fd = fs::openat2(
            &self.0,
            path,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(|_| Error::InvalidFilesystemObject)?;
        let stat = fs::fstat(&fd)?;
        if !FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
            return Err(Error::InvalidFilesystemObject);
        }
        // Reopen the pinned inode, not the attacker-changeable pathname. O_PATH
        // lets us reject a FIFO/device without opening or blocking on that object.
        Ok(File::open(format!("/proc/self/fd/{}", fd.as_raw_fd()))?)
    }
    pub fn create(&self, path: &str) -> Result<File> {
        Ok(File::from(fs::openat2(
            &self.0,
            path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
            RESOLVE,
        )?))
    }
    pub fn private(&self) -> Result<()> {
        let stat = fs::fstat(&self.0)?;
        if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o077 != 0 {
            return Err(Error::InvalidFilesystemObject);
        }
        Ok(())
    }
    pub fn inode(&self) -> Result<u64> {
        Ok(fs::fstat(&self.0)?.st_ino)
    }
    pub fn sync(&self) -> Result<()> {
        Ok(fs::fsync(&self.0)?)
    }
}

pub(crate) fn bounded(file: File, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::InvalidRequest);
    }
    Ok(bytes)
}
pub(crate) fn owner(fd: &impl rustix::fd::AsFd, uid: u32, gid: u32, mode: u32) -> Result<()> {
    fs::fchown(
        fd,
        Some(rustix::process::Uid::from_raw(uid)),
        Some(rustix::process::Gid::from_raw(gid)),
    )?;
    fs::fchmod(fd, Mode::from_raw_mode(mode))?;
    Ok(())
}
