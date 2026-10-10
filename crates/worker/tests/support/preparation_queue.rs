//! Real operator process. The SQL barrier delays claims, never fabricates storage.
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
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn terminate(process: &Process) {
    assert!(
        Command::new("kill")
            .args(["-TERM", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}
pub struct Fixture<'a> {
    pub config: &'a Value,
    pub worker: &'a Value,
    pub org: &'a OrganizationId,
    pub pool: &'a sqlx::PgPool,
}
impl Fixture<'_> {
    pub async fn run(&self, requests: &[String], drain_pending: bool) -> Value {
        let Self {
            config,
            worker,
            org,
            pool,
        } = *self;
        let path = PathBuf::from(config["observation_file"].as_str().unwrap())
            .with_file_name("preparation-queue-command.json");
        fs::write(&path, serde_json::to_vec(worker).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let command = || {
            let mut c = Command::new(config["server_binary"].as_str().unwrap());
            c.args([
                "candidate-worker",
                "--database-url-file",
                config["database_url_file"].as_str().unwrap(),
                "--organization",
                org.as_str(),
                "--worker-id",
                "continuous-fixture",
                "--config-file",
            ])
            .arg(&path)
            .args(["--concurrency", "2", "--poll-ms", "250"]);
            c
        };
        if drain_pending {
            let mut invalid = worker.clone();
            invalid["target"]["filesystem_uuid"] = json!("wrong-filesystem");
            fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(!command().output().unwrap().status.success());
            fs::write(&path, serde_json::to_vec(worker).unwrap()).unwrap();
        }
        let mut barrier = pool.begin().await.unwrap();
        if drain_pending {
            sqlx::query(
                "SELECT last_sequence FROM organization_streams WHERE organization=$1 FOR UPDATE",
            )
            .bind(org.as_str())
            .fetch_one(&mut *barrier)
            .await
            .unwrap();
        }
        let log = path.with_extension("log");
        let output = fs::File::create(&log).unwrap();
        fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
        let mut process = Process(
            command()
                .stdin(Stdio::null())
                .stdout(output.try_clone().unwrap())
                .stderr(output)
                .spawn()
                .unwrap(),
        );
        let until = tokio::time::Instant::now() + Duration::from_secs(40);
        loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(&log).unwrap()
            );
            let log_events = events(&log);
            let ready = log_events.iter().any(|v| v["event"] == "ready");
            let count = log_events
                .iter()
                .filter(|v| {
                    v["event"]
                        == if drain_pending {
                            "scheduled"
                        } else {
                            "finished"
                        }
                })
                .count();
            if ready && count == requests.len() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "Candidate queue progress: {}",
                fs::read_to_string(&log).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if requests.is_empty() {
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
        terminate(&process);
        if drain_pending {
            loop {
                let observed = events(&log);
                if observed
                    .iter()
                    .any(|v| v["event"] == "stopping" && v["active"] == requests.len())
                {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < until,
                    "SIGTERM while both jobs await admission"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        barrier.commit().await.unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                assert!(status.success(), "{}", fs::read_to_string(&log).unwrap());
                break;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "Candidate worker stop deadline"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let observed = events(&log);
        let mut scheduled: Vec<_> = observed
            .iter()
            .filter(|v| v["event"] == "scheduled")
            .map(|v| v["request_id"].as_str().unwrap().to_owned())
            .collect();
        scheduled.sort();
        let mut expected = requests.to_vec();
        expected.sort();
        assert_eq!(scheduled, expected);
        let summary = &observed.last().unwrap()["summary"];
        assert_eq!(summary["scheduled"], requests.len());
        assert_eq!(summary["unconfirmed"], 0);
        assert_eq!(summary["busy"], 0);
        assert_eq!(summary["poll_failures"], 0);
        if drain_pending {
            assert_eq!(summary["prepared"], 2);
            assert_eq!(summary["storage_unknown"], 0);
        } else if !requests.is_empty() {
            assert_eq!(summary["storage_unknown"], 1);
        }
        json!({"events":observed,"sql_barrier_released_after_sigterm":drain_pending,"invalid_target_rejected":drain_pending})
    }
}
