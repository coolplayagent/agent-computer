//! Real host fault injection; never used by production execution code.
use super::*;
use agent_computer_watchdog::boottime_ms;
use std::{
    io::{Read, Seek},
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
    while !data.join("started.txt").exists() {
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
    let mut heartbeat_after_kill = None;
    if stopped.is_some() {
        let before = fs::metadata(data.join("ticks.txt"))
            .map(|m| m.len())
            .unwrap_or(0);
        // Independent JuiceFS clients may briefly cache file attributes. Wait
        // for an observed new write, always within the original node deadline.
        let observation_limit = (boottime_ms() + 5000).min(deadline);
        let after = loop {
            let after = fs::metadata(data.join("ticks.txt"))
                .map(|m| m.len())
                .unwrap_or(0);
            if after > before {
                break after;
            }
            assert!(
                boottime_ms() < observation_limit,
                "writer must still run after controller death and PID 1 stop"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        heartbeat_after_kill =
            Some(json!({"before":before,"after":after,"observed_boottime_ms":boottime_ms()}));
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
    json!({"controller_pid":controller_pid,"controller_signal":9,"controller_killed_boottime_ms":killed_at,"pid1_state":stopped,"heartbeat_after_kill":heartbeat_after_kill,"cgroup_inode":runtime["cgroup_inode"],"deadline_boottime_ms":deadline,"empty_observed_boottime_ms":empty_at,"api_delete_before_empty":false,"state":state})
}

fn read_events(file: &mut fs::File) -> String {
    file.rewind().unwrap();
    let mut value = String::new();
    file.read_to_string(&mut value).unwrap();
    value
}
