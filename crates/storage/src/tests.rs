use super::*;
use crate::quota::Quota;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Cursor,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tempfile::TempDir;

fn private_temp() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[derive(Default)]
struct RecordingQuota {
    calls: Mutex<Vec<(String, u64)>>,
    fail_at: Option<usize>,
}
impl Quota for RecordingQuota {
    fn ensure(&self, uuid: &str, path: &str, bytes: u64) -> Result<()> {
        assert_eq!(uuid, "test-fs");
        let mut calls = self.calls.lock().unwrap();
        calls.push((path.into(), bytes));
        if self.fail_at == Some(calls.len()) {
            return Err(Error::QuotaUnavailable);
        }
        Ok(())
    }
}
struct Bytes {
    data: Vec<u8>,
    reads: AtomicUsize,
}
impl ObjectSource for Bytes {
    fn open(&self, _: &str) -> Result<Box<dyn std::io::Read>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Cursor::new(self.data.clone())))
    }
}
fn source(bytes: &[u8]) -> Bytes {
    Bytes {
        data: bytes.into(),
        reads: AtomicUsize::new(0),
    }
}
fn request(bytes: &[u8]) -> PrepareRequest {
    let manifest = Manifest {
        entries: [
            (
                "src/main.sh".into(),
                Entry::File {
                    sha256: format!("sha256:{:x}", Sha256::digest(bytes)),
                    size: bytes.len() as u64,
                    executable: true,
                },
            ),
            ("empty".into(), Entry::Directory),
        ]
        .into(),
    };
    PrepareRequest {
        organization: "org-1".into(),
        volume_uid: "volume-1".into(),
        workspace: "ws-1".into(),
        candidate: "cand-1".into(),
        computer: "comp-1".into(),
        generation: 1,
        quota_bytes: 1 << 30,
        manifest_digest: manifest.digest().unwrap(),
        manifest,
    }
}
fn refresh(request: &mut PrepareRequest) {
    request.manifest_digest = request.manifest.digest().unwrap();
}
fn path(root: &Path, request: &PrepareRequest) -> std::path::PathBuf {
    root.join(request.path_ref())
}

#[test]
fn copies_independent_inodes_with_modes_and_private_receipt() {
    let root = private_temp();
    let cache = private_temp();
    let req = request(b"echo hello\n");
    let Entry::File { sha256, .. } = &req.manifest.entries["src/main.sh"] else {
        panic!()
    };
    fs::write(cache.path().join(&sha256[7..]), b"echo hello\n").unwrap();
    let volume = MountedVolume::local(root.path()).unwrap();
    let quota = RecordingQuota::default();
    let receipt = volume
        .prepare(&req, &ObjectCache::open(cache.path()).unwrap(), &quota)
        .unwrap();
    let file = path(root.path(), &req).join("src/main.sh");
    let metadata = fs::metadata(&file).unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"echo hello\n");
    assert_eq!(metadata.mode() & 0o777, 0o700);
    assert_eq!(metadata.nlink(), 1);
    assert_ne!(
        metadata.ino(),
        fs::metadata(cache.path().join(&sha256[7..])).unwrap().ino()
    );
    assert!(path(root.path(), &req).join("empty").is_dir());
    assert_eq!(
        receipt.data_inode,
        fs::metadata(path(root.path(), &req)).unwrap().ino()
    );
    assert!(!path(root.path(), &req).join("receipt.json").exists());
    assert_eq!(
        fs::metadata(
            path(root.path(), &req)
                .parent()
                .unwrap()
                .join("receipt.json")
        )
        .unwrap()
        .mode()
            & 0o777,
        0o400
    );
    assert_eq!(quota.calls.lock().unwrap().len(), 2);
    fs::write(&file, b"edited by writer").unwrap();
    assert_eq!(
        fs::read(cache.path().join(&sha256[7..])).unwrap(),
        b"echo hello\n"
    );
}

#[test]
fn observation_never_creates_missing_or_partial_generations_and_preserves_edits() {
    let root = private_temp();
    let volume = MountedVolume::local(root.path()).unwrap();
    let req = request(b"input");
    let quota = RecordingQuota::default();
    assert!(volume.observe_prepared(&req, &quota).unwrap().is_none());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    assert!(quota.calls.lock().unwrap().is_empty());
    let receipt = volume.prepare(&req, &source(b"input"), &quota).unwrap();
    fs::write(path(root.path(), &req).join("src/main.sh"), b"edited").unwrap();
    assert_eq!(
        volume.observe_prepared(&req, &quota).unwrap(),
        Some(receipt)
    );
    assert_eq!(
        fs::read(path(root.path(), &req).join("src/main.sh")).unwrap(),
        b"edited"
    );
    let mut next = req.clone();
    next.generation = 2;
    assert!(volume.observe_prepared(&next, &quota).unwrap().is_none());
    assert!(!path(root.path(), &next).exists());
    fs::create_dir_all(root.path().join(next.parent()).join("staging_partial/data")).unwrap();
    assert!(volume.observe_prepared(&next, &quota).unwrap().is_none());
    assert!(!path(root.path(), &next).exists());
}

#[test]
fn durable_binding_digest_rejects_unsafe_owner_path_and_manifest() {
    let req = request(b"input");
    assert!(req.binding_digest("../escape", 1000, 1000).is_err());
    assert!(req.binding_digest("volume", 0, 1000).is_err());
    assert!(req.binding_digest("volume", 1000, u32::MAX).is_err());
    let hash = req.binding_digest("volume", 1000, 1000).unwrap();
    assert_ne!(hash, req.binding_digest("volume", 1001, 1000).unwrap());
    assert_ne!(hash, req.binding_digest("other", 1000, 1000).unwrap());
    let mut changed = req;
    changed.manifest_digest = "wrong".into();
    assert!(changed.binding_digest("volume", 1000, 1000).is_err());
}
#[test]
fn retry_preserves_mutated_files_and_does_not_read_source() {
    let root = private_temp();
    let volume = MountedVolume::local(root.path()).unwrap();
    let req = request(b"old");
    let input = source(b"old");
    let quota = RecordingQuota::default();
    let receipt = volume.prepare(&req, &input, &quota).unwrap();
    fs::write(path(root.path(), &req).join("src/main.sh"), b"new").unwrap();
    assert_eq!(volume.prepare(&req, &input, &quota).unwrap(), receipt);
    assert_eq!(input.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(path(root.path(), &req).join("src/main.sh")).unwrap(),
        b"new"
    );
}
#[test]
fn candidate_and_generation_copies_are_independent() {
    let root = private_temp();
    let volume = MountedVolume::local(root.path()).unwrap();
    let first = request(b"base");
    let mut next = first.clone();
    next.generation = 2;
    let mut candidate = first.clone();
    candidate.candidate = "cand-2".into();
    for req in [&first, &next, &candidate] {
        volume
            .prepare(req, &source(b"base"), &RecordingQuota::default())
            .unwrap();
    }
    fs::write(path(root.path(), &first).join("src/main.sh"), b"changed").unwrap();
    for req in [&next, &candidate] {
        assert_eq!(
            fs::read(path(root.path(), req).join("src/main.sh")).unwrap(),
            b"base"
        );
    }
}
#[test]
fn retained_identity_rejects_changed_computer_manifest_quota_or_owner() {
    let root = private_temp();
    let volume = MountedVolume::local(root.path()).unwrap();
    let req = request(b"base");
    volume
        .prepare(&req, &source(b"base"), &RecordingQuota::default())
        .unwrap();
    let mut computer = req.clone();
    computer.computer = "comp-2".into();
    let mut quota = req.clone();
    quota.quota_bytes *= 2;
    let manifest = request(b"other");
    for changed in [computer, quota, manifest] {
        assert_eq!(
            volume.prepare(&changed, &source(b"other"), &RecordingQuota::default()),
            Err(Error::IdentityConflict)
        );
    }
    let volume = volume.with_test_owner(rustix::process::getuid().as_raw() + 1);
    assert_eq!(
        volume.prepare(&req, &source(b"base"), &RecordingQuota::default()),
        Err(Error::IdentityConflict)
    );
}
#[test]
fn invalid_paths_and_conflicting_file_parents_fail_before_side_effects() {
    for bad in [
        "../escape",
        "/root",
        "x/../z",
        "x//z",
        "x/./z",
        "x\\z",
        "x\0z",
        "x/",
    ] {
        let root = private_temp();
        let mut req = request(b"base");
        req.manifest.entries.insert(bad.into(), Entry::Directory);
        refresh(&mut req);
        assert_eq!(
            MountedVolume::local(root.path()).unwrap().prepare(
                &req,
                &source(b"base"),
                &RecordingQuota::default()
            ),
            Err(Error::InvalidRequest)
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
    let mut req = request(b"base");
    req.manifest
        .entries
        .insert("src/main.sh/child".into(), Entry::Directory);
    refresh(&mut req);
    assert_eq!(req.validate(), Err(Error::InvalidRequest));
}
#[test]
fn identity_bounds_manifest_digest_and_size_overflow_fail_closed() {
    let req = request(b"base");
    let mut bad = Vec::new();
    let mut r = req.clone();
    r.organization = "../outside".into();
    bad.push(r);
    let mut r = req.clone();
    r.generation = 0;
    bad.push(r);
    let mut r = req.clone();
    r.quota_bytes += 1;
    bad.push(r);
    let mut r = req.clone();
    r.manifest_digest = "sha256:wrong".into();
    bad.push(r);
    for size in [u64::MAX, 1 << 30] {
        let mut r = req.clone();
        let Entry::File { size: field, .. } = r.manifest.entries.get_mut("src/main.sh").unwrap()
        else {
            panic!()
        };
        *field = size;
        refresh(&mut r);
        bad.push(r);
    }
    for r in bad {
        assert_eq!(r.validate(), Err(Error::InvalidRequest));
    }
}
#[test]
fn corrupt_short_or_oversized_input_never_publishes() {
    for bytes in [b"bad!".as_slice(), b"bas", b"baseextra"] {
        let root = private_temp();
        let req = request(b"base");
        assert_eq!(
            MountedVolume::local(root.path()).unwrap().prepare(
                &req,
                &source(bytes),
                &RecordingQuota::default()
            ),
            Err(Error::InputMismatch)
        );
        assert!(!path(root.path(), &req).exists());
    }
}
#[test]
fn quota_failure_prevents_source_read_and_publication() {
    let root = private_temp();
    let req = request(b"base");
    let input = source(b"base");
    let quota = RecordingQuota {
        fail_at: Some(1),
        ..Default::default()
    };
    assert_eq!(
        MountedVolume::local(root.path())
            .unwrap()
            .prepare(&req, &input, &quota),
        Err(Error::QuotaUnavailable)
    );
    assert_eq!(input.reads.load(Ordering::SeqCst), 0);
    assert!(!path(root.path(), &req).exists());
}
#[test]
fn final_quota_failure_returns_no_success_and_retry_recovers_same_inode() {
    let root = private_temp();
    let req = request(b"base");
    let input = source(b"base");
    let volume = MountedVolume::local(root.path()).unwrap();
    assert_eq!(
        volume.prepare(
            &req,
            &input,
            &RecordingQuota {
                fail_at: Some(2),
                ..Default::default()
            }
        ),
        Err(Error::QuotaUnavailable)
    );
    let inode = fs::metadata(path(root.path(), &req)).unwrap().ino();
    assert_eq!(
        volume
            .prepare(&req, &input, &RecordingQuota::default())
            .unwrap()
            .data_inode,
        inode
    );
    assert_eq!(input.reads.load(Ordering::SeqCst), 1);
}
#[test]
fn preexisting_symlink_cannot_escape_volume() {
    let root = private_temp();
    let outside = private_temp();
    symlink(outside.path(), root.path().join("organization")).unwrap();
    assert_eq!(
        MountedVolume::local(root.path()).unwrap().prepare(
            &request(b"base"),
            &source(b"base"),
            &RecordingQuota::default()
        ),
        Err(Error::InvalidFilesystemObject)
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
#[test]
fn source_cache_rejects_symlink_hardlink_fifo_and_directory() {
    for kind in ["symlink", "hardlink", "fifo", "directory"] {
        let root = private_temp();
        let cache = private_temp();
        let req = request(b"base");
        let Entry::File { sha256, .. } = &req.manifest.entries["src/main.sh"] else {
            panic!()
        };
        let object = cache.path().join(&sha256[7..]);
        let target = cache.path().join("target");
        fs::write(&target, b"base").unwrap();
        match kind {
            "symlink" => symlink(&target, &object).unwrap(),
            "hardlink" => fs::hard_link(&target, &object).unwrap(),
            "fifo" => {
                rustix::fs::mkfifoat(rustix::fs::CWD, &object, rustix::fs::Mode::RUSR).unwrap()
            }
            _ => fs::create_dir(&object).unwrap(),
        }
        assert_eq!(
            MountedVolume::local(root.path()).unwrap().prepare(
                &req,
                &ObjectCache::open(cache.path()).unwrap(),
                &RecordingQuota::default()
            ),
            Err(Error::InvalidFilesystemObject)
        );
        assert!(!path(root.path(), &req).exists());
    }
}
#[test]
fn replaced_data_inode_is_not_accepted_as_existing_preparation() {
    let root = private_temp();
    let req = request(b"base");
    let volume = MountedVolume::local(root.path()).unwrap();
    volume
        .prepare(&req, &source(b"base"), &RecordingQuota::default())
        .unwrap();
    let data = path(root.path(), &req);
    fs::rename(&data, data.with_file_name("retained-data")).unwrap();
    fs::create_dir(&data).unwrap();
    assert_eq!(
        volume.prepare(&req, &source(b"base"), &RecordingQuota::default()),
        Err(Error::IdentityConflict)
    );
}
#[test]
fn concurrent_identical_publishers_return_one_data_inode() {
    let root = private_temp();
    let volume = Arc::new(MountedVolume::local(root.path()).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let volume = volume.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                volume
                    .prepare(
                        &request(b"base"),
                        &source(b"base"),
                        &RecordingQuota::default(),
                    )
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(results.iter().all(|v| v == &results[0]));
}
#[test]
fn concurrent_conflicting_publishers_never_overwrite_winner() {
    let root = private_temp();
    let volume = Arc::new(MountedVolume::local(root.path()).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [b"first".as_slice(), b"second"]
        .into_iter()
        .map(|bytes| {
            let volume = volume.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                volume.prepare(&request(bytes), &source(bytes), &RecordingQuota::default())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err(Error::IdentityConflict))
            .count(),
        1
    );
}
#[test]
fn local_filesystem_cannot_use_production_backend_constructor() {
    let root = private_temp();
    assert!(matches!(
        MountedVolume::open(root.path(), "volume", "test-fs", "volume-1", 1000, 1000),
        Err(Error::UnsupportedBackend)
    ));
}
#[test]
fn mount_requires_matching_uuid_and_both_writeback_layers_disabled() {
    let config = serde_json::json!({"Version":"1.4.1", "Format":{"UUID":"test-fs","Storage":"s3"},"Chunk":{"Writeback":false},"FuseOpts":{"EnableWriteback":false,"Options":[]},"Meta":{"ReadOnly":false}});
    assert_eq!(materialize::validate_mount(&config, "test-fs"), Ok(()));
    for (pointer, value) in [
        ("/Format/UUID", serde_json::json!("other")),
        ("/Chunk/Writeback", serde_json::json!(true)),
        ("/FuseOpts/EnableWriteback", serde_json::json!(true)),
        ("/FuseOpts/Options", serde_json::json!(["writeback_cache"])),
        ("/Meta/ReadOnly", serde_json::json!(true)),
    ] {
        let mut bad = config.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            materialize::validate_mount(&bad, "test-fs"),
            Err(Error::UnsupportedBackend)
        );
    }
    let mut subdir = config.clone();
    subdir["Subdir"] = serde_json::json!("/other");
    assert_eq!(
        materialize::validate_mount(&subdir, "test-fs"),
        Err(Error::UnsupportedBackend)
    );
    assert_eq!(
        materialize::validate_mount(&serde_json::json!({}), "test-fs"),
        Err(Error::UnsupportedBackend)
    );
}
#[test]
fn operator_inputs_require_private_single_link_regular_files() {
    let dir = private_temp();
    let file = dir.path().join("config");
    fs::write(&file, b"{}").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(quota::read_private(&file, 10).unwrap(), b"{}");
    assert_eq!(quota::read_private(&file, 1), Err(Error::InvalidRequest));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(quota::read_private(&file, 10), Err(Error::InvalidRequest));
    let link = dir.path().join("link");
    symlink(&file, &link).unwrap();
    assert_eq!(
        quota::read_private(&link, 10),
        Err(Error::InvalidFilesystemObject)
    );
}

fn quota_fixture(script: &str) -> (TempDir, quota::JuiceFsConfig) {
    let root = private_temp();
    let executable = root.path().join("juicefs");
    let password = root.path().join("password");
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&password, "private-test-password").unwrap();
    fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
    let config = quota::JuiceFsConfig {
        executable,
        executable_sha256: format!("sha256:{:x}", Sha256::digest(script)),
        metadata_url: "postgres://metadata@127.0.0.1:5432/juicefs?sslmode=disable".into(),
        password_file: password,
        timeout_seconds: 1,
    };
    (root, config)
}
#[test]
fn quota_adapter_binds_metadata_uuid_and_exact_capacity_arguments() {
    let script = "#!/bin/sh\nif [ \"$1\" = status ]; then printf '%s' '{\"Setting\":{\"UUID\":\"test-fs\"}}'; exit 0; fi\n[ \"$1\" = quota ] && [ \"$2\" = set ] && [ \"$3\" = --path ] && [ \"$4\" = /volume/directory ] && [ \"$5\" = --capacity ] && [ \"$6\" = 2 ] && [ \"$7\" = 'postgres://metadata@127.0.0.1:5432/juicefs?sslmode=disable' ] && [ \"$#\" = 7 ] && [ -f \"$META_PASSWORD_FILE\" ]\n";
    let (_root, config) = quota_fixture(script);
    let quota = quota::JuiceFsQuota::new(config).unwrap();
    assert_eq!(
        quota.ensure("test-fs", "/volume/directory", 2 << 30),
        Ok(())
    );
    assert_eq!(
        quota.ensure("other-fs", "/volume/directory", 2 << 30),
        Err(Error::IdentityConflict)
    );
    assert_eq!(
        quota.ensure("test-fs", "/volume/directory", 1 << 30),
        Err(Error::QuotaUnavailable)
    );
}
#[test]
fn quota_adapter_propagates_backend_failure_and_timeout_without_raw_output() {
    for script in [
        "#!/bin/sh\necho private-test-password >&2\nexit 1\n",
        "#!/bin/sh\n/bin/sleep 10\n",
        "#!/bin/sh\n/bin/sleep 10 &\nexit 0\n",
        "#!/bin/sh\nhead -c 1048577 /dev/zero\n",
    ] {
        let (_root, config) = quota_fixture(script);
        let quota = quota::JuiceFsQuota::new(config).unwrap();
        let start = std::time::Instant::now();
        let error = quota
            .ensure("test-fs", "/volume/directory", 1 << 30)
            .unwrap_err();
        assert_eq!(error, Error::QuotaUnavailable);
        assert!(!error.to_string().contains("private-test-password"));
        assert!(start.elapsed() < std::time::Duration::from_secs(4));
    }
}
#[test]
fn quota_config_rejects_credentials_unknown_query_and_unpinned_executable() {
    for url in [
        "postgres://metadata:secret@host/db?sslmode=require",
        "postgres://metadata@host/db?password=secret",
        "postgres://metadata@host/db?sslmode=require&sslmode=disable",
        "redis://metadata@host/db?sslmode=disable",
    ] {
        let (_root, mut config) = quota_fixture("#!/bin/sh\nexit 0\n");
        config.metadata_url = url.into();
        assert!(matches!(
            quota::JuiceFsQuota::new(config),
            Err(Error::InvalidRequest)
        ));
    }
    let (_root, mut config) = quota_fixture("#!/bin/sh\nexit 0\n");
    config.executable_sha256 = format!("sha256:{}", "0".repeat(64));
    assert!(matches!(
        quota::JuiceFsQuota::new(config),
        Err(Error::InputMismatch)
    ));
}

#[test]
fn shared_writable_control_directory_is_rejected() {
    let root = private_temp();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        MountedVolume::local(root.path()).unwrap().prepare(
            &request(b"base"),
            &source(b"base"),
            &RecordingQuota::default()
        ),
        Err(Error::InvalidFilesystemObject)
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn manifest_digest_matches_independent_canonical_json_vector() {
    let manifest: Manifest = serde_json::from_str(r#"{"entries":{"z":{"size":0,"kind":"file","executable":false,"sha256":"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},"a":{"kind":"directory"}}}"#).unwrap();
    // Independently computed with Python's sorted compact JSON plus the domain and NUL.
    assert_eq!(
        manifest.digest().unwrap(),
        "sha256:3f22170f4a95fbc35c21a70a7b0ef9c3b5e7e4921de0b77331d1c1dfc31e43b9"
    );
}

mod capture_tests;
