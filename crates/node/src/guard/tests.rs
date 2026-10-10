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
    let spool = tempfile::Builder::new()
        .prefix("journal-test-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/root")
        .unwrap();
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
        let expected = serde_json::json!({"version":2,"armed":guards[0].0,"backup_armed":guards[1].0,"watchdog_pids":[cleanup.0[0].id(),cleanup.0[1].id()]});
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
        // The survivor persists its report before the closed output returns 2.
        assert_eq!(cleanup.0[1 - index].wait().unwrap().code(), Some(2));
        let observations = crate::observe_journals(spool.path(), &expected).unwrap();
        assert!(matches!(
            observations.guards[index],
            crate::JournalStatus::Unconfirmed { .. }
        ));
        assert!(
            matches!(&observations.guards[1-index],crate::JournalStatus::Recorded{report,..} if report.observation==agent_computer_watchdog::Observation::EmptyObserved)
        );
        println!("{}", serde_json::to_string(&observations).unwrap());
        rejects_tampered_journals(spool.path(), &expected, 1 - index);
        drop(cleanup);
        std::fs::remove_dir(&path).unwrap();
        println!(
            "{}",
            serde_json::json!({"case":"redundant_guard_fault", "failed_index":index, "signal":format!("{signal:?}"), "deadline_boottime_ms":request.deadline_boottime_ms,"empty_observed_boottime_ms":observed,"writer_released":false})
        );
    }
}

fn rejects_tampered_journals(spool: &std::path::Path, evidence: &serde_json::Value, index: usize) {
    use crate::{JournalStatus, observe_journals};
    use std::os::unix::fs::{PermissionsExt, symlink};
    let arm = if index == 0 { "armed" } else { "backup_armed" };
    let dir = spool.join(evidence[arm]["journal"]["id"].as_str().unwrap());
    let mut wrong = evidence.clone();
    wrong["watchdog_pids"][index] = serde_json::json!(1);
    assert!(matches!(
        observe_journals(spool, &wrong).unwrap().guards[index],
        JournalStatus::Unavailable { .. }
    ));
    let mut legacy = evidence.clone();
    legacy[arm].as_object_mut().unwrap().remove("journal");
    assert!(matches!(
        observe_journals(spool, &legacy).unwrap().guards[index],
        JournalStatus::LegacyUnjournaled
    ));
    let intent = dir.join("intent.json");
    let report = dir.join("report.json");
    let original = std::fs::read(&report).unwrap();
    let expected_unavailable = || {
        assert!(matches!(
            observe_journals(spool, evidence).unwrap().guards[index],
            JournalStatus::Unavailable { .. }
        ))
    };
    let mut bad: serde_json::Value = serde_json::from_slice(&original).unwrap();
    bad["request"]["execution_id"] = "foreign".into();
    std::fs::write(&report, serde_json::to_vec(&bad).unwrap()).unwrap();
    expected_unavailable();
    std::fs::write(&report, &original).unwrap();
    let bytes = std::fs::read(&intent).unwrap();
    let mut changed = bytes.clone();
    changed.push(b' ');
    std::fs::write(&intent, &changed).unwrap();
    expected_unavailable();
    std::fs::write(&intent, &bytes).unwrap();
    std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o644)).unwrap();
    expected_unavailable();
    std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o600)).unwrap();
    let second = dir.join("copy");
    std::fs::hard_link(&report, &second).unwrap();
    expected_unavailable();
    std::fs::remove_file(second).unwrap();
    std::fs::remove_file(&report).unwrap();
    symlink(&intent, &report).unwrap();
    expected_unavailable();
    std::fs::remove_file(&report).unwrap();
    // A partially written temporary file cannot become a completed observation.
    std::fs::write(dir.join("report.pending"), b"{").unwrap();
    assert!(matches!(
        observe_journals(spool, evidence).unwrap().guards[index],
        JournalStatus::Unconfirmed { .. }
    ));
    std::fs::remove_file(dir.join("report.pending")).unwrap();
    // Restore only this owned fixture's original result for final readback.
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&report)
        .unwrap();
    file.write_all(&original).unwrap();
    assert!(matches!(
        observe_journals(spool, evidence).unwrap().guards[index],
        JournalStatus::Recorded { .. }
    ));
}

#[test]
#[ignore = "requires root, writable host cgroup v2 and AGENT_COMPUTER_WATCHDOG_BIN in a disposable VM"]
fn reaper_observation_preserves_original_arms_after_both_guards_die() {
    use std::{os::unix::fs::MetadataExt, path::PathBuf};
    assert!(rustix::process::geteuid().is_root());
    let executable = File::open(std::env::var("AGENT_COMPUTER_WATCHDOG_BIN").unwrap()).unwrap();
    let spool = tempfile::Builder::new()
        .prefix("reaper-test-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/root")
        .unwrap();
    let name = format!("ac-reaper-node-{}-{}", std::process::id(), boottime_ms());
    let path = PathBuf::from("/sys/fs/cgroup").join(&name);
    std::fs::create_dir(&path).unwrap();
    let _group = OwnedGroup(path.clone());
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
        deadline_boottime_ms: boottime_ms() + 1200,
    };
    let mut guards = launch_pair(
        &executable,
        spool.path(),
        &request,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    let mut children = OwnedProcesses(
        guards
            .iter_mut()
            .map(|(_, child, _)| child.0.take().unwrap())
            .collect(),
    );
    let evidence = serde_json::json!({"version":2,"armed":guards[0].0,"backup_armed":guards[1].0,"watchdog_pids":[children.0[0].id(),children.0[1].id()]});
    for child in &mut children.0 {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    drop(guards);
    let mut reaper = agent_computer_watchdog::reaper::Reaper::open(spool.path()).unwrap();
    let limit = Instant::now() + Duration::from_secs(5);
    loop {
        reaper.step().unwrap();
        let result = crate::observe_journals(spool.path(), &evidence).unwrap();
        if result
            .guards
            .iter()
            .all(|v| matches!(v, crate::JournalStatus::Recovered { .. }))
        {
            println!("{}", serde_json::to_string(&result).unwrap());
            break;
        }
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(boottime_ms() >= request.deadline_boottime_ms);
    workload.process().wait().unwrap();
    let mut wrong = evidence.clone();
    wrong["armed"]["cgroup_device"] = serde_json::json!(999);
    assert!(matches!(
        crate::observe_journals(spool.path(), &wrong)
            .unwrap()
            .guards[0],
        crate::JournalStatus::Unavailable { .. }
    ));
    let recovered_path = spool
        .path()
        .join(evidence["armed"]["journal"]["id"].as_str().unwrap())
        .join("recovery.json");
    let bytes = std::fs::read(&recovered_path).unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    changed["trigger"] = "Deadline".into();
    std::fs::write(&recovered_path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(matches!(
        crate::observe_journals(spool.path(), &evidence)
            .unwrap()
            .guards[0],
        crate::JournalStatus::Unavailable { .. }
    ));
    std::fs::write(recovered_path, bytes).unwrap();
    println!(
        "{}",
        serde_json::json!({"case":"node_reaper_recovery","status":"pass","writer_released":false})
    );
}
