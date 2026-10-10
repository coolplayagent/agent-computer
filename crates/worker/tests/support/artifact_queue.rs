//! Exercise the real operator process, including shutdown while claim waits.
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
pub async fn run(
    c: &artifacts::Context<'_>,
    config: &std::path::Path,
    expected: Option<&str>,
) -> Vec<Value> {
    let mut barrier = c.pool.begin().await.unwrap();
    if expected.is_some() {
        sqlx::query(
            "SELECT last_sequence FROM organization_streams WHERE organization=$1 FOR UPDATE",
        )
        .bind(c.org.as_str())
        .fetch_one(&mut *barrier)
        .await
        .unwrap();
    }
    let log = config.with_extension("queue.log");
    let output = fs::File::create(&log).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
    let mut process = Process(
        Command::new(c.config["server_binary"].as_str().unwrap())
            .args([
                "artifact-worker",
                "--database-url-file",
                c.config["database_url_file"].as_str().unwrap(),
                "--organization",
                c.org.as_str(),
                "--worker-id",
                c.owner.as_str(),
                "--config-file",
            ])
            .arg(config)
            .args(["--concurrency", "2", "--poll-ms", "250"])
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let until = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(&log).unwrap()
        );
        let observed = events(&log);
        if observed.iter().any(|v| v["event"] == "ready")
            && (expected.is_none()
                || observed
                    .iter()
                    .any(|v| v["event"] == "scheduled" && v["commit_id"] == expected.unwrap()))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "artifact daemon admission"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if expected.is_none() {
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    if expected.is_some() {
        loop {
            if events(&log)
                .iter()
                .any(|v| v["event"] == "stopping" && v["active"] == 1)
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "artifact daemon SIGTERM"
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
            "artifact daemon joined stop"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let observed = events(&log);
    let n = usize::from(expected.is_some());
    assert_eq!(
        observed.last().unwrap(),
        &json!({"event":"stopped","summary":{"scheduled":n,"committed":n,"stopped":n,"busy":0,"unconfirmed":0,"poll_failures":0}})
    );
    observed
}
