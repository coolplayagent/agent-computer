use crate::{Error, Request, Result};
use rustix::fs::{Mode, OFlags, ResolveFlags, fstat, fstatfs, open, openat2};
use std::{fs::File, io::Read, os::unix::fs::FileExt};

/// Open descriptions pin the original kernfs nodes, including across path reuse.
pub(crate) struct Cgroup {
    _directory: File,
    kill: File,
    events: File,
    pub device: u64,
}

impl Cgroup {
    pub fn open(request: &Request) -> Result<Self> {
        Self::open_matching(request, None)
    }

    pub fn open_matching(request: &Request, device: Option<u64>) -> Result<Self> {
        let mut dir = File::from(open(
            "/sys/fs/cgroup",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if fstatfs(&dir)?.f_type != 0x6367_7270 {
            return Err(Error::CgroupRequired);
        }
        trusted(&dir)?;
        for part in request.cgroup_path.split('/') {
            dir = child(&dir, part, OFlags::RDONLY | OFlags::DIRECTORY)?;
            trusted(&dir)?;
            // A workload must not be delegated migration authority outside its tree.
            trusted(&child(&dir, "cgroup.procs", OFlags::RDONLY)?)?;
        }
        let stat = fstat(&dir)?;
        if stat.st_ino != request.cgroup_inode || device.is_some_and(|v| v != stat.st_dev) {
            return Err(Error::IdentityMismatch);
        }
        if read_control(&child(&dir, "cgroup.type", OFlags::RDONLY)?)? != "domain\n" {
            return Err(Error::CgroupRequired);
        }
        let kill = child(&dir, "cgroup.kill", OFlags::WRONLY)?;
        trusted(&kill)?;
        let events = child(&dir, "cgroup.events", OFlags::RDONLY)?;
        trusted(&events)?;
        // No empty-tree rejection: the bootstrap may not have spawned yet.
        populated(&read_control(&events)?)?;
        Ok(Self {
            _directory: dir,
            kill,
            events,
            device: stat.st_dev,
        })
    }

    pub fn kill(&self) -> Result<()> {
        match rustix::io::write(&self.kill, b"1") {
            Ok(1) => Ok(()),
            _ => Err(Error::KillFailed),
        }
    }

    pub fn populated(&self) -> Result<bool> {
        populated(&read_control(&self.events)?)
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // Best effort after any setup/report failure; never a drain assertion.
        let _ = self.kill();
    }
}

fn child(parent: &File, name: &str, flags: OFlags) -> Result<File> {
    Ok(File::from(openat2(
        parent,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )?))
}

fn trusted(file: &File) -> Result<()> {
    let stat = fstat(file)?;
    if stat.st_uid != 0 || stat.st_mode & 0o022 != 0 {
        return Err(Error::UntrustedCgroup);
    }
    Ok(())
}

fn read_control(file: &File) -> Result<String> {
    let mut bytes = [0; 4096];
    let count = file
        .read_at(&mut bytes, 0)
        .map_err(|_| Error::ObservationFailed)?;
    if count == bytes.len() {
        return Err(Error::ObservationFailed);
    }
    String::from_utf8(bytes[..count].to_vec()).map_err(|_| Error::ObservationFailed)
}

fn populated(value: &str) -> Result<bool> {
    let mut found = None;
    for line in value.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            return Err(Error::ObservationFailed);
        };
        if key == "populated" {
            if found.is_some() {
                return Err(Error::ObservationFailed);
            }
            found = Some(match value {
                "0" => false,
                "1" => true,
                _ => return Err(Error::ObservationFailed),
            });
        }
    }
    found.ok_or(Error::ObservationFailed)
}

pub(crate) fn read_small(path: &str) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| Error::Setup)?
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Setup)?;
    if bytes.len() > 4096 {
        return Err(Error::Setup);
    }
    String::from_utf8(bytes).map_err(|_| Error::Setup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_unique_zero_is_an_empty_observation() {
        assert_eq!(populated("populated 0\nfrozen 0\n"), Ok(false));
        assert_eq!(populated("populated 1\nfrozen 0\n"), Ok(true));
        for input in [
            "",
            "frozen 0\n",
            "populated 2\n",
            "populated 0\npopulated 1\n",
            "populated 00\n",
            "populated 0 \n",
        ] {
            assert!(populated(input).is_err(), "{input}");
        }
    }
}
