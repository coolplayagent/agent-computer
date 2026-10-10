//! Manual disposable-VM probe: real Store/server/K3s/gVisor/Chromium/watchdogs.
//! The existing PreparedComputer/storage prerequisite is synthetic database
//! evidence here. This test makes no Workspace/CSI/checkpoint acceptance claim.
use super::*;
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};
fn command(args: &[&str]) {
    let result = Command::new("sudo").arg("-n").args(args).output().unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
#[ignore = "requires explicit disposable VM, root server, K3s and Browser image; storage prerequisite is synthetic"]
async fn manual_browser_controller_live() {
    let config: Value = serde_json::from_slice(
        &fs::read(
            std::env::var("AGENT_COMPUTER_BROWSER_CONTROLLER_FIXTURE")
                .expect("explicit VM fixture"),
        )
        .unwrap(),
    )
    .unwrap();
    let mut results = Vec::new();
    for case in ["normal", "revoke", "database-stall", "controller-kill"] {
        let (db, token, computer, input) =
            setup_browser(Some(config["image"].as_str().unwrap())).await;
        let app = admit(&db, &token, &computer, &input).await;
        let dir = tempfile::tempdir().unwrap();
        let dbfile = dir.path().join("database-url");
        private(&dbfile, db.connection_url().as_bytes());
        let policy: String = sqlx::query_scalar(
            "SELECT binding#>>'{sandbox,spec,networkPolicyRef}' FROM app_instances",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let mut controller = config["controller"].clone();
        controller["deployment"]["network_policy_ref"] = policy.into();
        let conf = dir.path().join("controller.json");
        private(&conf, &serde_json::to_vec(&controller).unwrap());
        let log = dir.path().join("controller.log");
        let unit = format!("ac-browser-controller-{}-{case}", std::process::id());
        let output = fs::File::create(&log).unwrap();
        let mut child = Command::new("sudo")
            .args([
                "-n",
                "systemd-run",
                "--quiet",
                "--wait",
                "--pipe",
                "--collect",
                "--unit",
                &unit,
                "/usr/local/bin/agent-computer-server",
                "browser-dispatch-once",
                "--database-url-file",
                dbfile.to_str().unwrap(),
                "--organization",
                "acme",
                "--instance-id",
                &app.instance_id,
                "--expected-revision",
                "1",
                "--config-file",
                conf.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(120);
        let running = loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "{case}: exited {status} before Running: {}",
                    fs::read_to_string(&log).unwrap()
                );
            }
            let current = db
                .store
                .app_instance(&token, &app.instance_id)
                .await
                .unwrap();
            if current.state == AppState::Running {
                break current;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "{case}: no Running observation"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(!running.ready);
        let arm = sqlx::query_scalar::<_, Value>(
            "SELECT payload->'evidence' FROM app_browser_journal WHERE kind='arm'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let mut stall = None;
        if case == "revoke" {
            db.store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org("acme"),
                        principal: &principal("alice"),
                        kind: RuntimeKind::App,
                        resource_id: &app.app_id,
                        permission: RuntimePermission::AppUse,
                        max_runtime_seconds: None,
                    },
                    false,
                )
                .await
                .unwrap();
        } else if case == "controller-kill" {
            command(&[
                "systemctl",
                "kill",
                "--signal=SIGKILL",
                "--kill-whom=main",
                &unit,
            ]);
        } else if case == "database-stall" {
            let mut tx = db.pool.begin().await.unwrap();
            sqlx::query(
                "SELECT last_sequence FROM organization_streams WHERE organization='acme' FOR UPDATE",
            )
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            stall = Some(tx);
        }
        let exit = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "{case}: controller did not finish"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        if let Some(tx) = stall {
            tx.rollback().await.unwrap();
            db.store
                .drain_browser_app(&org("acme"), &app.instance_id)
                .await
                .unwrap();
        }
        // Only inspect the captured operator output after its producer terminates.
        let report = if case == "controller-kill" {
            assert!(!exit.success());
            Value::Null
        } else {
            assert!(
                exit.success(),
                "{case}: {}",
                fs::read_to_string(&log).unwrap()
            );
            serde_json::from_slice::<Value>(&fs::read(&log).unwrap()).unwrap()
        };
        // Physical readback of the original process identities, independent of
        // public state, controller output, Pod status, or a JSON stop assertion.
        let physical = loop {
            let evidence = arm["runtime"]["runtime_processes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    let pid = p["pid"].as_u64().unwrap();
                    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
                        Ok(value) => Some(value),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => panic!("original process readback failed: {error}"),
                    };
                    let fields = stat
                        .as_deref()
                        .and_then(|s| s.rsplit_once(") "))
                        .map(|(_, s)| s.split_whitespace().collect::<Vec<_>>());
                    let alive = fields.as_ref().is_some_and(|f| {
                        f.get(19).and_then(|v| v.parse::<u64>().ok()) == p["start_ticks"].as_u64()
                            && !matches!(f.first(), Some(&"Z" | &"X"))
                    });
                    json!({"pid":pid,"original_alive":alive})
                })
                .collect::<Vec<_>>();
            if evidence.iter().all(|p| p["original_alive"] == false) {
                break evidence;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "{case}: original runtime survived watchdog"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        if case == "revoke" {
            allow(
                &db,
                &app.app_id,
                RuntimeKind::App,
                RuntimePermission::AppUse,
                None,
            )
            .await;
        }
        let state = loop {
            let current = db
                .store
                .app_instance(&token, &app.instance_id)
                .await
                .unwrap();
            if current.state == AppState::Draining {
                break current;
            }
            // Loss of a controller is not an immediate database observation.
            // Reading reconciles its original, nonrenewable arm deadline; no
            // historical Running row can authorize another launch or GUI action.
            assert_eq!(case, "controller-kill");
            assert_eq!(current.state, AppState::Running);
            assert!(!current.ready);
            assert!(
                tokio::time::Instant::now() < until,
                "original arm deadline did not converge"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let entries = db
            .store
            .browser_app_journal(&org("acme"), &app.instance_id)
            .await
            .unwrap();
        let stopped = entries.iter().any(|e| e.kind == "stop");
        assert_eq!(stopped, matches!(case, "normal" | "revoke"));
        assert!(
            db.store
                .cancel_app(
                    &token,
                    &key("release-denied"),
                    &app.instance_id,
                    &CancelApp {
                        app_generation: 1,
                        expected_revision: state.revision
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(entries.iter().filter(|e| e.kind == "launch").count(), 1);
        let pod = entries.iter().find(|e| e.kind == "pod").unwrap().payload["pod_uid"]
            .as_str()
            .unwrap();
        let manifest = &entries[0].payload["manifest"];
        // Fixture-only API cleanup after independent physical readback. The
        // product controller retains its ordinary UID/resourceVersion delete.
        let deletion = dir.path().join("delete.json");
        fs::write(&deletion,serde_json::to_vec(&json!({"apiVersion":"v1","kind":"DeleteOptions","gracePeriodSeconds":0,"preconditions":{"uid":pod}})).unwrap()).unwrap();
        let path = format!(
            "/api/v1/namespaces/{}/pods/{}",
            manifest["metadata"]["namespace"].as_str().unwrap(),
            manifest["metadata"]["name"].as_str().unwrap()
        );
        let cleanup = Command::new("sudo")
            .args([
                "-n",
                "/usr/local/bin/k3s",
                "kubectl",
                "delete",
                "--raw",
                &path,
                "-f",
                deletion.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        // The controller's ordinary delete may already have completed. This
        // checks fixture API cleanup only; original process absence was checked
        // above, and a replacement UID still fails the delete precondition.
        assert!(
            cleanup.status.success()
                || String::from_utf8_lossy(&cleanup.stderr).contains("(NotFound)"),
            "fixture cleanup: {}",
            String::from_utf8_lossy(&cleanup.stderr)
        );
        results.push(json!({"case":case,"journal":entries,"report":report,"physical_processes":physical,"state":state}));
        println!("BROWSER_CONTROLLER_CASE_PASSED {case}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    fs::write(
        config["result_file"].as_str().unwrap(),
        serde_json::to_vec_pretty(&results).unwrap(),
    )
    .unwrap();
}
