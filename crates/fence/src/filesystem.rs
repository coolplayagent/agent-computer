use crate::{Gate, backend};
use fuser::*;
use std::{
    ffi::OsStr,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};
const TTL: Duration = Duration::ZERO;
pub(crate) struct Filesystem(pub Arc<Gate>);
impl fuser::Filesystem for Filesystem {
    fn init(&mut self, _: &Request, c: &mut KernelConfig) -> std::io::Result<()> {
        c.set_max_write(backend::MAX_IO as u32)
            .map_err(|_| std::io::Error::other("bounded FUSE writes required"))?;
        c.set_max_readahead(backend::MAX_IO as u32)
            .map_err(|_| std::io::Error::other("bounded FUSE reads required"))?;
        // fuser 0.18's pinned defaults do not negotiate writeback, passthrough,
        // DAX, direct-IO mmap, or zero-message opens. Never add those capabilities.
        Ok(())
    }
    fn lookup(&self, r: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.0.access(r.uid(), false, |s| {
            let ino = s.lookup(parent.0, name)?;
            s.attr(ino)
        }) {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }
    fn getattr(&self, r: &Request, ino: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        match self.0.access(r.uid(), false, |s| s.attr(ino.0)) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }
    fn setattr(
        &self,
        r: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _: Option<SystemTime>,
        _: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = self.0.access(r.uid(), true, |s| {
            if uid.is_some_and(|n| n != s.uid)
                || gid.is_some_and(|n| n != s.gid)
                || crtime.is_some()
                || chgtime.is_some()
                || bkuptime.is_some()
                || flags.is_some()
            {
                return Err(Errno::EPERM);
            }
            let times = if atime.is_some() || mtime.is_some() {
                Some(rustix::fs::Timestamps {
                    last_access: timestamp(atime)?,
                    last_modification: timestamp(mtime)?,
                })
            } else {
                None
            };
            s.setattr(ino.0, mode, size, times)
        });
        match result {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }
    fn mkdir(
        &self,
        r: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match self
            .0
            .access(r.uid(), true, |s| s.mkdir(parent.0, name, mode & !umask))
        {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }
    fn create(
        &self,
        r: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match self.0.access(r.uid(), true, |s| {
            s.create(parent.0, name, mode & !umask, OpenFlags(flags))
        }) {
            Ok((a, fh)) => reply.created(&TTL, &a, Generation(0), fh, FopenFlags::FOPEN_DIRECT_IO),
            Err(e) => reply.error(e),
        }
    }
    fn open(&self, r: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        match self.0.access(r.uid(), write, |s| s.open(ino.0, flags)) {
            Ok((fh, _)) => reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO),
            Err(e) => reply.error(e),
        }
    }
    fn read(
        &self,
        r: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self
            .0
            .access(r.uid(), false, |s| s.read(ino.0, fh.0, offset, size))
        {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(e),
        }
    }
    fn write(
        &self,
        r: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        flags: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.0.access(r.uid(), true, |s| {
            if write_flags.contains(WriteFlags::FUSE_WRITE_CACHE) {
                s.failed = true;
                return Err(Errno::EIO);
            }
            let offset = if flags.0 as u32 & rustix::fs::OFlags::APPEND.bits() != 0 {
                s.handle(ino.0, fh.0)?
                    .file
                    .metadata()
                    .map_err(backend::err)?
                    .len()
            } else {
                offset
            };
            s.write(ino.0, fh.0, offset, data)
        }) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }
    fn flush(&self, r: &Request, ino: INodeNo, fh: FileHandle, _: LockOwner, reply: ReplyEmpty) {
        match self
            .0
            .access(r.uid(), false, |s| s.sync_handle(ino.0, fh.0))
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn fsync(&self, r: &Request, ino: INodeNo, fh: FileHandle, _: bool, reply: ReplyEmpty) {
        match self
            .0
            .access(r.uid(), false, |s| s.sync_handle(ino.0, fh.0))
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn release(
        &self,
        r: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        match self.0.access(r.uid(), false, |s| s.release(ino.0, fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn unlink(&self, r: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self
            .0
            .access(r.uid(), true, |s| s.remove(parent.0, name, false))
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rmdir(&self, r: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self
            .0
            .access(r.uid(), true, |s| s.remove(parent.0, name, true))
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rename(
        &self,
        r: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        match self.0.access(r.uid(), true, |s| {
            s.rename(parent.0, name, newparent.0, newname, flags.bits())
        }) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn opendir(&self, r: &Request, ino: INodeNo, _: OpenFlags, reply: ReplyOpen) {
        match self.0.access(r.uid(), false, |s| s.opendir(ino.0)) {
            Ok(fh) => reply.opened(fh, FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }
    fn readdir(
        &self,
        r: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = self.0.access(r.uid(), false, |s| {
            let entries = s
                .handle(ino.0, fh.0)?
                .directory
                .as_ref()
                .ok_or(Errno::ENOTDIR)?;
            let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
            for (i, (id, kind, name)) in entries.iter().enumerate().skip(offset) {
                if reply.add(INodeNo(*id), (i + 1) as u64, *kind, name) {
                    break;
                }
            }
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn releasedir(
        &self,
        r: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        reply: ReplyEmpty,
    ) {
        match self.0.access(r.uid(), false, |s| s.release(ino.0, fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn fsyncdir(&self, r: &Request, ino: INodeNo, fh: FileHandle, _: bool, reply: ReplyEmpty) {
        match self
            .0
            .access(r.uid(), false, |s| s.sync_handle(ino.0, fh.0))
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn statfs(&self, r: &Request, _: INodeNo, reply: ReplyStatfs) {
        match self.0.access(r.uid(), false, |s| {
            let stat = rustix::fs::fstatvfs(s.node(1)?).map_err(backend::err)?;
            Ok((stat, s.nodes.len() as u64))
        }) {
            Ok((s, nodes)) => {
                let blocks = (self.0.prepared.quota_bytes / 4096)
                    .min(s.f_blocks.saturating_mul(s.f_frsize) / 4096);
                reply.statfs(
                    blocks,
                    blocks.min(s.f_bfree.saturating_mul(s.f_frsize) / 4096),
                    blocks.min(s.f_bavail.saturating_mul(s.f_frsize) / 4096),
                    backend::MAX_NODES as u64,
                    backend::MAX_NODES as u64 - nodes,
                    4096,
                    255,
                    4096,
                );
            }
            Err(e) => reply.error(e),
        }
    }
    fn symlink(&self, _: &Request, _: INodeNo, _: &OsStr, _: &Path, reply: ReplyEntry) {
        reply.error(Errno::EOPNOTSUPP)
    }
    fn readlink(&self, _: &Request, _: INodeNo, reply: ReplyData) {
        reply.error(Errno::EOPNOTSUPP)
    }
}
fn timestamp(t: Option<TimeOrNow>) -> backend::Result<rustix::fs::Timespec> {
    use rustix::fs::{Timespec, UTIME_NOW, UTIME_OMIT};
    match t {
        None => Ok(Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        }),
        Some(TimeOrNow::Now) => Ok(Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW,
        }),
        Some(TimeOrNow::SpecificTime(t)) => {
            let d = t
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Errno::EINVAL)?;
            Ok(Timespec {
                tv_sec: d.as_secs().try_into().map_err(|_| Errno::EOVERFLOW)?,
                tv_nsec: d.subsec_nanos().into(),
            })
        }
    }
}
