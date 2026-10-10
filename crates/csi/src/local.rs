//! Descriptor anchored, root-only local state. No tenant-selected path traversal.
use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};
pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
pub(crate) fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
pub(crate) fn directory(path: &Path) -> io::Result<File> {
    if !rustix::process::geteuid().is_root() || !path.is_absolute() {
        return Err(invalid("root and absolute private path required"));
    }
    let mut f = File::open("/")?;
    for c in path.components().skip(1) {
        let Component::Normal(n) = c else {
            return Err(invalid("invalid path"));
        };
        if !n.to_str().is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
        }) {
            return Err(invalid("invalid path component"));
        }
        f = child(&f, n, true)?;
        let m = f.metadata()?;
        if m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(invalid("untrusted directory"));
        }
    }
    Ok(f)
}
pub(crate) fn child(parent: &File, name: impl rustix::path::Arg, dir: bool) -> io::Result<File> {
    let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if dir {
        flags |= OFlags::DIRECTORY;
    }
    Ok(File::from(openat2(
        parent,
        name,
        flags,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
    )?))
}
pub(crate) fn mkdir(parent: &File, name: &str) -> io::Result<File> {
    if !self::name(name) {
        return Err(invalid("invalid directory name"));
    }
    match rustix::fs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => parent.sync_all()?,
        Err(rustix::io::Errno::EXIST) => (),
        Err(e) => return Err(e.into()),
    }
    let f = child(parent, name, true)?;
    let m = f.metadata()?;
    if m.uid() != 0 || m.mode() & 0o7777 != 0o700 {
        return Err(invalid("private root directory required"));
    }
    Ok(f)
}
pub(crate) fn write_new(parent: &File, name: &str, value: &impl Serialize) -> io::Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > 65536 {
        return Err(invalid("record too large"));
    }
    let mut f = File::from(openat2(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
    )?);
    f.write_all(&data)?;
    f.sync_all()?;
    parent.sync_all()
}
pub(crate) fn read<T: DeserializeOwned>(parent: &File, name: &str) -> io::Result<Option<T>> {
    let f = match child(parent, name, false) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let m = f.metadata()?;
    if !m.is_file()
        || m.uid() != 0
        || m.mode() & 0o7777 != 0o600
        || m.nlink() != 1
        || m.len() > 65536
    {
        return Err(invalid("untrusted record"));
    }
    let mut data = Vec::new();
    f.take(65537).read_to_end(&mut data)?;
    if data.len() > 65536 {
        return Err(invalid("record too large"));
    }
    Ok(Some(serde_json::from_slice(&data)?))
}
pub(crate) fn lock(parent: &File) -> io::Result<File> {
    let f = File::from(openat2(
        parent,
        ".lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
    )?);
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o7777 != 0o600 || m.nlink() != 1 {
        return Err(invalid("untrusted lock"));
    }
    rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
    Ok(f)
}
