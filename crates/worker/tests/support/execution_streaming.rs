//! Real v3 output while commands run, plus interruption and fixed publication recovery.
use super::*;
use renewal::Context;

async fn page(server: &output_http::Server, token: &str, id: &str, after: u32) -> Value {
    let (status, _, bytes) = server
        .get_live(
            token,
            &format!("/v1alpha1/executions/{id}/output-chunks?after_sequence={after}&limit=32"),
        )
        .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}
async fn prefix(server: &output_http::Server, token: &str, id: &str) -> (Value, [Vec<u8>; 2]) {
    let mut after = 0;
    let mut outputs: [Vec<u8>; 2] = Default::default();
    let mut all = vec![];
    let last = loop {
        let p = page(server, token, id, after).await;
        for chunk in p["chunks"].as_array().unwrap() {
            let sequence = chunk["sequence"].as_u64().unwrap() as u32;
            assert_eq!(sequence, after + 1);
            let (status, headers, bytes) = server
                .get_live(
                    token,
                    &format!("/v1alpha1/executions/{id}/output-chunks/{sequence}"),
                )
                .await;
            assert_eq!(status, 200);
            assert_eq!(
                bytes.len() as u64,
                chunk["retained_bytes"].as_u64().unwrap()
            );
            let hash = agent_computer_objects::sha256(&bytes);
            assert_eq!(hash, chunk["sha256"]);
            assert!(headers.contains(&format!("x-output-sha256: {hash}")));
            let index = if chunk["stream"] == "stdout" {
                0
            } else {
                assert_eq!(chunk["stream"], "stderr");
                1
            };
            assert_eq!(
                outputs[index].len() as u64,
                chunk["offset"].as_u64().unwrap()
            );
            outputs[index].extend(bytes);
            all.push(chunk.clone());
            after = sequence;
        }
        assert_eq!(p["next_sequence"], after);
        if after >= p["available_sequence"].as_u64().unwrap() as u32 {
            break p;
        }
    };
    (
        json!({"page":last,"chunks":all,"stdout_sha256":agent_computer_objects::sha256(&outputs[0]),"stderr_sha256":agent_computer_objects::sha256(&outputs[1]),"stdout_bytes":outputs[0].len(),"stderr_bytes":outputs[1].len()}),
        outputs,
    )
}

pub async fn run(c: Context<'_>, cases: &[(String, String, String)]) -> Vec<Value> {
    let server = output_http::start(c.config, c.private).await;
    let mut results = vec![];
    for (name, computer, sandbox) in cases {
        let name = name.as_str();
        let success = matches!(name, "stream-long" | "stream-fixed" | "stream-flood");
        let pending = matches!(name, "stream-store-failure" | "stream-db-failure");
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
                    scope: WriterScope::Modify,
                    connection_session_id: session.session_id.clone(),
                    candidate_id: start.candidate_id.clone(),
                    generation: start.generation,
                    duration_seconds: 30,
                },
            )
            .await
            .unwrap();
        let script = match name {
            "stream-long" => {
                r"printf started > started.txt; /bin/sync started.txt; printf '\000\377early\n'; printf 'err\000\377' >&2; /bin/sleep 38; printf tail; printf done > output.txt; /bin/sync output.txt"
            }
            "stream-fixed" => {
                r"printf '\000\377early\n'; printf 'err\000\377' >&2; /bin/sleep 5; printf tail; printf done > output.txt; /bin/sync output.txt"
            }
            "stream-flood" => {
                r"/bin/head -c 1100000 /dev/zero; /bin/head -c 1100000 /dev/zero >&2; /bin/sleep 5; printf done > output.txt; /bin/sync output.txt"
            }
            _ => {
                r"printf started > started.txt; /bin/sync started.txt; printf '\000\377early\n'; printf 'err\000\377' >&2; /bin/sleep 60; printf unexpected > late.txt; /bin/sync late.txt"
            }
        };
        let queued = c
            .store
            .submit_candidate_execution(
                c.token,
                &key(&format!("execute-{name}")),
                computer,
                &SubmitExecution {
                    renewable: if name == "stream-fixed" {
                        Some(false)
                    } else {
                        None
                    },
                    stream_output: None,
                    lease_id: lease.lease_id.clone(),
                    lease: WriterLeaseCommand {
                        connection_session_id: session.session_id.clone(),
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
                        output_limit_bytes: if name == "stream-flood" {
                            1048576
                        } else {
                            4096
                        },
                    },
                },
            )
            .await
            .unwrap();
        assert!(queued.stream_output);
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
        let mut worker = c.worker.clone();
        if name == "stream-store-failure" {
            worker["outputs"]["credentials_file"] =
                c.config["rejected_output_credentials_file"].clone();
        }
        if name == "stream-db-failure" {
            sqlx::raw_sql("CREATE FUNCTION reject_stream_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='execution.output_chunk_verified' THEN RAISE EXCEPTION 'fixture chunk acknowledgement failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_stream_ack BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION reject_stream_ack();").execute(c.pool).await.unwrap();
        }
        let running = async {
            if name == "stream-controller-kill" {
                faults::kill_controller(c.config, c.private, c.store, c.org, id, &data, name).await
            } else {
                let outcome = execution::execute_once(
                    c.store,
                    c.client,
                    c.org,
                    id,
                    queued.revision,
                    serde_json::from_value(worker).unwrap(),
                )
                .await
                .unwrap();
                eprintln!("stream fixture {name}: {outcome:?}");
                serde_json::to_value(outcome).unwrap()
            }
        };
        let observing = async {
            if pending || name == "stream-controller-kill" {
                return Value::Null;
            }
            let until = tokio::time::Instant::now() + Duration::from_secs(25);
            let first = loop {
                let p = page(&server, c.token, id, 0).await;
                if p["available_sequence"].as_u64().unwrap() > 0 {
                    break p;
                }
                assert!(
                    tokio::time::Instant::now() < until,
                    "no early output: {name}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            assert_eq!(first["execution_state"], "Dispatching");
            assert!(!first["final_report_verified"].as_bool().unwrap());
            assert!(!data.join("output.txt").exists());
            let (status, _, bytes) = server
                .get_live(
                    c.token,
                    &format!("/v1alpha1/executions/{id}/output-chunks/1"),
                )
                .await;
            assert_eq!(status, 200);
            assert!(!bytes.is_empty());
            assert_eq!(
                server
                    .get_live(
                        "invalid",
                        &format!("/v1alpha1/executions/{id}/output-chunks/1")
                    )
                    .await
                    .0,
                401
            );
            let closed = c
                .store
                .close_connection_session(c.token, &session.session_id)
                .await
                .unwrap();
            assert_eq!(closed.state, ConnectionState::Closed);
            if name == "stream-cancel" {
                let state = c.store.candidate_execution(c.token, id).await.unwrap();
                c.store
                    .cancel_candidate_execution(
                        c.token,
                        &key("stream-cancel"),
                        id,
                        &CancelExecution {
                            expected_revision: state.revision,
                        },
                    )
                    .await
                    .unwrap();
            }
            json!({"while_dispatching":true,"first_page":first,"early_bytes":bytes.len(),"early_sha256":agent_computer_objects::sha256(&bytes),"disconnected":true})
        };
        let (outcome, early) = tokio::join!(running, observing);
        if name == "stream-db-failure" {
            sqlx::raw_sql(
                "DROP TRIGGER reject_stream_ack ON events; DROP FUNCTION reject_stream_ack();",
            )
            .execute(c.pool)
            .await
            .unwrap();
        }
        let before = page(&server, c.token, id, 0).await;
        let mut publication = Value::Null;
        if pending {
            assert_eq!(before["available_sequence"], 0);
            assert_eq!(outcome["output_unconfirmed"], true);
            let mut recovery = c.private.clone();
            recovery["ca_file"] = json!("/unavailable-kubernetes-ca");
            recovery["token_file"] = json!("/unavailable-kubernetes-token");
            let config_path = PathBuf::from(field(c.config, "result_file"))
                .with_file_name(format!("{name}-recover.json"));
            fs::write(&config_path, serde_json::to_vec(&recovery).unwrap()).unwrap();
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
            let command = std::process::Command::new(field(c.config, "server_binary"))
                .args([
                    "execution-output-chunks-recover",
                    "--database-url-file",
                    field(c.config, "database_url_file"),
                    "--organization",
                    c.org.as_str(),
                    "--execution-id",
                    id,
                    "--config-file",
                ])
                .arg(&config_path)
                .output()
                .unwrap();
            assert!(
                command.status.success(),
                "{}",
                String::from_utf8_lossy(&command.stderr)
            );
            publication = serde_json::from_slice(&command.stdout).unwrap();
            assert_eq!(publication["recovered_chunk"]["sequence"], 1);
            fs::remove_file(config_path).unwrap();
        }
        let (prefix, bytes) = prefix(&server, c.token, id).await;
        assert!(prefix["page"]["available_sequence"].as_u64().unwrap() > 0);
        let state = c.store.candidate_execution(c.token, id).await.unwrap();
        if success {
            assert_eq!(state.state, ExecutionState::Succeeded, "{outcome}");
            assert_eq!(fs::read(data.join("output.txt")).unwrap(), b"done");
            assert_eq!(prefix["page"]["final_report_verified"], true);
            assert_eq!(
                prefix["page"]["final_sequence"],
                prefix["page"]["available_sequence"]
            );
            for (index, stream) in ["stdout", "stderr"].into_iter().enumerate() {
                let (status, _, final_bytes) = server
                    .get_live(
                        c.token,
                        &format!("/v1alpha1/executions/{id}/output/{stream}"),
                    )
                    .await;
                assert_eq!(status, 200);
                assert_eq!(final_bytes, bytes[index]);
            }
            if name == "stream-flood" {
                for output in &bytes {
                    assert_eq!(output.len(), 1048576);
                    assert!(output.iter().all(|b| *b == 0));
                }
                assert!(
                    prefix["chunks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v["truncated"] == true)
                );
            } else {
                assert_eq!(bytes[0], b"\0\xffearly\ntail");
                assert_eq!(bytes[1], b"err\0\xff");
            }
        } else {
            assert!(
                matches!(
                    state.state,
                    ExecutionState::Unknown | ExecutionState::Cancelled
                ),
                "{state:?} {outcome}"
            );
            assert!(!data.join("late.txt").exists());
            assert_eq!(prefix["page"]["final_report_verified"], false);
        }
        let renewals: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM execution_renewal_acks WHERE organization=$1 AND execution_id=$2",
        )
        .bind(c.org.as_str())
        .bind(id)
        .fetch_one(c.pool)
        .await
        .unwrap();
        if name == "stream-long" {
            assert!(renewals >= 1);
        }
        if name == "stream-fixed" {
            assert_eq!(renewals, 0);
        }
        let recovery = execution::recover_once(
            c.store,
            c.client,
            c.org,
            id,
            c.storage,
            c.image,
            std::path::Path::new(field(&c.config["node"], "spool")),
        )
        .await
        .unwrap();
        let grants:i64=sqlx::query_scalar("SELECT count(*) FROM execution_startup_grants WHERE organization=$1 AND execution_id=$2").bind(c.org.as_str()).bind(id).fetch_one(c.pool).await.unwrap();
        assert_eq!(grants, 1);
        assert_eq!(
            prefix["page"]["available_sequence"],
            page(&server, c.token, id, 0).await["available_sequence"]
        );
        results.push(json!({"case":name,"execution_id":id,"prepared":prepared,"state":state,"outcome":outcome,"early":early,"prefix":prefix,"publication_recovery":publication,"renewal_acks":renewals,"startup_grants":grants,"recovery":recovery}));
    }
    results
}
