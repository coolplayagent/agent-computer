//! Real operator process, two admitted executions, SIGTERM and idle restart.
use super::*;
use std::process::{Child, Command, Stdio};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn events(path: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn stop(process: &Process) {
    assert!(
        Command::new("kill")
            .args(["-TERM", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}
async fn join(process: &mut Process) {
    let until = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "worker status: {status}");
            return;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "worker shutdown deadline"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub struct Fixture<'a> {
    pub config: &'a Value,
    pub private: &'a Value,
    pub store: &'a Store,
    pub org: &'a OrganizationId,
    pub token: &'a str,
}
impl Fixture<'_> {
    pub async fn run(&self, normal: &Value, queued: &ExecutionRequest) -> (Value, Value, Value) {
        let Self {
            config,
            private,
            store,
            org,
            token,
        } = *self;
        let old = store
            .candidate_execution_dispatch(org, field(normal, "execution_id"))
            .await
            .unwrap();
        let lease = store
            .acquire_candidate_writer(
                token,
                &key("queue-concurrent-lease"),
                &old.execution.computer_id,
                &AcquireWriterLease {
                    connection_session_id: old.input.lease.connection_session_id.clone(),
                    generation: old.execution.generation,
                    candidate_id: old.execution.candidate_id.clone(),
                    scope: WriterScope::Modify,
                    duration_seconds: 30,
                },
            )
            .await
            .unwrap();
        assert_eq!(lease.epoch, 3);
        let mut input = old.input;
        input.lease = WriterLeaseCommand {
            connection_session_id: lease.connection_session_id.clone(),
            generation: lease.generation,
            epoch: lease.epoch,
            expected_revision: lease.revision,
        };
        input.command.argv = vec!["/bin/sh".into(), "-c".into(), "printf active > queue-started.txt; /bin/sync queue-started.txt; /bin/sleep 5; printf concurrent > queue-concurrent.txt; /bin/sync queue-concurrent.txt; printf queue-done".into()];
        let extra = store
            .submit_candidate_execution(
                token,
                &key("queue-concurrent"),
                &old.execution.computer_id,
                &input,
            )
            .await
            .unwrap();
        let path = PathBuf::from(field(config, "result_file")).with_file_name("queue-command.json");
        fs::write(&path, serde_json::to_vec(private).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let command = || {
            let mut cmd = Command::new(field(config, "server_binary"));
            cmd.args([
                "execution-worker",
                "--database-url-file",
                field(config, "database_url_file"),
                "--organization",
                org.as_str(),
                "--config-file",
            ])
            .arg(&path);
            cmd
        };
        for args in [
            ["--concurrency", "5"],
            ["--concurrency", "0"],
            ["--poll-ms", "249"],
            ["--poll-ms", "5001"],
        ] {
            assert!(!command().args(args).output().unwrap().status.success());
        }
        // A stale node boot must fail preflight before consuming either request.
        let mut stale = private.clone();
        stale["execution"]["node"]["node"]["boot_id"] =
            json!("00000000-0000-0000-0000-000000000000");
        fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(!command().output().unwrap().status.success());
        fs::write(&path, serde_json::to_vec(private).unwrap()).unwrap();
        assert_eq!(
            store
                .candidate_execution(token, &queued.execution_id)
                .await
                .unwrap()
                .state,
            ExecutionState::Queued
        );
        assert_eq!(
            store
                .candidate_execution(token, &extra.execution_id)
                .await
                .unwrap()
                .state,
            ExecutionState::Queued
        );
        let log = path.with_extension("log");
        let spawn = || {
            let output = fs::File::create(&log).unwrap();
            fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
            Process(
                command()
                    .args(["--concurrency", "2", "--poll-ms", "250"])
                    .stdin(Stdio::null())
                    .stdout(output.try_clone().unwrap())
                    .stderr(output)
                    .spawn()
                    .unwrap(),
            )
        };
        let mut process = spawn();
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(&log).unwrap()
            );
            let a = store
                .candidate_execution_startup(org, &queued.execution_id)
                .await
                .unwrap();
            let b = store
                .candidate_execution_startup(org, &extra.execution_id)
                .await
                .unwrap();
            if a.is_some() && b.is_some() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "two concurrent startup grants: {}",
                fs::read_to_string(&log).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        stop(&process);
        join(&mut process).await;
        let initial = events(&log);
        let claims: Vec<_> = initial
            .iter()
            .filter(|v| v["event"] == "claimed")
            .map(|v| field(v, "execution_id"))
            .collect();
        assert_eq!(
            claims,
            [queued.execution_id.as_str(), extra.execution_id.as_str()]
        );
        assert!(
            initial
                .iter()
                .any(|v| v["event"] == "stopping" && v["active"] == 2)
        );
        let summary = &initial.last().unwrap()["summary"];
        assert_eq!(
            summary,
            &json!({"claimed":2,"cancelled_before_dispatch":0,"finished":2,"unconfirmed":0,"poll_failures":0})
        );
        let result = |id: &str| {
            initial
                .iter()
                .find(|v| {
                    v["event"] == "finished" && v["result"]["execution"]["execution_id"] == id
                })
                .unwrap()["result"]
                .clone()
        };
        let command_result = result(&queued.execution_id);
        let extra_result = result(&extra.execution_id);
        for item in [&command_result, &extra_result] {
            assert_eq!(item["execution"]["state"], "Succeeded", "{item}");
            assert_eq!(item["completion"]["accepted_state"], "Succeeded");
            assert_eq!(item["publication_revoked"], true);
        }
        assert_eq!(
            store
                .reconcile_candidate_writer(org, &lease.lease_id)
                .await
                .unwrap()
                .state,
            WriterLeaseState::Released
        );
        let output = outputs::verify(
            config,
            private,
            store,
            token,
            org,
            &extra.execution_id,
            "command",
            &extra_result,
        )
        .await;
        let client = Client::new(
            field(private, "api_url"),
            &fs::read(field(private, "ca_file")).unwrap(),
            fs::read_to_string(field(private, "token_file"))
                .unwrap()
                .trim(),
            serde_json::from_value(private["deployment"].clone()).unwrap(),
        )
        .unwrap();
        let execution_config: execution::Configuration =
            serde_json::from_value(private["execution"].clone()).unwrap();
        let recovery = execution::recover_once(
            store,
            &client,
            org,
            &extra.execution_id,
            &execution_config.storage,
            &execution_config.approved_supervisor_image,
            &execution_config.node.spool,
        )
        .await
        .unwrap();
        let extra_evidence = json!({"case":"queue-concurrent","execution_id":extra.execution_id,"prepared":normal["prepared"],"outcome":extra_result,"outputs":output,"next_writer":null,"recovery":recovery,"pod":store.candidate_execution_pod(org,&extra.execution_id).await.unwrap(),"watchdog":store.candidate_execution_watchdog(org,&extra.execution_id).await.unwrap()});
        let mut restarted = spawn();
        let until = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(restarted.0.try_wait().unwrap().is_none());
            if fs::read_to_string(&log)
                .unwrap()
                .contains("\"event\":\"ready\"")
            {
                break;
            }
            assert!(tokio::time::Instant::now() < until);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
        stop(&restarted);
        join(&mut restarted).await;
        let restart = events(&log);
        assert_eq!(
            restart.last().unwrap()["summary"],
            json!({"claimed":0,"cancelled_before_dispatch":0,"finished":0,"unconfirmed":0,"poll_failures":0})
        );
        (
            command_result,
            extra_evidence,
            json!({"initial":initial,"restart":restart,"invalid_options_rejected":4,"stale_boot_rejected_before_claim":true}),
        )
    }
}
