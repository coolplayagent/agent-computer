//! Real host fault injection; never used by production execution code.
use super::*;
use agent_computer_watchdog::boottime_ms;
use std::{
    io::{Read, Seek, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Command, Stdio},
};

pub async fn kill_controller(
    config: &Value,
    private: &Value,
    store: &Store,
    org: &OrganizationId,
    execution: &str,
    data: &Path,
    case: &str,
) -> Value {
    let config_path =
        PathBuf::from(field(config, "result_file")).with_file_name(format!("{case}-command.json"));
    fs::write(&config_path, serde_json::to_vec(private).unwrap()).unwrap();
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    let output_path = config_path.with_extension("log");
    let output = fs::File::create(&output_path).unwrap();
    fs::set_permissions(&output_path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut controller = Command::new(field(config, "server_binary"))
        .args([
            "execution-dispatch-once",
            "--database-url-file",
            field(config, "database_url_file"),
            "--organization",
            org.as_str(),
            "--execution-id",
            execution,
            "--expected-revision",
            "1",
            "--config-file",
        ])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    let limit = tokio::time::Instant::now() + Duration::from_secs(25);
    // Creation precedes the write. Observe the complete marker before killing
    // the controller, otherwise the fixture may interrupt its own setup IO.
    while !matches!(fs::read(data.join("started.txt")), Ok(bytes) if bytes == b"started") {
        assert!(
            controller.try_wait().unwrap().is_none(),
            "controller exited before child marker: {}",
            fs::read_to_string(&output_path).unwrap()
        );
        assert!(tokio::time::Instant::now() < limit, "fault marker deadline");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let arm = store
        .candidate_execution_watchdog(org, execution)
        .await
        .unwrap()
        .unwrap();
    let runtime = &arm.evidence["runtime"];
    let path = Path::new("/sys/fs/cgroup").join(field(runtime, "cgroup_path"));
    assert_eq!(
        fs::metadata(&path).unwrap().ino(),
        runtime["cgroup_inode"].as_u64().unwrap()
    );
    let mut events = fs::File::open(path.join("cgroup.events")).unwrap();
    assert!(read_events(&mut events).lines().any(|s| s == "populated 1"));
    let stopped = if case == "pid1-stop" {
        let status = fs::read_to_string(data.join("pid1-status.txt")).unwrap();
        let state = status
            .lines()
            .find(|s| s.starts_with("State:"))
            .unwrap()
            .to_owned();
        assert!(state.split_whitespace().nth(1) == Some("T"), "{state}");
        Some(state)
    } else {
        None
    };
    let mut heartbeat_before_kill = None;
    if stopped.is_some() {
        let before = fs::metadata(data.join("ticks.txt"))
            .map(|m| m.len())
            .unwrap_or(0);
        let limit = tokio::time::Instant::now() + Duration::from_secs(3);
        let after = loop {
            let after = fs::metadata(data.join("ticks.txt"))
                .map(|m| m.len())
                .unwrap_or(0);
            if after > before {
                break after;
            }
            assert!(
                tokio::time::Instant::now() < limit,
                "writer did not run while PID 1 was stopped"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        heartbeat_before_kill =
            Some(json!({"before":before,"after":after,"observed_boottime_ms":boottime_ms()}));
    }
    let front = Path::new(field(&runtime["workspace_mount"], "path"));
    let mut old_fd = fs::OpenOptions::new()
        .append(true)
        .open(front.join("started.txt"))
        .unwrap();
    let killed_at = boottime_ms();
    let controller_pid = controller.id();
    controller.kill().unwrap();
    let exit = controller.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(exit.signal(), Some(9));
    let deadline = arm.evidence["armed"]["request"]["deadline_boottime_ms"]
        .as_u64()
        .unwrap();
    assert!(killed_at < deadline);
    // A disconnected FUSE connection denies new IO even on an old descriptor.
    // This is not a successful seal: in-flight backing IO remains unconfirmed.
    let write_error = old_fd
        .write_all(b"forbidden-after-controller-death")
        .unwrap_err()
        .raw_os_error()
        .unwrap();
    assert!(
        matches!(write_error, 5 | 107),
        "unexpected post-kill errno: {write_error}"
    );
    assert_eq!(fs::read(data.join("started.txt")).unwrap(), b"started");
    if stopped.is_some() {
        assert!(
            read_events(&mut events).lines().any(|s| s == "populated 1"),
            "stopped PID 1 still needs the independent timer"
        );
    }
    let empty_at = loop {
        if read_events(&mut events).lines().any(|s| s == "populated 0") {
            break boottime_ms();
        }
        assert!(
            boottime_ms() <= deadline + 5000,
            "original cgroup still populated after deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    if stopped.is_some() {
        assert!(
            empty_at >= deadline,
            "stopped PID 1 must survive until the independent node timer"
        );
        let before = fs::metadata(data.join("ticks.txt")).unwrap().len();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(before, fs::metadata(data.join("ticks.txt")).unwrap().len());
    }
    assert!(!data.join("late.txt").exists());
    // Observe expiry before recovery; no delete request caused this observation.
    while boottime_ms() <= deadline + 10 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The conservative node deadline can precede the original database
    // deadline. Keep observing the latter; do not equate the two clocks or
    // issue cancellation to force this assertion to pass.
    let reconcile_until = tokio::time::Instant::now() + Duration::from_secs(5);
    let state = loop {
        let state = store
            .reconcile_candidate_execution(org, execution)
            .await
            .unwrap();
        if state.state == ExecutionState::Unknown {
            break state;
        }
        assert_eq!(state.state, ExecutionState::Dispatching);
        assert!(
            tokio::time::Instant::now() < reconcile_until,
            "database expiry reconciliation deadline"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    json!({"controller_pid":controller_pid,"controller_signal":9,"controller_killed_boottime_ms":killed_at,"pid1_state":stopped,"heartbeat_before_kill":heartbeat_before_kill,"post_kill_old_fd_write_errno":write_error,"cgroup_inode":runtime["cgroup_inode"],"deadline_boottime_ms":deadline,"empty_observed_boottime_ms":empty_at,"api_delete_before_empty":false,"state":state})
}

fn read_events(file: &mut fs::File) -> String {
    file.rewind().unwrap();
    let mut value = String::new();
    file.read_to_string(&mut value).unwrap();
    value
}
