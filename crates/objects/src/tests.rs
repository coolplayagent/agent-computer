use super::*;
use std::os::unix::fs::PermissionsExt;

fn object(bytes: &[u8]) -> ObjectRef {
    ObjectRef {
        store_digest: sha256(b"store"),
        key: format!("execution-outputs/v1/org/exec/{}", &sha256(bytes)[7..]),
        sha256: sha256(bytes),
        size: bytes.len() as u64,
    }
}
#[test]
fn object_references_require_exact_namespace_and_digest() {
    let v = object(b"data");
    v.verify(b"data").unwrap();
    assert_eq!(v.verify(b"wrong"), Err(Error::Integrity));
    for key in [
        "../escape",
        "execution-outputs/v1/org/../evil",
        "execution-outputs/v1/org/exec/prefix",
    ]
    .iter()
    {
        let mut bad = v.clone();
        bad.key = key.to_string();
        assert!(bad.validate().is_err());
    }
    let mut bad = v;
    bad.key.push('a');
    assert!(bad.validate().is_err());
}
#[test]
fn durable_spool_reopens_retries_and_rejects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let binding = sha256(b"manifest");
    let o = object(b"bytes");
    let spool = Spool::open(dir.path()).unwrap();
    spool
        .record(&binding, &[(o.clone(), b"bytes".to_vec())])
        .unwrap();
    drop(spool);
    let spool = Spool::open(dir.path()).unwrap();
    assert_eq!(spool.read(&binding, &o).unwrap(), b"bytes");
    spool
        .record(&binding, &[(o.clone(), b"bytes".to_vec())])
        .unwrap();
    let file = dir
        .path()
        .join(format!("output-{}", &binding[7..]))
        .join(&o.sha256[7..]);
    std::fs::write(&file, b"other").unwrap();
    assert_eq!(spool.read(&binding, &o), Err(Error::Integrity));
    assert_eq!(
        spool.record(&binding, &[(o, b"bytes".to_vec())]),
        Err(Error::Integrity)
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
#[test]
fn spool_refuses_nonprivate_paths_symlinks_and_hardlinks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spool");
    std::fs::create_dir(&path).unwrap();
    assert!(Spool::open(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(Spool::open(&link).is_err());
    let spool = Spool::open(&path).unwrap();
    let binding = sha256(b"manifest");
    let o = object(b"bytes");
    spool
        .record(&binding, &[(o.clone(), b"bytes".to_vec())])
        .unwrap();
    let file = path
        .join(format!("output-{}", &binding[7..]))
        .join(&o.sha256[7..]);
    std::fs::hard_link(&file, dir.path().join("hardlink")).unwrap();
    assert_eq!(spool.read(&binding, &o), Err(Error::Configuration));
}
