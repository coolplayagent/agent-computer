use fuser::{Errno, FileAttr, FileHandle, FileType, INodeNo, OpenAccMode, OpenFlags};
use rustix::fs::{self, AtFlags, Mode, OFlags, ResolveFlags};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt},
        },
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) type Result<T> = std::result::Result<T, Errno>;
pub(crate) const MAX_NODES: usize = 16_384;
pub(crate) const MAX_HANDLES: usize = 4096;
pub(crate) const MAX_IO: usize = 128 * 1024;
// Qualified JuiceFS v1.4.1 reserves this range for virtual operator files.
const MIN_INTERNAL_INODE: u64 = 0x7FFF_FFFF_0000_0000;
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);
pub(crate) fn err(e: impl Into<std::io::Error>) -> Errno {
    Errno::from_i32(e.into().raw_os_error().unwrap_or(5))
}
pub(crate) fn name(n: &OsStr) -> Result<()> {
    let b = n.as_bytes();
    if b.is_empty()
        || b.len() > 255
        || b == b"."
        || b == b".."
        || b.contains(&b'/')
        || b.contains(&0)
        || b.starts_with(b".agent-computer-")
        || b.starts_with(b".jfs.")
        || matches!(b, b".control")
    {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}
pub(crate) fn mode(mode: u32) -> Result<Mode> {
    if mode & 0o7000 != 0 {
        return Err(Errno::EPERM);
    }
    Ok(Mode::from_raw_mode(mode & 0o777))
}
fn kind(m: &std::fs::Metadata) -> Result<FileType> {
    if m.is_file() {
        Ok(FileType::RegularFile)
    } else if m.is_dir() {
        Ok(FileType::Directory)
    } else if m.is_symlink() {
        Ok(FileType::Symlink)
    } else {
        Err(Errno::EPERM)
    }
}
fn time(s: i64, n: i64) -> SystemTime {
    if s >= 0 {
        UNIX_EPOCH + Duration::new(s as u64, n.max(0) as u32)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(s.unsigned_abs()))
            .unwrap_or(UNIX_EPOCH)
    }
}
pub(crate) struct Node {
    pub file: File,
}
pub(crate) struct Handle {
    pub ino: u64,
    pub file: File,
    pub writable: bool,
    pub directory: Option<Vec<(u64, FileType, OsString)>>,
}
pub(crate) struct State {
    pub nodes: BTreeMap<u64, Node>,
    pub handles: BTreeMap<u64, Handle>,
    identities: BTreeMap<(u64, u64), u64>,
    next_node: u64,
    next_handle: u64,
    pub uid: u32,
    pub gid: u32,
    pub failed: bool,
    pub mutations: u64,
}
impl State {
    pub fn new(root: File, uid: u32, gid: u32) -> std::io::Result<Self> {
        let m = root.metadata()?;
        if !m.is_dir() || m.uid() != uid || m.gid() != gid {
            return Err(std::io::Error::other(
                "Candidate directory identity mismatch",
            ));
        }
        Ok(Self {
            nodes: [(1, Node { file: root })].into(),
            handles: BTreeMap::new(),
            identities: [((m.dev(), m.ino()), 1)].into(),
            next_node: 2,
            next_handle: 1,
            uid,
            gid,
            failed: false,
            mutations: 0,
        })
    }
    pub fn node(&self, ino: u64) -> Result<&File> {
        self.nodes.get(&ino).map(|n| &n.file).ok_or(Errno::ESTALE)
    }
    pub fn handle(&self, ino: u64, fh: u64) -> Result<&Handle> {
        self.handles
            .get(&fh)
            .filter(|h| h.ino == ino)
            .ok_or(Errno::EBADF)
    }
    pub fn attr(&self, ino: u64) -> Result<FileAttr> {
        let m = self.node(ino)?.metadata().map_err(err)?;
        Ok(FileAttr {
            ino: INodeNo(ino),
            size: m.size(),
            blocks: m.blocks(),
            atime: time(m.atime(), m.atime_nsec()),
            mtime: time(m.mtime(), m.mtime_nsec()),
            ctime: time(m.ctime(), m.ctime_nsec()),
            crtime: UNIX_EPOCH,
            kind: kind(&m)?,
            perm: (m.mode() & 0o777) as u16,
            nlink: m.nlink().try_into().map_err(|_| Errno::EOVERFLOW)?,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    fn register(&mut self, file: File) -> Result<u64> {
        let m = file.metadata().map_err(err)?;
        kind(&m)?;
        let root = self.node(1)?.metadata().map_err(err)?;
        if m.dev() != root.dev()
            || m.ino() >= MIN_INTERNAL_INODE
            || m.uid() != self.uid
            || m.gid() != self.gid
            || (m.is_file() && m.nlink() != 1)
            || m.mode() & 0o7000 != 0
        {
            return Err(Errno::EPERM);
        }
        let id = (m.dev(), m.ino());
        if let Some(ino) = self.identities.get(&id) {
            return Ok(*ino);
        }
        if self.nodes.len() >= MAX_NODES {
            return Err(Errno::ENOSPC);
        }
        let ino = self.next_node;
        self.next_node = self.next_node.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        self.nodes.insert(ino, Node { file });
        self.identities.insert(id, ino);
        Ok(ino)
    }
    pub fn lookup(&mut self, parent: u64, n: &OsStr) -> Result<u64> {
        name(n)?;
        let fd = fs::openat2(
            self.node(parent)?,
            n,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(err)?;
        self.register(fd.into())
    }
    pub fn reopen(&self, ino: u64, flags: OFlags) -> Result<File> {
        let node = self.node(ino)?;
        let m = node.metadata().map_err(err)?;
        if !m.is_file() && !m.is_dir() {
            return Err(Errno::ELOOP);
        }
        // Reopen a pinned inode, never a tenant-controlled pathname or symlink.
        fs::open(
            format!("/proc/self/fd/{}", node.as_raw_fd()),
            flags | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(err)
    }
    fn sync(&mut self, file: &File) -> Result<()> {
        file.sync_all().map_err(|e| {
            self.failed = true;
            err(e)
        })
    }
    fn effect<T>(&mut self, result: std::io::Result<T>) -> Result<T> {
        result.map_err(|e| {
            // Ambiguous backend failures permanently prevent a drain receipt.
            // Only definite, effect-free POSIX rejections leave the gate usable.
            // Transport failures, timeouts, quota and capacity errors may hide
            // an accepted operation and must never become a positive receipt.
            if !matches!(
                e.raw_os_error(),
                Some(1 | 2 | 13 | 17 | 18 | 20 | 21 | 22 | 30 | 36 | 38 | 39 | 40 | 95)
            ) {
                self.failed = true;
            }
            err(e)
        })
    }
    pub fn sync_node(&mut self, ino: u64) -> Result<()> {
        let f = self.reopen(ino, OFlags::RDONLY)?;
        self.sync(&f)
    }
    fn owned(&mut self, file: &File, permissions: Mode) -> Result<()> {
        self.effect(
            fs::fchown(
                file,
                Some(rustix::process::Uid::from_raw(self.uid)),
                Some(rustix::process::Gid::from_raw(self.gid)),
            )
            .map_err(Into::into),
        )?;
        self.effect(fs::fchmod(file, permissions).map_err(Into::into))?;
        self.sync(file)
    }
    pub fn add_handle(
        &mut self,
        ino: u64,
        file: File,
        writable: bool,
        directory: Option<Vec<(u64, FileType, OsString)>>,
    ) -> Result<u64> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(Errno::EMFILE);
        }
        let fh = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        self.handles.insert(
            fh,
            Handle {
                ino,
                file,
                writable,
                directory,
            },
        );
        Ok(fh)
    }
    pub fn open(&mut self, ino: u64, flags: OpenFlags) -> Result<(FileHandle, bool)> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(Errno::EMFILE);
        }
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        if self.attr(ino)?.kind != FileType::RegularFile {
            return Err(Errno::EISDIR);
        }
        let f = self.reopen(
            ino,
            if writable {
                OFlags::RDWR
            } else {
                OFlags::RDONLY
            },
        )?;
        if flags.0 as u32 & OFlags::TRUNC.bits() != 0 {
            if !writable {
                return Err(Errno::EACCES);
            }
            self.effect(f.set_len(0))?;
            self.sync(&f)?;
        }
        Ok((
            FileHandle(self.add_handle(ino, f, writable, None)?),
            writable,
        ))
    }
    pub fn create(
        &mut self,
        parent: u64,
        n: &OsStr,
        permissions: u32,
        flags: OpenFlags,
    ) -> Result<(FileAttr, FileHandle)> {
        name(n)?;
        let permissions = mode(permissions)?;
        if self.nodes.len() >= MAX_NODES || self.handles.len() >= MAX_HANDLES {
            return Err(Errno::ENOSPC);
        }
        let f = File::from(
            self.effect(
                fs::openat2(
                    self.node(parent)?,
                    n,
                    OFlags::RDWR
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::CLOEXEC
                        | OFlags::NOFOLLOW,
                    permissions,
                    RESOLVE,
                )
                .map_err(Into::into),
            )?,
        );
        // Failure after creation must never be mistaken for a clean rejection.
        if let Err(e) = self
            .owned(&f, permissions)
            .and_then(|_| self.sync_node(parent))
        {
            self.failed = true;
            return Err(e);
        }
        let ino = self.register(f.try_clone().map_err(err)?)?;
        let fh = self.add_handle(ino, f, flags.acc_mode() != OpenAccMode::O_RDONLY, None)?;
        Ok((self.attr(ino)?, FileHandle(fh)))
    }
    pub fn mkdir(&mut self, parent: u64, n: &OsStr, permissions: u32) -> Result<FileAttr> {
        name(n)?;
        let permissions = mode(permissions)?;
        if self.nodes.len() >= MAX_NODES {
            return Err(Errno::ENOSPC);
        }
        self.effect(fs::mkdirat(self.node(parent)?, n, permissions).map_err(Into::into))?;
        let result = (|| {
            let f = File::from(
                fs::openat2(
                    self.node(parent)?,
                    n,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    RESOLVE,
                )
                .map_err(err)?,
            );
            self.owned(&f, permissions)?;
            self.sync_node(parent)?;
            let ino = self.register(f)?;
            self.attr(ino)
        })();
        if result.is_err() {
            self.failed = true
        }
        result
    }
    pub fn remove(&mut self, parent: u64, n: &OsStr, directory: bool) -> Result<()> {
        name(n)?;
        self.effect(
            fs::unlinkat(
                self.node(parent)?,
                n,
                if directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                },
            )
            .map_err(Into::into),
        )?;
        self.sync_node(parent)
    }
    pub fn rename(
        &mut self,
        parent: u64,
        n: &OsStr,
        newparent: u64,
        new: &OsStr,
        flags: u32,
    ) -> Result<()> {
        name(n)?;
        name(new)?;
        let flags = fs::RenameFlags::from_bits(flags).ok_or(Errno::EINVAL)?;
        if !(fs::RenameFlags::NOREPLACE | fs::RenameFlags::EXCHANGE).contains(flags) {
            return Err(Errno::EINVAL);
        }
        self.effect(
            fs::renameat_with(self.node(parent)?, n, self.node(newparent)?, new, flags)
                .map_err(Into::into),
        )?;
        self.sync_node(parent)?;
        self.sync_node(newparent)
    }
    pub fn write(&mut self, ino: u64, fh: u64, offset: u64, data: &[u8]) -> Result<u32> {
        if data.len() > MAX_IO
            || offset
                .checked_add(data.len() as u64)
                .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(Errno::EFBIG);
        }
        let h = self.handle(ino, fh)?;
        if !h.writable {
            return Err(Errno::EBADF);
        }
        let f = h.file.try_clone().map_err(err)?;
        // All writes, including writes through a subsequently unlinked handle,
        // finish persistence before their FUSE acknowledgement.
        if let Err(e) = f.write_all_at(data, offset) {
            self.failed = true;
            return Err(err(e));
        }
        self.sync(&f)?;
        Ok(data.len() as u32)
    }
    pub fn read(&self, ino: u64, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        if size as usize > MAX_IO || offset > i64::MAX as u64 {
            return Err(Errno::EINVAL);
        }
        let h = self.handle(ino, fh)?;
        let mut b = vec![0; size as usize];
        let mut read = 0;
        while read < b.len() {
            let n = h
                .file
                .read_at(&mut b[read..], offset + read as u64)
                .map_err(err)?;
            if n == 0 {
                break;
            }
            read += n;
        }
        b.truncate(read);
        Ok(b)
    }
    pub fn sync_handle(&mut self, ino: u64, fh: u64) -> Result<()> {
        let f = self.handle(ino, fh)?.file.try_clone().map_err(err)?;
        self.sync(&f)
    }
    pub fn release(&mut self, ino: u64, fh: u64) -> Result<()> {
        self.handle(ino, fh)?;
        let h = self.handles.remove(&fh).ok_or(Errno::EBADF)?;
        // No queued dirty data is introduced by close: every prior mutator was
        // synchronously flushed. Retain an error if a final sync fails as well.
        if h.writable {
            self.sync(&h.file)?
        }
        Ok(())
    }
    pub fn opendir(&mut self, ino: u64) -> Result<FileHandle> {
        let f = self.reopen(ino, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let mut names = Vec::new();
        for e in fs::Dir::read_from(&f).map_err(err)? {
            let e = e.map_err(err)?;
            let n = OsStr::from_bytes(e.file_name().to_bytes());
            if n == "." || n == ".." {
                continue;
            }
            name(n)?;
            if names.len() >= MAX_NODES {
                return Err(Errno::ENOSPC);
            }
            names.push(n.to_owned());
        }
        names.sort();
        let mut entries = vec![(ino, FileType::Directory, OsString::from("."))];
        for n in names {
            let id = self.lookup(ino, &n)?;
            entries.push((id, self.attr(id)?.kind, n));
        }
        Ok(FileHandle(self.add_handle(ino, f, false, Some(entries))?))
    }
    pub fn setattr(
        &mut self,
        ino: u64,
        permissions: Option<u32>,
        size: Option<u64>,
        times: Option<fs::Timestamps>,
    ) -> Result<FileAttr> {
        let directory = self.attr(ino)?.kind == FileType::Directory;
        if directory && size.is_some() {
            return Err(Errno::EISDIR);
        }
        if size.is_some_and(|n| n > i64::MAX as u64) {
            return Err(Errno::EFBIG);
        }
        let permissions = permissions.map(mode).transpose()?;
        let f = self.reopen(
            ino,
            if directory {
                OFlags::RDONLY | OFlags::DIRECTORY
            } else {
                OFlags::RDWR
            },
        )?;
        // A compound setattr may fail after an earlier field was changed.
        // Persist even that partial result before returning the original error.
        let result: Result<()> = (|| {
            if let Some(n) = size {
                self.effect(f.set_len(n))?;
            }
            if let Some(p) = permissions {
                self.effect(fs::fchmod(&f, p).map_err(Into::into))?;
            }
            if let Some(t) = times {
                self.effect(fs::futimens(&f, &t).map_err(Into::into))?;
            }
            Ok(())
        })();
        self.sync(&f)?;
        result?;
        self.attr(ino)
    }
}
