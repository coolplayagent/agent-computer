//! Kill the real controller at a database barrier after its live drain. Only
//! the original private node receipt may release the old writer after restart.
use super::*;
use renewal::Context;
use std::{
    path::Path,
    process::{Child, Command, Stdio},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn config_file(c: &Context<'_>, name: &str, value: &Value) -> PathBuf {
    let path = PathBuf::from(field(c.config, "result_file")).with_file_name(format!("{name}.json"));
    fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}
fn command(c: &Context<'_>, name: &str, config: &Path) -> Command {
    let mut command = Command::new(field(c.config, "server_binary"));
    command
        .args([
            name,
            "--database-url-file",
            field(c.config, "database_url_file"),
            "--organization",
            c.org.as_str(),
            "--config-file",
        ])
        .arg(config);
    command
}
fn spawn(mut command: Command, log: &Path) -> Process {
    let file = fs::File::create(log).unwrap();
    fs::set_permissions(log, fs::Permissions::from_mode(0o600)).unwrap();
    Process(
        command
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap(),
    )
}
async fn marker(child: &mut Process, data: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while fs::read(data.join("started.txt")).ok().as_deref() != Some(b"started") {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "controller exited before marker"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "start marker deadline"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn run(c: Context<'_>, cases: &[(String, String, String)]) -> Vec<Value> {
    let mut results = vec![];
    for (name, computer, sandbox) in cases {
        let start = c
            .store
            .admit_computer_start(
                c.token,
                &key(&format!("start-{name}")),
                computer,
                &StartRequest {
                    expected_revision: 1,
                    expected_spec_revision: 1,
                    max_runtime_seconds: 300,
                    input_artifact_id: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            candidate::prepare_once(
                c.store,
                c.org,
                &start.request_id,
                c.owner,
                serde_json::from_value(c.local.clone()).unwrap()
            )
            .await
            .unwrap(),
            candidate::WorkResult::Prepared
        ));
        let session = c
            .store
            .create_connection_session(
                c.token,
                &key(&format!("session-{name}")),
                computer,
                &ConnectRequest {
                    requested_capabilities: vec![
                        RuntimePermission::Connect,
                        RuntimePermission::Read,
                        RuntimePermission::Modify,
                    ],
                    lifetime_seconds: 300,
                },
            )
            .await
            .unwrap();
        let lease = c
            .store
            .acquire_candidate_writer(
                c.token,
                &key(&format!("lease-{name}")),
                computer,
                &AcquireWriterLease {
                    connection_session_id: session.session_id,
                    generation: start.generation,
                    candidate_id: start.candidate_id.clone(),
                    scope: WriterScope::Modify,
                    duration_seconds: 30,
                },
            )
            .await
            .unwrap();
        let script = if name == "drain-unknown" {
            "printf started > started.txt; /bin/sync started.txt; /bin/sleep 2; printf done"
        } else {
            "printf started > started.txt; /bin/sync started.txt; /bin/sleep 60; printf unexpected > late.txt; /bin/sync late.txt"
        };
        let queued = c
            .store
            .submit_candidate_execution(
                c.token,
                &key(&format!("execute-{name}")),
                computer,
                &SubmitExecution {
                    renewable: Some(false),
                    stream_output: Some(false),
                    lease_id: lease.lease_id.clone(),
                    lease: WriterLeaseCommand {
                        connection_session_id: lease.connection_session_id.clone(),
                        generation: lease.generation,
                        epoch: lease.epoch,
                        expected_revision: lease.revision,
                    },
                    sandbox_id: sandbox.clone(),
                    lifetime: ExecutionLifetime::Background,
                    command: ExecutionCommand {
                        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
                        cwd: String::new(),
                        timeout_seconds: 65,
                        term_grace_ms: 100,
                        output_limit_bytes: 4096,
                    },
                },
            )
            .await
            .unwrap();
        let id = &queued.execution_id;
        let prepared: Value = sqlx::query_scalar(
            "SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(c.org.as_str())
        .bind(&start.request_id)
        .fetch_one(c.pool)
        .await
        .unwrap();
        let data = c.root.join(field(&prepared, "path_ref"));
        let spool = Path::new(field(&c.worker["node"], "spool"));
        if name == "drain-unsealed" {
            let before =
                faults::kill_controller(c.config, c.private, c.store, c.org, id, &data, name).await;
            assert!(
                execution::recover_completion(c.store, c.org, id, spool)
                    .await
                    .unwrap()
                    .is_none()
            );
            let state = c.store.candidate_execution(c.token, id).await.unwrap();
            assert_eq!(state.state, ExecutionState::Unknown);
            let drains:i64=sqlx::query_scalar("SELECT count(*) FROM candidate_writer_drains WHERE organization=$1 AND lease_id=$2").bind(c.org.as_str()).bind(&lease.lease_id).fetch_one(c.pool).await.unwrap();
            assert_eq!(drains, 0);
            results.push(json!({"case":name,"execution_id":id,"prepared":prepared,"completion":null,"state":state.state,"drain_count":drains,"fault":before}));
            continue;
        }
        // The trigger blocks only this disposable fixture's completion event.
        // It runs after the original node seal is fsynced, inside the transaction.
        let mut barrier = c.pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock(617111)")
            .execute(&mut *barrier)
            .await
            .unwrap();
        sqlx::raw_sql("CREATE FUNCTION pause_drain_publication() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='execution.completed' THEN PERFORM pg_advisory_xact_lock(617111); END IF; RETURN NEW; END; $$; CREATE TRIGGER pause_drain_publication BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION pause_drain_publication();").execute(c.pool).await.unwrap();
        let path = config_file(&c, name, c.private);
        let mut dispatch = command(&c, "execution-dispatch-once", &path);
        dispatch.args(["--execution-id", id, "--expected-revision", "1"]);
        let log = path.with_extension("log");
        let mut child = spawn(dispatch, &log);
        marker(&mut child, &data).await;
        let normal = json!({"case":"cancel","execution_id":id,"prepared":prepared});
        let checkpoint = if name == "drain-cancel" {
            Some(
                checkpoint::begin(
                    &checkpoint::Context {
                        store: c.store,
                        pool: c.pool,
                        org: c.org,
                        actor: c.actor,
                        token: c.token,
                        config: c.config,
                        local: c.local,
                        normal: &normal,
                    },
                    true,
                )
                .await,
            )
        } else {
            None
        };
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=0 AND objid=617111 AND NOT granted)").fetch_one(c.pool).await.unwrap();
            if blocked {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "controller exited before SQL drain barrier"
            );
            assert!(
                tokio::time::Instant::now() < until,
                "completion barrier deadline"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        sqlx::query("SELECT pg_advisory_unlock(617111)")
            .execute(&mut *barrier)
            .await
            .unwrap();
        drop(barrier);
        sqlx::raw_sql("DROP TRIGGER pause_drain_publication ON events; DROP FUNCTION pause_drain_publication();").execute(c.pool).await.unwrap();
        assert!(
            c.store
                .candidate_execution_completion(c.org, id)
                .await
                .unwrap()
                .is_none()
        );
        let arm = c
            .store
            .candidate_execution_watchdog(c.org, id)
            .await
            .unwrap()
            .unwrap();
        let sealed = agent_computer_node::read_recorded_seal(spool, &arm.evidence)
            .unwrap()
            .unwrap();
        assert_eq!(sealed.evidence()["io"]["prepared"], prepared);
        assert!(matches!(
            c.store
                .recover_candidate_execution_completion(c.org, id, &sealed)
                .await,
            Err(Error::WriterLeaseInactive)
        ));
        let state: String = sqlx::query_scalar(
            "SELECT state FROM execution_requests WHERE organization=$1 AND execution_id=$2",
        )
        .bind(c.org.as_str())
        .bind(id)
        .fetch_one(c.pool)
        .await
        .unwrap();
        assert_eq!(
            state,
            if checkpoint.is_some() {
                "CancelRequested"
            } else {
                "Dispatching"
            }
        );
        // Damaged or temporarily missing local evidence cannot be replaced by
        // a serialized SQL arm, output object, absent Pod or expired deadline.
        let receipt_path = fs::read_dir(spool)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("drain-")
                    && p.extension().is_some_and(|x| x == "json")
                    && fs::read(p)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                        .is_some_and(|v| v["seal"]["arm"] == arm.evidence)
            })
            .unwrap();
        let original = fs::read(&receipt_path).unwrap();
        let retained = receipt_path.with_extension("retained");
        fs::rename(&receipt_path, &retained).unwrap();
        assert!(
            execution::recover_completion(c.store, c.org, id, spool)
                .await
                .unwrap()
                .is_none()
        );
        fs::write(&receipt_path, b"{").unwrap();
        fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            execution::recover_completion(c.store, c.org, id, spool)
                .await
                .is_err()
        );
        fs::remove_file(&receipt_path).unwrap();
        fs::rename(&retained, &receipt_path).unwrap();
        assert_eq!(fs::read(&receipt_path).unwrap(), original);
        let dispatch = c
            .store
            .candidate_execution_dispatch(c.org, id)
            .await
            .unwrap();
        sqlx::query("SELECT pg_sleep(GREATEST(0,($1::bigint-floor(extract(epoch from clock_timestamp())*1000)::bigint)::double precision/1000.0)+0.01)").bind(dispatch.deadline_at_ms).execute(c.pool).await.unwrap();
        let mut daemon = spawn(
            command(&c, "execution-worker", &path),
            &path.with_extension("recovery.log"),
        );
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        let recovered = loop {
            if let Some(r) = c
                .store
                .candidate_execution_completion(c.org, id)
                .await
                .unwrap()
            {
                break r;
            }
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "recovery worker exited"
            );
            assert!(
                tokio::time::Instant::now() < until,
                "automatic drain recovery deadline"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(
            Command::new("kill")
                .args(["-TERM", &daemon.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        loop {
            if let Some(status) = daemon.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "recovery shutdown deadline"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let events: Vec<Value> = fs::read_to_string(path.with_extension("recovery.log"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(
            events.iter().any(
                |e| e["event"] == "completion_recovered" && e["receipt"]["execution_id"] == *id
            )
        );
        assert_eq!(events.last().unwrap()["summary"]["claimed"], 0);
        assert_eq!(
            recovered.accepted_state,
            if checkpoint.is_some() {
                ExecutionState::Cancelled
            } else {
                ExecutionState::Unknown
            }
        );
        let (a, b) = tokio::join!(
            execution::recover_completion(c.store, c.org, id, spool),
            execution::recover_completion(c.store, c.org, id, spool)
        );
        assert_eq!(a.unwrap().unwrap(), recovered);
        assert_eq!(b.unwrap().unwrap(), recovered);
        let mut offline = c.private.clone();
        offline["ca_file"] = json!("/unavailable-kubernetes-ca");
        offline["token_file"] = json!("/unavailable-kubernetes-token");
        let offline = config_file(&c, &format!("{name}-offline"), &offline);
        let result = command(&c, "execution-completion-recover", &offline)
            .args(["--execution-id", id])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap()["completion"],
            serde_json::to_value(&recovered).unwrap()
        );
        let counts:Value=sqlx::query_scalar("SELECT jsonb_build_object('dispatches',(SELECT count(*) FROM execution_dispatch_intents WHERE organization=$1 AND execution_id=$2),'grants',(SELECT count(*) FROM execution_startup_grants WHERE organization=$1 AND execution_id=$2),'completions',(SELECT count(*) FROM execution_completions WHERE organization=$1 AND execution_id=$2),'drains',(SELECT count(*) FROM candidate_writer_drains WHERE organization=$1 AND lease_id=$3))").bind(c.org.as_str()).bind(id).bind(&lease.lease_id).fetch_one(c.pool).await.unwrap();
        assert_eq!(
            counts,
            json!({"dispatches":1,"grants":1,"completions":1,"drains":1})
        );
        assert!(!data.join("late.txt").exists());
        let restored = if checkpoint.is_some() {
            checkpoint::verify(checkpoint::Context {
                store: c.store,
                pool: c.pool,
                org: c.org,
                actor: c.actor,
                token: c.token,
                config: c.config,
                local: c.local,
                normal: &normal,
            })
            .await
        } else {
            let drained: bool = sqlx::query_scalar("SELECT artifact_candidate_drained($1,$2)")
                .bind(c.org.as_str())
                .bind(&start.request_id)
                .fetch_one(c.pool)
                .await
                .unwrap();
            assert!(!drained);
            Value::Null
        };
        results.push(json!({"case":name,"execution_id":id,"prepared":prepared,"completion":recovered,"counts":counts,"missing_and_corrupt_refused":true,"late_file_absent":true,"checkpoint":restored,"worker_events":events}));
    }
    results
}
