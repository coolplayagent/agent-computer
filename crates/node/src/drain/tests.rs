use super::*;
use serde_json::json;
use std::os::unix::fs::{PermissionsExt, symlink};

fn seal() -> Value {
    let prepared = json!({"data_inode":42});
    json!({"version":1,"arm":{"observed_boottime_ms":10,"runtime":{"workspace_mount":{"instance":"a".repeat(64),"prepared":prepared}}},"io":{"version":1,"instance":"a".repeat(64),"prepared":prepared,"accepted_mutating_requests":3},"domain":"empty","observed_boottime_ms":20})
}
fn fixture() -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    let file = File::open(dir.path()).unwrap();
    (dir, file)
}

#[test]
fn immutable_drain_receipt_survives_reopen_and_pinned_directory_rename() {
    let (dir, fd) = fixture();
    let s = seal();
    record(&fd, s.clone()).unwrap();
    record(&fd, s.clone()).unwrap();
    let new = dir.path().join("moved");
    std::fs::create_dir(&new).unwrap();
    let reopened = File::open(dir.path()).unwrap();
    assert_eq!(read(&reopened, &s["arm"]).unwrap().unwrap().evidence(), &s);
    let other = tempfile::tempdir().unwrap();
    let moved = other.path().join("original");
    std::fs::rename(dir.path(), &moved).unwrap();
    assert_eq!(read(&fd, &s["arm"]).unwrap().unwrap().evidence(), &s);
    let mut changed = s.clone();
    changed["domain"] = json!("removed");
    assert!(record(&fd, changed).is_err());
    assert_eq!(read(&fd, &s["arm"]).unwrap().unwrap().evidence(), &s);
    assert_eq!(std::fs::read_dir(moved).unwrap().count(), 2);
}

#[test]
fn partial_corrupt_and_foreign_drain_receipts_never_become_proof() {
    let (dir, fd) = fixture();
    let s = seal();
    std::fs::write(dir.path().join("drain-crashed.pending"), b"{").unwrap();
    assert!(read(&fd, &s["arm"]).unwrap().is_none());
    record(&fd, s.clone()).unwrap();
    let path = dir.path().join(name(&s["arm"]).unwrap());
    let bytes = std::fs::read(&path).unwrap();
    let mut record: Value = serde_json::from_slice(&bytes).unwrap();
    record["seal"]["domain"] = json!("removed");
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(read(&fd, &s["arm"]).is_err());
    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    assert!(read(&fd, &s["arm"]).is_err());
    let mut foreign = s["arm"].clone();
    foreign["runtime"]["workspace_mount"]["instance"] = json!("b".repeat(64));
    std::fs::write(&path, &bytes).unwrap();
    std::fs::rename(&path, dir.path().join(name(&foreign).unwrap())).unwrap();
    assert!(read(&fd, &foreign).is_err());
}

#[test]
fn drain_reader_rejects_links_devices_oversize_and_public_permissions() {
    let (dir, fd) = fixture();
    let s = seal();
    record(&fd, s.clone()).unwrap();
    let path = dir.path().join(name(&s["arm"]).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(read(&fd, &s["arm"]).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let alias = dir.path().join("alias");
    std::fs::hard_link(&path, &alias).unwrap();
    assert!(read(&fd, &s["arm"]).is_err());
    std::fs::remove_file(&path).unwrap();
    symlink(&alias, &path).unwrap();
    assert!(read(&fd, &s["arm"]).is_err());
    std::fs::remove_file(&path).unwrap();
    fs::mknodat(
        &fd,
        name(&s["arm"]).unwrap(),
        fs::FileType::Fifo,
        Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    assert!(read(&fd, &s["arm"]).is_err());
    std::fs::remove_file(&path).unwrap();
    let file = File::create(&path).unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
    file.set_len(MAX_BYTES as u64 + 1).unwrap();
    assert!(read(&fd, &s["arm"]).is_err());
}

#[test]
fn drain_proof_requires_completed_process_and_matching_io_barrier() {
    let (_dir, fd) = fixture();
    for (pointer, value) in [
        ("/version", json!(2)),
        ("/domain", json!("path_absent")),
        ("/io/version", json!(2)),
        ("/io/instance", json!("b".repeat(64))),
        ("/io/prepared/data_inode", json!(43)),
        ("/observed_boottime_ms", json!(9)),
    ] {
        let mut s = seal();
        *s.pointer_mut(pointer).unwrap() = value;
        assert!(record(&fd, s).is_err(), "{pointer}");
    }
    assert!(read(&fd, &seal()["arm"]).unwrap().is_none());
}

#[test]
fn concurrent_drain_publications_preserve_one_identical_immutable_receipt() {
    let (dir, fd) = fixture();
    let a = fd.try_clone().unwrap();
    let b = fd.try_clone().unwrap();
    let first = std::thread::spawn(move || record(&a, seal()));
    let second = std::thread::spawn(move || record(&b, seal()));
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    assert_eq!(
        read(&fd, &seal()["arm"]).unwrap().unwrap().evidence(),
        &seal()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn public_drain_recovery_requires_operator_owned_ancestors() {
    let (dir, fd) = fixture();
    record(&fd, seal()).unwrap();
    // Even a correctly shaped private file below the untrusted test directory
    // cannot be imported as node evidence through the public API.
    assert!(read_recorded_seal(dir.path(), &seal()["arm"]).is_err());
}
