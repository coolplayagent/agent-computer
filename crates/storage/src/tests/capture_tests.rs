use super::*;
use crate::capture::{CHUNK_BYTES, CaptureSink, Chunk};
use std::collections::BTreeMap;
#[derive(Default)]
struct Sink(BTreeMap<String, Vec<u8>>);
impl CaptureSink for Sink {
    fn record(&mut self, chunk: &Chunk, bytes: &[u8]) -> Result<()> {
        self.0.insert(chunk.sha256.clone(), bytes.to_vec());
        Ok(())
    }
}
#[test]
fn capture_chunks_large_files_and_restores_independent_verified_input() {
    let root = private_temp();
    let cache = private_temp();
    let volume = MountedVolume::local(root.path()).unwrap();
    let bytes = vec![71; CHUNK_BYTES * 2 + 37];
    let req = request(&bytes);
    let prepared = volume
        .prepare(&req, &source(&bytes), &RecordingQuota::default())
        .unwrap();
    let mut sink = Sink::default();
    let captured = volume.capture(&prepared, &mut sink).unwrap();
    assert_eq!(captured.chunks()["src/main.sh"].len(), 3);
    assert_eq!(
        captured.manifest().entries["src/main.sh"],
        req.manifest.entries["src/main.sh"]
    );
    assert_eq!(captured.manifest().entries["empty"], Entry::Directory);
    let object_cache = ObjectCache::open(cache.path()).unwrap();
    let Entry::File { sha256, size, .. } = &req.manifest.entries["src/main.sh"] else {
        panic!()
    };
    let mut writer = object_cache.begin_file(sha256, *size).unwrap();
    for c in &captured.chunks()["src/main.sh"] {
        use std::io::Write;
        writer.write_all(&sink.0[&c.sha256]).unwrap();
    }
    writer.finish().unwrap();
    let mut next = req.clone();
    next.candidate = "next-candidate".into();
    next.generation = 2;
    next.manifest = captured.manifest().clone();
    refresh(&mut next);
    let restored = volume
        .prepare(&next, &object_cache, &RecordingQuota::default())
        .unwrap();
    assert_eq!(
        fs::read(root.path().join(&restored.path_ref).join("src/main.sh")).unwrap(),
        bytes
    );
    assert_ne!(prepared.data_inode, restored.data_inode);
    assert_ne!(
        fs::metadata(path(root.path(), &req).join("src/main.sh"))
            .unwrap()
            .ino(),
        fs::metadata(path(root.path(), &next).join("src/main.sh"))
            .unwrap()
            .ino()
    );
}
#[test]
fn capture_rejects_symlinks_hardlinks_fifos_and_private_staging() {
    for kind in ["symlink", "hardlink", "fifo", "staging"] {
        let root = private_temp();
        let volume = MountedVolume::local(root.path()).unwrap();
        let req = request(b"safe");
        let p = volume
            .prepare(&req, &source(b"safe"), &RecordingQuota::default())
            .unwrap();
        let data = path(root.path(), &req);
        match kind {
            "symlink" => symlink("/etc/passwd", data.join("escape")).unwrap(),
            "hardlink" => fs::hard_link(data.join("src/main.sh"), data.join("alias")).unwrap(),
            "fifo" => rustix::fs::mknodat(
                rustix::fs::CWD,
                data.join("fifo"),
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::from_raw_mode(0o600),
                0,
            )
            .unwrap(),
            _ => fs::write(
                data.join(".agent-computer-write-leftover"),
                b"not-an-artifact",
            )
            .unwrap(),
        }
        assert!(volume.capture(&p, &mut Sink::default()).is_err(), "{kind}");
    }
}
#[test]
fn capture_observed_mutation_and_sink_failure_do_not_produce_a_receipt() {
    let root = private_temp();
    let volume = MountedVolume::local(root.path()).unwrap();
    let bytes = vec![12; CHUNK_BYTES + 3];
    let req = request(&bytes);
    let p = volume
        .prepare(&req, &source(&bytes), &RecordingQuota::default())
        .unwrap();
    struct Mutate(std::path::PathBuf);
    impl CaptureSink for Mutate {
        fn record(&mut self, _: &Chunk, _: &[u8]) -> Result<()> {
            fs::write(&self.0, b"changed")?;
            Ok(())
        }
    }
    assert!(
        volume
            .capture(&p, &mut Mutate(path(root.path(), &req).join("src/main.sh")))
            .is_err()
    );
    struct Fail;
    impl CaptureSink for Fail {
        fn record(&mut self, _: &Chunk, _: &[u8]) -> Result<()> {
            Err(Error::Io)
        }
    }
    assert!(volume.capture(&p, &mut Fail).is_err());
}
#[test]
fn cache_rejects_corrupt_incomplete_and_replaced_objects() {
    use std::io::Write;
    let root = private_temp();
    let cache = ObjectCache::open(root.path()).unwrap();
    let hash = format!("sha256:{:x}", Sha256::digest(b"okay"));
    let mut writer = cache.begin_file(&hash, 4).unwrap();
    writer.write_all(b"bad!").unwrap();
    assert!(writer.finish().is_err());
    let mut writer = cache.begin_file(&hash, 4).unwrap();
    writer.write_all(b"ok").unwrap();
    assert!(writer.finish().is_err());
    assert!(!root.path().join(&hash[7..]).exists());
    fs::write(root.path().join(&hash[7..]), b"evil").unwrap();
    let mut writer = cache.begin_file(&hash, 4).unwrap();
    writer.write_all(b"okay").unwrap();
    assert!(writer.finish().is_err());
    assert_eq!(fs::read(root.path().join(&hash[7..])).unwrap(), b"evil");
}
