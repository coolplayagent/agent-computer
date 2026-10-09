use super::*;
use crate::{Manifest, ObjectCache, PrepareRequest, quota::Quota};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    time::Duration,
};

struct QuotaOk;
impl Quota for QuotaOk {
    fn ensure(&self, _: &str, _: &str, _: u64) -> Result<()> {
        Ok(())
    }
}
fn setup() -> (tempfile::TempDir, MountedVolume, Prepared) {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mount = MountedVolume::local(root.path()).unwrap();
    let manifest = Manifest::default();
    let request = PrepareRequest {
        organization: "org".into(),
        volume_uid: "volume-1".into(),
        workspace: "workspace".into(),
        candidate: "candidate".into(),
        computer: "computer".into(),
        generation: 1,
        quota_bytes: 1 << 30,
        manifest_digest: manifest.digest().unwrap(),
        manifest,
    };
    let receipt = mount
        .prepare(&request, &ObjectCache::open(root.path()).unwrap(), &QuotaOk)
        .unwrap();
    (root, mount, receipt)
}
fn edit(path: &str, bytes: &[u8]) -> FileEdit {
    FileEdit {
        path: path.into(),
        expected: None,
        content: bytes.into(),
        executable: false,
    }
}
fn run(mount: &MountedVolume, prepared: &Prepared, edit: &FileEdit) -> ClosedFileEdit {
    mount
        .edit_file(prepared, edit, Instant::now() + Duration::from_secs(30))
        .unwrap()
}

#[test]
fn atomic_create_replace_and_conflict_preserve_old_open_inode() {
    let (root, mount, prepared) = setup();
    let first = run(&mount, &prepared, &edit("hello.txt", b"original"));
    assert_eq!(first.report().state, FileEditState::Applied);
    assert!(first.report().drain_confirmed);
    let path = root.path().join(&prepared.path_ref).join("hello.txt");
    let old = fs::File::open(&path).unwrap();
    let mut next = edit("hello.txt", b"changed");
    next.expected = first.report().version.clone();
    next.executable = true;
    let result = run(&mount, &prepared, &next);
    assert_eq!(result.report().state, FileEditState::Applied);
    assert!(result.report().version.as_ref().unwrap().executable);
    assert_ne!(
        old.metadata().unwrap().ino(),
        fs::metadata(&path).unwrap().ino()
    );
    use std::io::Read;
    let mut original = Vec::new();
    (&old).read_to_end(&mut original).unwrap();
    assert_eq!(original, b"original");
    assert_eq!(fs::read(&path).unwrap(), b"changed");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let conflict = run(&mount, &prepared, &next);
    assert_eq!(conflict.report().state, FileEditState::Conflict);
    assert!(conflict.report().drain_confirmed);
    assert_eq!(conflict.report().version, result.report().version);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn binary_unicode_and_empty_files_have_exact_hashes() {
    let (root, mount, prepared) = setup();
    for (name, bytes) in [
        ("资料.bin", vec![0, 255, 128, 10]),
        ("empty", vec![]),
        ("limit", vec![42; MAX_FILE_BYTES]),
    ] {
        let result = run(&mount, &prepared, &edit(name, &bytes));
        assert_eq!(result.report().state, FileEditState::Applied);
        let version = result.report().version.as_ref().unwrap();
        assert_eq!(
            version.sha256,
            format!("sha256:{:x}", Sha256::digest(&bytes))
        );
        assert_eq!(version.size, bytes.len() as u64);
        assert_eq!(
            fs::read(root.path().join(&prepared.path_ref).join(name)).unwrap(),
            bytes
        );
    }
}

#[test]
fn invalid_paths_and_oversize_fail_before_any_effect() {
    let (root, mount, prepared) = setup();
    for name in [
        "../escape",
        "/escape",
        "a//b",
        "a/../b",
        "a\\b",
        "a\0b",
        "",
        ".agent-computer-write-forged",
    ] {
        assert!(matches!(
            mount.edit_file(
                &prepared,
                &edit(name, b"bad"),
                Instant::now() + Duration::from_secs(30)
            ),
            Err(Error::InvalidRequest)
        ));
    }
    assert!(matches!(
        mount.edit_file(
            &prepared,
            &edit("huge", &vec![1; MAX_FILE_BYTES + 1]),
            Instant::now() + Duration::from_secs(30)
        ),
        Err(Error::InvalidRequest)
    ));
    assert_eq!(
        fs::read_dir(root.path().join(&prepared.path_ref))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn symlinks_hardlinks_fifo_and_directory_never_follow_or_overwrite() {
    for kind in ["symlink", "hardlink", "fifo", "directory"] {
        let (root, mount, prepared) = setup();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("keep");
        fs::write(&target, b"secret").unwrap();
        let path = root.path().join(&prepared.path_ref).join("bad");
        match kind {
            "symlink" => symlink(&target, &path).unwrap(),
            "hardlink" => fs::hard_link(&target, &path).unwrap(),
            "fifo" => rustix::fs::mkfifoat(rustix::fs::CWD, &path, rustix::fs::Mode::RUSR).unwrap(),
            _ => fs::create_dir(&path).unwrap(),
        }
        let result = run(&mount, &prepared, &edit("bad", b"replace"));
        assert_eq!(result.report().state, FileEditState::Unknown);
        assert!(!result.report().drain_confirmed);
        assert_eq!(std::fs::read(target).unwrap(), b"secret");
    }
}

#[test]
fn parent_symlink_and_replaced_candidate_identity_are_rejected() {
    let (root, mount, prepared) = setup();
    let outside = tempfile::tempdir().unwrap();
    let path = root.path().join(&prepared.path_ref);
    symlink(outside.path(), path.join("escape")).unwrap();
    assert_eq!(
        run(&mount, &prepared, &edit("escape/new", b"bad"))
            .report()
            .state,
        FileEditState::Unknown
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    fs::rename(&path, path.with_file_name("old-data")).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(
        run(&mount, &prepared, &edit("new", b"bad")).report().state,
        FileEditState::Unknown
    );
    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
}

#[test]
fn expired_before_mutation_yields_no_write_and_positive_drain() {
    let (root, mount, prepared) = setup();
    let result = mount
        .edit_file(&prepared, &edit("late", b"late"), Instant::now())
        .unwrap();
    assert_eq!(result.report().state, FileEditState::Expired);
    assert!(result.report().drain_confirmed);
    assert_eq!(
        fs::read_dir(root.path().join(prepared.path_ref))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn prepared_receipt_and_operation_digest_bind_every_file_intent_field() {
    let (_root, mount, prepared) = setup();
    let input = edit("a", b"one");
    let digest = input.digest(&prepared).unwrap();
    for changed in [
        FileEdit {
            path: "b".into(),
            ..input.clone()
        },
        FileEdit {
            content: b"two".to_vec(),
            ..input.clone()
        },
        FileEdit {
            executable: true,
            ..input.clone()
        },
    ] {
        assert_ne!(changed.digest(&prepared).unwrap(), digest);
    }
    let mut changed = prepared.clone();
    changed.data_inode += 1;
    assert_ne!(input.digest(&changed).unwrap(), digest);
    assert_eq!(
        run(&mount, &changed, &input).report().state,
        FileEditState::Unknown
    );
}

#[test]
fn bounded_reads_return_exact_binary_version_and_absence() {
    let (_root, mount, prepared) = setup();
    assert_eq!(mount.read_file(&prepared, "absent").unwrap(), None);
    for (path, bytes) in [
        ("资料", vec![0, 255, 128]),
        ("empty", vec![]),
        ("limit", vec![7; MAX_FILE_BYTES]),
    ] {
        let saved = run(&mount, &prepared, &edit(path, &bytes));
        let read = mount.read_file(&prepared, path).unwrap().unwrap();
        assert_eq!(read.content, bytes);
        assert_eq!(Some(read.version), saved.report().version);
    }
}
#[test]
fn reads_reject_unsafe_objects_staging_and_candidate_replacement() {
    let (root, mount, prepared) = setup();
    let data = root.path().join(&prepared.path_ref);
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"private").unwrap();
    symlink(outside.path(), data.join("escape")).unwrap();
    symlink(outside.path().join("secret"), data.join("link")).unwrap();
    fs::hard_link(outside.path().join("secret"), data.join("hard")).unwrap();
    rustix::fs::mkfifoat(rustix::fs::CWD, data.join("fifo"), rustix::fs::Mode::RUSR).unwrap();
    fs::write(data.join(".agent-computer-write-secret"), b"stage").unwrap();
    fs::write(data.join("large"), vec![1; MAX_FILE_BYTES + 1]).unwrap();
    for path in [
        "escape/secret",
        "link",
        "hard",
        "fifo",
        "large",
        ".agent-computer-write-secret",
        "../receipt.json",
    ] {
        assert!(mount.read_file(&prepared, path).is_err(), "{path}");
    }
    fs::rename(&data, data.with_file_name("old")).unwrap();
    fs::create_dir(&data).unwrap();
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(data.join("forged"), b"different inode").unwrap();
    assert!(mount.read_file(&prepared, "forged").is_err());
}
