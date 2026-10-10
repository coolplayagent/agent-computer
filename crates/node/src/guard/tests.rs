use super::*;
use rustix::process::{Pid, Signal, kill_process};
use std::process::Command;

fn sleeper() -> DetachedChild {
    DetachedChild(Some(
        Command::new("/usr/bin/sleep").arg("30").spawn().unwrap(),
    ))
}

#[test]
fn killed_or_stopped_watchdog_cannot_authorize_and_is_not_reaped_by_observation() {
    for signal in [Signal::KILL, Signal::STOP] {
        let mut child = sleeper();
        child.require_running().unwrap();
        let pid = Pid::from_child(child.process());
        kill_process(pid, signal).unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while child.require_running().is_ok() {
            assert!(Instant::now() < limit);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(child.require_running(), Err(Error::WatchdogUnavailable));
        // The original identity remains waitable and reserved during inspection.
        kill_process(pid, Signal::KILL).unwrap();
        child.process().wait().unwrap();
    }
}

#[test]
fn dropping_controller_handle_keeps_independent_guard_alive() {
    let mut child = sleeper();
    let pid = Pid::from_child(child.process());
    drop(child);
    rustix::process::test_kill_process(pid).unwrap();
    kill_process(pid, Signal::KILL).unwrap();
}

struct OwnedGroup(std::path::PathBuf);
impl Drop for OwnedGroup {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("cgroup.kill"), "1");
        for _ in 0..500 {
            if !self.0.exists() {
                break;
            }
            if std::fs::read_to_string(self.0.join("cgroup.events"))
                .is_ok_and(|value| value.contains("populated 0\n"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}
struct OwnedProcesses(Vec<std::process::Child>);
impl Drop for OwnedProcesses {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
/// Actual kernel test, never run by default or against an existing workload.
#[test]
#[ignore = "requires root, writable host cgroup v2 and AGENT_COMPUTER_WATCHDOG_BIN in a disposable VM"]
fn redundant_timers_survive_either_guard_killed_or_stopped() {
    use std::{os::unix::fs::MetadataExt, path::PathBuf};
    assert!(rustix::process::geteuid().is_root());
    let binary = std::env::var("AGENT_COMPUTER_WATCHDOG_BIN").unwrap();
    let executable = File::open(binary).unwrap();
    let spool = tempfile::tempdir().unwrap();
    for (index, signal) in [
        (0, Signal::KILL),
        (1, Signal::KILL),
        (0, Signal::STOP),
        (1, Signal::STOP),
    ] {
        let name = format!("ac-guard-pair-{}-{}", std::process::id(), boottime_ms());
        let path = PathBuf::from("/sys/fs/cgroup").join(&name);
        std::fs::create_dir(&path).unwrap();
        let _group_cleanup = OwnedGroup(path.clone());
        let mut workload = sleeper();
        let pid = Pid::from_child(workload.process());
        std::fs::write(path.join("cgroup.procs"), pid.as_raw_nonzero().to_string()).unwrap();
        kill_process(pid, Signal::STOP).unwrap();
        let request = Request {
            version: 1,
            execution_id: name.clone(),
            boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim()
                .into(),
            cgroup_path: name,
            cgroup_inode: std::fs::metadata(&path).unwrap().ino(),
            deadline_boottime_ms: boottime_ms() + 1500,
        };
        let mut guards = launch_pair(
            &executable,
            spool.path(),
            &request,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&guards[0].0.request).unwrap(),
            serde_json::to_value(&guards[1].0.request).unwrap()
        );
        for (_, child, _) in &mut guards {
            let guard_pid = Pid::from_child(child.process());
            assert_eq!(rustix::process::getsid(Some(guard_pid)).unwrap(), guard_pid);
        }
        // Hold original child identities for fault cleanup even if assertions fail.
        let mut cleanup = OwnedProcesses(
            guards
                .iter_mut()
                .map(|(_, child, _)| child.0.take().unwrap())
                .collect(),
        );
        let failed = Pid::from_child(&cleanup.0[index]);
        kill_process(failed, signal).unwrap();
        let limit = Instant::now() + Duration::from_secs(1);
        loop {
            use rustix::process::{WaitId, WaitIdOptions, waitid};
            if waitid(
                WaitId::Pid(failed),
                WaitIdOptions::EXITED
                    | WaitIdOptions::STOPPED
                    | WaitIdOptions::NOHANG
                    | WaitIdOptions::NOWAIT,
            )
            .unwrap()
            .is_some()
            {
                break;
            }
            assert!(Instant::now() < limit);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cleanup.0[1 - index].try_wait().unwrap().is_none());
        // Both controller-owned pipe readers disappear before the deadline.
        // Neither dropping a handle nor the failed peer can disarm the survivor.
        drop(guards);
        let limit = request.deadline_boottime_ms + 3000;
        while !std::fs::read_to_string(path.join("cgroup.events"))
            .unwrap()
            .contains("populated 0\n")
        {
            assert!(
                boottime_ms() < limit,
                "survivor did not terminate the target"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let observed = boottime_ms();
        assert!(observed >= request.deadline_boottime_ms);
        workload.process().wait().unwrap();
        // A stopped child has not been reaped; its PID is still reserved.
        if signal == Signal::STOP {
            kill_process(failed, Signal::KILL).unwrap();
        }
        drop(cleanup);
        std::fs::remove_dir(&path).unwrap();
        println!(
            "{}",
            serde_json::json!({"case":"redundant_guard_fault", "failed_index":index, "signal":format!("{signal:?}"), "deadline_boottime_ms":request.deadline_boottime_ms,"empty_observed_boottime_ms":observed,"writer_released":false})
        );
    }
}
