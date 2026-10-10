use crate::{Error, MAX_BYTES, ObjectRef, Result, digest};
use rustix::fs::{self, Mode, OFlags, RenameFlags};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    sync::Arc,
};

/// Private durable bytes for retrying object publication, never workload replay.
#[derive(Clone)]
pub struct Spool(Arc<File>);
fn directory(parent: &File, name: &str) -> Result<File> {
    let file = File::from(
        fs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Error::Io)?,
    );
    private(&file, true)?;
    Ok(file)
}
fn private(file: &File, directory: bool) -> Result<()> {
    let m = file.metadata().map_err(|_| Error::Io)?;
    if m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o077 != 0
        || if directory {
            !m.is_dir()
        } else {
            !m.is_file() || m.nlink() != 1
        }
    {
        return Err(Error::Configuration);
    }
    Ok(())
}
fn name(binding: &str) -> Result<String> {
    if !digest(binding) {
        return Err(Error::Invalid);
    }
    Ok(format!("output-{}", &binding[7..]))
}
impl Spool {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Configuration);
        }
        let file = File::from(
            fs::openat2(
                fs::CWD,
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                fs::ResolveFlags::NO_SYMLINKS,
            )
            .map_err(|_| Error::Configuration)?,
        );
        private(&file, true)?;
        Ok(Self(Arc::new(file)))
    }
    pub fn read(&self, binding: &str, object: &ObjectRef) -> Result<Vec<u8>> {
        object.validate()?;
        let dir = directory(&self.0, &name(binding)?)?;
        let file = File::from(
            fs::openat(
                &dir,
                &object.sha256[7..],
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Error::Missing)?,
        );
        private(&file, false)?;
        if file.metadata().map_err(|_| Error::Io)?.len() != object.size {
            return Err(Error::Integrity);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Io)?;
        object.verify(&bytes)?;
        Ok(bytes)
    }
    pub fn record(&self, binding: &str, objects: &[(ObjectRef, Vec<u8>)]) -> Result<()> {
        let target = name(binding)?;
        if objects.is_empty() || objects.len() > 4 {
            return Err(Error::Invalid);
        }
        for (object, bytes) in objects {
            object.verify(bytes)?;
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Io)?;
        let temporary = format!(
            "output-pending-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        fs::mkdirat(&*self.0, &temporary, Mode::from_raw_mode(0o700)).map_err(|_| Error::Io)?;
        let dir = directory(&self.0, &temporary)?;
        let mut files = std::collections::BTreeSet::new();
        let result = (|| {
            for (object, bytes) in objects {
                let file_name = &object.sha256[7..];
                if !files.insert(file_name.to_owned()) {
                    continue;
                }
                let mut file = File::from(
                    fs::openat(
                        &dir,
                        file_name,
                        OFlags::WRONLY
                            | OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC,
                        Mode::from_raw_mode(0o600),
                    )
                    .map_err(|_| Error::Io)?,
                );
                file.write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|_| Error::Io)?;
            }
            dir.sync_all().map_err(|_| Error::Io)?;
            match fs::renameat_with(
                &*self.0,
                &temporary,
                &*self.0,
                &target,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => {
                    for (object, bytes) in objects {
                        if self.read(binding, object)? != *bytes {
                            return Err(Error::Integrity);
                        }
                    }
                }
                Err(_) => return Err(Error::Io),
            }
            self.0.sync_all().map_err(|_| Error::Io)
        })();
        // Only our newly allocated staging directory is eligible for cleanup.
        // If it was renamed, cleanup via the retained FD would remove live data.
        if directory(&self.0, &temporary).is_ok() {
            for file in files {
                let _ = fs::unlinkat(&dir, file, fs::AtFlags::empty());
            }
            let _ = fs::unlinkat(&*self.0, &temporary, fs::AtFlags::REMOVEDIR);
        }
        result
    }
}
