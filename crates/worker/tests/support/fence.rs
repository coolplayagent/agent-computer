//! Real kernel FUSE + the qualified JuiceFS mount, including a stopped client.
//! This operator fixture does not assert database writer admission or Ready.
use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::Instant,
};

const WORKLOAD: &str = r#"
import errno,json,mmap,os,sys
os.chdir(sys.argv[1])
assert os.getuid()==1000 and os.getgroups()==[]
os.mkdir('fence-dir')
fd=os.open('fence-dir/held',os.O_CREAT|os.O_RDWR,0o600)
assert os.write(fd,b'before')==6
os.fsync(fd)
os.utime('fence-dir/held',ns=(1000000000,2000000000))
assert os.stat('fence-dir/held').st_mtime_ns==2000000000
os.rename('fence-dir/held','fence-dir/renamed')
orphan=os.open('fence-dir/orphan',os.O_CREAT|os.O_RDWR,0o600)
os.write(orphan,b'orphan')
os.unlink('fence-dir/orphan')
try:mmap.mmap(fd,6)
except OSError as e:assert e.errno in (errno.ENODEV,errno.ENOSYS,errno.EOPNOTSUPP,errno.EACCES)
else:raise AssertionError('writable mmap unexpectedly supported')
assert os.statvfs('.').f_blocks>0
print(json.dumps({'phase':'ready'}),flush=True)
assert sys.stdin.readline().strip()=='write'
os.lseek(fd,0,0)
assert os.write(fd,b'durable')==7
os.fsync(fd)
print(json.dumps({'phase':'persisted'}),flush=True)
assert sys.stdin.readline().strip()=='sealed'
denied=[]
def reject(name,call):
 try:call()
 except OSError as e:assert e.errno==errno.EROFS,(name,e);denied.append(name)
 else:raise AssertionError(name+' survived sealing')
reject('old-fd-write',lambda:os.write(fd,b'late'))
reject('unlinked-fd-write',lambda:os.write(orphan,b'late'))
reject('truncate',lambda:os.ftruncate(fd,0))
reject('new-file',lambda:os.open('late',os.O_CREAT|os.O_WRONLY,0o600))
reject('mkdir',lambda:os.mkdir('late-dir'))
reject('rename',lambda:os.rename('fence-dir/renamed','fence-dir/late'))
reject('unlink',lambda:os.unlink('fence-dir/renamed'))
reject('rmdir',lambda:os.rmdir('fence-dir'))
reject('chmod',lambda:os.chmod('fence-dir/renamed',0o400))
reject('utime',lambda:os.utime('fence-dir/renamed'))
assert os.pread(fd,99,0)==b'durable'
assert os.pread(orphan,99,0)==b'orphan'
assert os.listdir('fence-dir')==['renamed']
os.close(orphan);os.close(fd)
print(json.dumps({'phase':'done','uid':os.getuid(),'denied':denied,'bytes':'durable','mmap_rejected':True}),flush=True)
"#;

struct Workload {
    child: Child,
    input: ChildStdin,
    lines: Receiver<String>,
}
impl Workload {
    fn start(mount: &Path) -> Self {
        let mut child = Command::new("/usr/bin/setpriv")
            .args([
                "--reuid=1000",
                "--regid=1000",
                "--clear-groups",
                "/usr/bin/python3",
                "-c",
                WORKLOAD,
            ])
            .arg(mount)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            lines,
        }
    }
    fn phase(&self, phase: &str) -> Value {
        let value: Value =
            serde_json::from_str(&self.lines.recv_timeout(Duration::from_secs(60)).unwrap())
                .unwrap();
        assert_eq!(value["phase"], phase);
        value
    }
    fn send(&mut self, message: &str) {
        writeln!(self.input, "{message}").unwrap();
    }
}
impl Drop for Workload {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Paused(rustix::process::Pid);
impl Paused {
    fn new(mount: &Path) -> Self {
        let config: Value =
            serde_json::from_slice(&fs::read(mount.join(".config")).unwrap()).unwrap();
        let raw: i32 = config["Pid"].as_i64().unwrap().try_into().unwrap();
        assert!(raw > 1);
        let cmdline = fs::read(format!("/proc/{raw}/cmdline")).unwrap();
        // JuiceFS daemonization replaces argv with one space-separated title.
        // Check the executable separately and never print its credential-bearing title.
        assert_eq!(
            fs::read_link(format!("/proc/{raw}/exe")).unwrap(),
            Path::new("/usr/local/bin/juicefs")
        );
        let args: Vec<_> = cmdline
            .split(|b| *b == 0 || b.is_ascii_whitespace())
            .collect();
        assert!(args.contains(&b"mount".as_slice()));
        assert!(args.contains(&mount.as_os_str().as_encoded_bytes()));
        let pid = rustix::process::Pid::from_raw(raw).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
        Self(pid)
    }
}
impl Drop for Paused {
    fn drop(&mut self) {
        rustix::process::kill_process(self.0, rustix::process::Signal::CONT).unwrap();
    }
}
pub fn verify(worker: &Value, config: &Value, prepared: &Value) -> Value {
    let t: PreparationTarget = serde_json::from_value(worker["target"].clone()).unwrap();
    let root = Path::new(worker["mount_root"].as_str().unwrap());
    let volume = MountedVolume::open(
        root,
        &t.volume_path,
        &t.filesystem_uuid,
        &t.pvc_uid,
        t.writer_uid,
        t.writer_gid,
    )
    .unwrap();
    let prepared: Prepared = serde_json::from_value(prepared.clone()).unwrap();
    let target = Path::new(
        config["fence_mount_root"]
            .as_str()
            .expect("explicit root-owned FUSE fixture directory"),
    )
    .join(format!("probe-{}", std::process::id()));
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let mounted =
        agent_computer_fence::mount(volume.candidate_directory(&prepared).unwrap(), &target)
            .unwrap();
    let mut workload = Workload::start(&target);
    workload.phase("ready");
    let proof = thread::scope(|scope| {
        // Keep the resume guard inside the scope, so assertion failures resume
        // JuiceFS before Rust joins the sealing thread.
        let paused = Paused::new(root);
        workload.send("write");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !mounted.status().active_mutation {
            assert!(Instant::now() < deadline, "write never entered the barrier");
            thread::sleep(Duration::from_millis(5));
        }
        let (send, result) = mpsc::channel();
        let fence = &mounted;
        scope.spawn(move || {
            send.send(fence.seal()).unwrap();
        });
        while !mounted.status().closing {
            assert!(Instant::now() < deadline, "seal was not requested");
            thread::yield_now();
        }
        assert!(
            matches!(
                result.recv_timeout(Duration::from_millis(300)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "paused JuiceFS produced a false drain receipt"
        );
        drop(paused);
        result
            .recv_timeout(Duration::from_secs(60))
            .unwrap()
            .unwrap()
    });
    workload.phase("persisted");
    assert!(proof.is_sealed());
    workload.send("sealed");
    let observed = workload.phase("done");
    assert!(workload.child.wait().unwrap().success());
    assert!(proof.is_sealed());
    let data = root
        .join(&t.volume_path)
        .join(&prepared.path_ref)
        .join("fence-dir/renamed");
    assert_eq!(fs::read(&data).unwrap(), b"durable");
    let evidence = json!({"receipt":proof.evidence(),"workload":observed,"paused_client_ms":300,"inflight_mutation_observed":true,"backing_path":data,"limits":["one disposable Linux VM","mount-local IO barrier only; not process fencing or database admission","no crash recovery or power-loss evidence"]});
    mounted.unmount().unwrap();
    fs::remove_dir(target).unwrap();
    evidence
}
