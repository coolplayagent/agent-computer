use super::*;
use fuser::{Errno, OpenFlags};
use std::{ffi::OsStr, os::unix::fs::MetadataExt, sync::mpsc, time::Duration};
fn fixture() -> (tempfile::TempDir, Arc<Gate>) {
    let dir = tempfile::tempdir().unwrap();
    let file = File::open(dir.path()).unwrap();
    let m = file.metadata().unwrap();
    let prepared = Prepared {
        version: 1,
        request_digest: format!("sha256:{}", "a".repeat(64)),
        filesystem_uuid: "fixture".into(),
        volume_uid: "fixture".into(),
        path_ref: "fixture/data".into(),
        data_inode: m.ino(),
        manifest_digest: format!("sha256:{}", "b".repeat(64)),
        quota_bytes: 10 << 30,
    };
    (
        dir,
        Arc::new(Gate {
            closed: AtomicBool::new(false),
            active_mutation: AtomicBool::new(false),
            state: Mutex::new(State::new(file, m.uid(), m.gid()).unwrap()),
            prepared,
            instance: "c".repeat(64),
        }),
    )
}
fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}
fn create(g: &Gate, name: &str) -> (u64, u64) {
    g.access(uid(), true, |s| {
        s.create(1, OsStr::new(name), 0o600, OpenFlags(2))
    })
    .map(|(a, h)| (a.ino.0, h.0))
    .unwrap()
}
#[test]
fn sealed_open_and_unlinked_handles_cannot_modify_the_candidate() {
    let (dir, g) = fixture();
    let (ino, fh) = create(&g, "before");
    g.access(uid(), true, |s| s.write(ino, fh, 0, b"persisted"))
        .unwrap();
    g.access(uid(), true, |s| {
        s.rename(1, OsStr::new("before"), 1, OsStr::new("after"), 0)
    })
    .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("after")).unwrap(),
        b"persisted"
    );
    g.access(uid(), true, |s| s.remove(1, OsStr::new("after"), false))
        .unwrap();
    let sealed = g.seal().unwrap();
    assert!(sealed.is_sealed());
    assert_eq!(
        g.access(uid(), true, |s| s.write(ino, fh, 0, b"late")),
        Err(Errno::EROFS)
    );
    assert_eq!(
        g.access(uid(), true, |s| s.setattr(ino, None, Some(0), None)),
        Err(Errno::EROFS)
    );
    assert_eq!(
        g.access(uid(), true, |s| s.mkdir(1, OsStr::new("late"), 0o700)),
        Err(Errno::EROFS)
    );
    assert_eq!(
        g.access(uid(), false, |s| s.read(ino, fh, 0, 20)).unwrap(),
        b"persisted"
    );
    g.access(uid(), false, |s| s.release(ino, fh)).unwrap();
    assert!(sealed.is_sealed());
    assert_eq!(
        g.seal().unwrap().evidence().accepted_mutating_requests,
        sealed.evidence().accepted_mutating_requests
    );
}
#[test]
fn barrier_joins_inflight_io_and_rejects_a_queued_writer_before_acknowledging() {
    let (dir, g) = fixture();
    let (ino, fh) = create(&g, "file");
    let (entered_rx, entered) = mpsc::channel();
    let (resume, wait) = mpsc::channel();
    let writer = g.clone();
    let worker = std::thread::spawn(move || {
        writer
            .access(uid(), true, |s| {
                entered_rx.send(()).unwrap();
                wait.recv().unwrap();
                s.write(ino, fh, 0, b"joined")
            })
            .unwrap()
    });
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    let sealer = g.clone();
    let (done, result) = mpsc::channel();
    let seal = std::thread::spawn(move || done.send(sealer.seal().unwrap()).unwrap());
    while !g.closed.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    resume.send(()).unwrap();
    worker.join().unwrap();
    let proof = result.recv_timeout(Duration::from_secs(2)).unwrap();
    seal.join().unwrap();
    assert!(proof.is_sealed());
    assert_eq!(std::fs::read(dir.path().join("file")).unwrap(), b"joined");
    assert_eq!(
        g.access(uid(), true, |s| s.write(ino, fh, 0, b"queued")),
        Err(Errno::EROFS)
    );
}
#[test]
fn persistence_failure_cannot_produce_or_revive_a_barrier() {
    let (_dir, g) = fixture();
    let (ino, fh) = create(&g, "file");
    // A wrong backend description forces an actual fsync error; no boolean input
    // supplied by a workload can claim that this syscall succeeded.
    g.state.lock().unwrap().handles.get_mut(&fh).unwrap().file = File::open("/dev/null").unwrap();
    assert!(g.access(uid(), false, |s| s.sync_handle(ino, fh)).is_err());
    assert!(g.seal().is_err());
    assert!(g.seal().is_err());
    assert_eq!(
        g.access(uid(), true, |s| s.mkdir(1, OsStr::new("late"), 0o700)),
        Err(Errno::EROFS)
    );
}
#[test]
fn path_escape_links_devices_foreign_identity_and_privileged_modes_are_rejected() {
    let (dir, g) = fixture();
    for n in ["..", ".", "/tmp", "a/b", ".agent-computer-write-x"] {
        assert!(
            g.access(uid(), false, |s| s.lookup(1, OsStr::new(n)))
                .is_err()
        );
    }
    std::os::unix::fs::symlink("/etc/passwd", dir.path().join("link")).unwrap();
    let link = g
        .access(uid(), false, |s| s.lookup(1, OsStr::new("link")))
        .unwrap();
    assert!(
        g.access(uid(), false, |s| s.open(link, OpenFlags(0)))
            .is_err()
    );
    let (ino, _) = create(&g, "file");
    std::fs::hard_link(dir.path().join("file"), dir.path().join("hard")).unwrap();
    assert!(
        g.access(uid(), false, |s| s.lookup(1, OsStr::new("hard")))
            .is_err()
    );
    assert!(
        g.access(uid(), true, |s| s.setattr(ino, Some(0o4755), None, None))
            .is_err()
    );
    let foreign = if uid() == 12345 { 12346 } else { 12345 };
    assert_eq!(g.access(foreign, false, |s| s.attr(1)), Err(Errno::EACCES));
}
#[test]
fn rejected_namespace_operations_do_not_poison_successful_storage() {
    let (_dir, g) = fixture();
    let (_, fh) = create(&g, "file");
    assert_eq!(
        g.access(uid(), true, |s| s.mkdir(1, OsStr::new("file"), 0o700)),
        Err(Errno::EEXIST)
    );
    assert_eq!(
        g.access(uid(), true, |s| s.remove(1, OsStr::new("absent"), false)),
        Err(Errno::ENOENT)
    );
    assert!(g.access(uid(), false, |s| s.read(1, fh, 0, 10)).is_err());
    assert!(g.seal().unwrap().is_sealed());
}
#[test]
fn pinned_inode_survives_path_replacement_without_following_a_new_target() {
    let (dir, g) = fixture();
    let (ino, fh) = create(&g, "file");
    g.access(uid(), true, |s| s.write(ino, fh, 0, b"first"))
        .unwrap();
    std::fs::rename(dir.path().join("file"), dir.path().join("old")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", dir.path().join("file")).unwrap();
    let (opened, _) = g
        .access(uid(), false, |s| s.open(ino, OpenFlags(0)))
        .unwrap();
    assert_eq!(
        g.access(uid(), false, |s| s.read(ino, opened.0, 0, 50))
            .unwrap(),
        b"first"
    );
}
