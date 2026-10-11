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
async fn manual_browser_control_live() {
    let config: Value = serde_json::from_slice(
        &fs::read(
            std::env::var("AGENT_COMPUTER_BROWSER_CONTROL_FIXTURE").expect("explicit VM fixture"),
        )
        .unwrap(),
    )
    .unwrap();
    let mut results = Vec::new();
    for case in ["handoff", "completion-loss", "controller-kill"] {
        let (db, token, computer, input) =
            setup_browser(Some(config["image"].as_str().unwrap())).await;
        let app = admit(&db, &token, &computer, &input).await;
        for (kind, id) in [
            (RuntimeKind::Computer, computer.as_str()),
            (RuntimeKind::App, app.app_id.as_str()),
        ] {
            for permission in [RuntimePermission::Observe, RuntimePermission::Control] {
                allow(&db, id, kind, permission, None).await;
            }
        }
        allow(
            &db,
            &app.profile_id,
            RuntimeKind::BrowserProfile,
            RuntimePermission::Read,
            None,
        )
        .await;
        let gui = super::control::connection(&db, &token, &computer, "control-gui").await;
        let next_gui = super::control::connection(&db, &token, &computer, "control-next-gui").await;
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
        let mut api = api_start(&dbfile).await;
        let base = format!("/v1alpha1/app-instances/{}/browser", app.instance_id);
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
        let input = json!({"connection_session_id":gui,"app_generation":1,"expected_epoch":0,"lifetime_ms":25_000});
        let (status, control) = http(
            api.address,
            &token,
            "POST",
            &format!("{base}/control"),
            Some("control"),
            input,
        );
        if status != 200 {
            let diagnostic = db
                .store
                .acquire_browser_control(
                    &token,
                    &key("control"),
                    &app.instance_id,
                    &agent_computer_store::runtime::apps::browser::control::AcquireControl {
                        connection_session_id: gui.clone(),
                        app_generation: 1,
                        expected_epoch: 0,
                        lifetime_ms: 25_000,
                    },
                )
                .await;
            panic!(
                "HTTP control returned {status}: {control}; same-key store diagnostic: {diagnostic:?}"
            );
        }
        assert_eq!(control["control_epoch"], 1);
        let pages = gui_command(
            &api,
            &token,
            &base,
            &gui,
            1,
            "pages",
            json!({"kind":"pages"}),
        )
        .await;
        assert_eq!(pages["state"], "Complete");
        let old_page = pages["result"][0]["page_id"].as_str().unwrap();
        let old = gui_command(
            &api,
            &token,
            &base,
            &gui,
            1,
            "observe",
            json!({"kind":"observe","page_id":old_page}),
        )
        .await;
        assert_eq!(old["state"], "Complete");
        assert_eq!(old["result"]["scope"]["app_instance_id"], app.instance_id);
        assert!(
            old["result"]["screenshot_base64"]
                .as_str()
                .unwrap()
                .starts_with("iVBORw0KGgo")
        );
        let evidence = if case == "handoff" {
            let action = gui_command(
                &api,
                &token,
                &base,
                &gui,
                1,
                "create-page",
                json!({"kind":"act","action":{"kind":"create_page"}}),
            )
            .await;
            assert_eq!(action["state"], "Complete");
            assert_eq!(action["result"]["receipt"]["outcome"], "delivered");
            let after = gui_command(
                &api,
                &token,
                &base,
                &gui,
                1,
                "pages-after",
                json!({"kind":"pages"}),
            )
            .await;
            assert_eq!(after["result"].as_array().unwrap().len(), 2);
            let retry=api.json(&token,"POST",&format!("{base}/commands"),Some("create-page"),json!({"connection_session_id":gui,"app_generation":1,"control_epoch":1,"command":{"kind":"act","action":{"kind":"create_page"}}}),202);
            assert_eq!(retry["request_id"], action["request_id"]);
            let released = api.json(
                &token,
                "POST",
                &format!("{base}/control/release"),
                None,
                json!({"connection_session_id":gui,"control_epoch":1}),
                202,
            );
            assert_eq!(released["state"], "Draining");
            let limit = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let c = api.json(
                    &token,
                    "GET",
                    &format!("{base}/control"),
                    None,
                    Value::Null,
                    200,
                );
                if c["state"] == "Idle" {
                    break;
                }
                assert!(tokio::time::Instant::now() < limit, "barrier missing: {c}");
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let new=api.json(&token,"POST",&format!("{base}/control"),Some("next-control"),json!({"connection_session_id":next_gui,"app_generation":1,"expected_epoch":1,"lifetime_ms":10_000}),200);
            assert_eq!(new["control_epoch"], 2);
            api.json(
                &token,
                "GET",
                &format!(
                    "/v1alpha1/browser-commands/{}",
                    old["request_id"].as_str().unwrap()
                ),
                None,
                Value::Null,
                409,
            );
            let stale=gui_command(&api,&token,&base,&next_gui,2,"old-frame",json!({"kind":"act","action":{"kind":"click_at","page_id":old_page,"observation_id":old["result"]["observation_id"],"x":10,"y":10}})).await;
            assert_eq!(stale["state"], "Rejected");
            let fresh = gui_command(
                &api,
                &token,
                &base,
                &next_gui,
                2,
                "new-frame",
                json!({"kind":"observe","page_id":old_page}),
            )
            .await;
            assert_eq!(fresh["result"]["control_epoch"], 2);
            let close=gui_command(&api,&token,&base,&next_gui,2,"close-page",json!({"kind":"act","action":{"kind":"close_page","page_id":old_page,"observation_id":fresh["result"]["observation_id"]}})).await;
            assert_eq!(close["result"]["receipt"]["outcome"], "delivered");
            let final_pages = gui_command(
                &api,
                &token,
                &base,
                &next_gui,
                2,
                "final-pages",
                json!({"kind":"pages"}),
            )
            .await;
            assert_eq!(final_pages["result"].as_array().unwrap().len(), 1);
            db.store
                .drain_browser_app(&org("acme"), &app.instance_id)
                .await
                .unwrap();
            json!({"create":action,"after_count":2,"new_epoch":2,"stale":stale,"close":close,"final_count":1})
        } else if case == "completion-loss" {
            // Actual driver side effect, then stall only its database completion.
            sqlx::raw_sql("CREATE FUNCTION delay_gui_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='complete' THEN PERFORM pg_sleep(8); END IF; RETURN NEW; END; $$; CREATE TRIGGER delay_gui_receipt BEFORE INSERT ON browser_action_journal FOR EACH ROW EXECUTE FUNCTION delay_gui_receipt();").execute(&db.pool).await.unwrap();
            let command=api.json(&token,"POST",&format!("{base}/commands"),Some("lost-completion"),json!({"connection_session_id":gui,"app_generation":1,"control_epoch":1,"command":{"kind":"act","action":{"kind":"create_page"}}}),202);
            tokio::time::sleep(Duration::from_secs(9)).await;
            let status = api.json(
                &token,
                "GET",
                &format!(
                    "/v1alpha1/browser-commands/{}",
                    command["request_id"].as_str().unwrap()
                ),
                None,
                Value::Null,
                200,
            );
            assert_eq!(status["state"], "Unknown");
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM browser_action_journal WHERE kind='begin'"
                )
                .fetch_one(&db.pool)
                .await
                .unwrap(),
                1
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM browser_action_journal WHERE kind='complete'"
                )
                .fetch_one(&db.pool)
                .await
                .unwrap(),
                0
            );
            let retry=api.json(&token,"POST",&format!("{base}/commands"),Some("lost-completion"),json!({"connection_session_id":gui,"app_generation":1,"control_epoch":1,"command":{"kind":"act","action":{"kind":"create_page"}}}),202);
            assert_eq!(retry["request_id"], command["request_id"]);
            json!({"status":status,"begin_count":1,"complete_count":0,"replayed":false})
        } else {
            command(&[
                "systemctl",
                "kill",
                "--signal=SIGKILL",
                "--kill-whom=main",
                &unit,
            ]);
            json!({"controller_killed":true})
        };
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
            assert!(matches!(case, "controller-kill" | "completion-loss"));
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
        if case == "handoff" {
            assert!(stopped);
        }
        if case == "controller-kill" {
            assert!(!stopped);
        }
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
        api.stop();
        results.push(json!({"gui":evidence,"case":case,"journal":entries,"report":report,"physical_processes":physical,"state":state}));
        println!("BROWSER_CONTROL_CASE_PASSED {case}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    fs::write(
        config["result_file"].as_str().unwrap(),
        serde_json::to_vec_pretty(&results).unwrap(),
    )
    .unwrap();
}

struct Api {
    child: std::process::Child,
    address: std::net::SocketAddr,
}
impl Api {
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
    fn json(
        &self,
        token: &str,
        method: &str,
        path: &str,
        key: Option<&str>,
        body: Value,
        expected: u16,
    ) -> Value {
        let (status, value) = http(self.address, token, method, path, key, body);
        assert_eq!(status, expected, "{method} {path}: {value}");
        value
    }
}
impl Drop for Api {
    fn drop(&mut self) {
        self.stop();
    }
}
fn http(
    address: std::net::SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Value,
) -> (u16, Value) {
    use std::io::{Read, Write};
    let mut stream =
        std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let key = key
        .map(|v| format!("Idempotency-Key: {v}\r\n"))
        .unwrap_or_default();
    write!(stream,"{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nAuthorization: Bearer {token}\r\n{key}Content-Length: {}\r\n\r\n{body}",body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}
async fn api_start(dbfile: &Path) -> Api {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let child = Command::new("/usr/local/bin/agent-computer-server")
        .args([
            "serve",
            "--database-url-file",
            dbfile.to_str().unwrap(),
            "--listen",
            &address.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut api = Api { child, address };
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(api.child.try_wait().unwrap().is_none());
        if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
            return api;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn gui_command(
    api: &Api,
    token: &str,
    base: &str,
    session: &str,
    epoch: i64,
    key: &str,
    command: Value,
) -> Value {
    let admitted=api.json(token,"POST",&format!("{base}/commands"),Some(key),json!({"connection_session_id":session,"app_generation":1,"control_epoch":epoch,"command":command}),202);
    let id = admitted["request_id"].as_str().unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let current = api.json(
            token,
            "GET",
            &format!("/v1alpha1/browser-commands/{id}"),
            None,
            Value::Null,
            200,
        );
        if !matches!(current["state"].as_str(), Some("Queued" | "Dispatched")) {
            return current;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "GUI command stuck: {current}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
