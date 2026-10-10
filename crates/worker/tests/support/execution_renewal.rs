//! Real v2 renewals across PostgreSQL, attach, gVisor, two guards and the reaper.
use super::*;
use std::path::Path;

pub struct Context<'a> {
    pub store: &'a Store,
    pub pool: &'a sqlx::PgPool,
    pub client: &'a Client,
    pub org: &'a OrganizationId,
    pub actor: &'a PrincipalId,
    pub token: &'a str,
    pub config: &'a Value,
    pub worker: &'a Value,
    pub private: &'a Value,
    pub local: &'a Value,
    pub root: &'a Path,
    pub owner: &'a WorkerId,
    pub storage: &'a StorageClassBinding,
    pub image: &'a str,
}
async fn acknowledged(pool: &sqlx::PgPool, org: &OrganizationId, id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM execution_renewal_acks WHERE organization=$1 AND execution_id=$2",
    )
    .bind(org.as_str())
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}
pub async fn run(c: Context<'_>, cases: &[(String, String, String)]) -> Vec<Value> {
    let mut results = Vec::new();
    for (name, computer, sandbox) in cases {
        let name = name.as_str();
        let start = c
            .store
            .admit_computer_start(
                c.token,
                &key(&format!("start-{name}")),
                computer,
                &StartRequest {
                    expected_revision: 1,
                    expected_spec_revision: 1,
                    max_runtime_seconds: if name == "renew-hard" { 35 } else { 300 },
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
                &key(&format!("connect-{name}")),
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
                    candidate_id: start.candidate_id.clone(),
                    generation: start.generation,
                    scope: WriterScope::Modify,
                    duration_seconds: 30,
                },
            )
            .await
            .unwrap();
        let script = match name {
            "renew-short" => "printf persisted > output.txt; /bin/sync output.txt; printf done",
            "renew-long" => {
                "printf started > started.txt; /bin/sync started.txt; /bin/sleep 38; printf persisted > output.txt; /bin/sync output.txt; printf done"
            }
            _ => {
                "printf started > started.txt; /bin/sync started.txt; /bin/sleep 60; printf unexpected > late.txt; /bin/sync late.txt"
            }
        };
        let queued = c
            .store
            .submit_candidate_execution(
                c.token,
                &key(&format!("execution-{name}")),
                computer,
                &SubmitExecution {
                    renewable: None,
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
                        timeout_seconds: if name == "renew-hard" { 35 } else { 65 },
                        term_grace_ms: 100,
                        output_limit_bytes: 4096,
                    },
                },
            )
            .await
            .unwrap();
        assert!(queued.renewable);
        let prepared: Value = sqlx::query_scalar(
            "SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(c.org.as_str())
        .bind(&start.request_id)
        .fetch_one(c.pool)
        .await
        .unwrap();
        let data = c.root.join(field(&prepared, "path_ref"));
        let began = tokio::time::Instant::now();
        let mut fault = Value::Null;
        let mut outcome = if name == "renew-controller-kill" {
            faults::kill_controller(
                c.config,
                c.private,
                c.store,
                c.org,
                &queued.execution_id,
                &data,
                name,
            )
            .await
        } else {
            let running = async {
                let result = execution::execute_once(
                    c.store,
                    c.client,
                    c.org,
                    &queued.execution_id,
                    queued.revision,
                    serde_json::from_value(c.worker.clone()).unwrap(),
                )
                .await;
                eprintln!("renewal fixture {name}: {result:?}");
                result
            };
            let intervention = async {
                if matches!(name, "renew-short" | "renew-hard") {
                    return Value::Null;
                }
                let until = tokio::time::Instant::now() + Duration::from_secs(35);
                while acknowledged(c.pool, c.org, &queued.execution_id).await < 1 {
                    assert!(
                        tokio::time::Instant::now() < until,
                        "{name}: no first durable renewal"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let before = c
                    .store
                    .reconcile_candidate_writer(c.org, &lease.lease_id)
                    .await
                    .unwrap();
                assert!(before.expires_at_ms > lease.expires_at_ms);
                let details = match name {
                    "renew-long" => {
                        let closed = c
                            .store
                            .close_connection_session(c.token, &lease.connection_session_id)
                            .await
                            .unwrap();
                        let after = c
                            .store
                            .reconcile_candidate_writer(c.org, &lease.lease_id)
                            .await
                            .unwrap();
                        assert_eq!(before.lease_id, after.lease_id);
                        assert_eq!(before.epoch, after.epoch);
                        assert_eq!(before.revision, after.revision);
                        assert_eq!(before.expires_at_ms, after.expires_at_ms);
                        assert_eq!(before.state, after.state);
                        assert_eq!(closed.state, ConnectionState::Closed);
                        json!({"connection_closed_after_renewal":closed,"writer_before":before,"writer_after":after})
                    }
                    "renew-cancel" => {
                        let current = c
                            .store
                            .candidate_execution(c.token, &queued.execution_id)
                            .await
                            .unwrap();
                        serde_json::to_value(
                            c.store
                                .cancel_candidate_execution(
                                    c.token,
                                    &key(name),
                                    &queued.execution_id,
                                    &CancelExecution {
                                        expected_revision: current.revision,
                                    },
                                )
                                .await
                                .unwrap(),
                        )
                        .unwrap()
                    }
                    "renew-revoked" => {
                        c.store
                            .set_runtime_grant(
                                RuntimeGrant {
                                    organization: c.org,
                                    principal: c.actor,
                                    kind: RuntimeKind::Computer,
                                    resource_id: computer,
                                    permission: RuntimePermission::Modify,
                                    max_runtime_seconds: None,
                                },
                                false,
                            )
                            .await
                            .unwrap();
                        json!({"modify_revoked_after_renewal":true})
                    }
                    "renew-db-failure" | "renew-ack-failure" => {
                        let kind = if name == "renew-db-failure" {
                            "execution.renewal_authorized"
                        } else {
                            "execution.renewal_acknowledged"
                        };
                        // A real transaction exception on the second renewal;
                        // all authorization and acknowledgment records remain real.
                        let sql = format!(
                            "CREATE FUNCTION reject_renewal_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='{kind}' AND (NEW.payload->>'sequence')::integer=2 THEN RAISE EXCEPTION 'fixture renewal database failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_renewal_event BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION reject_renewal_event();"
                        );
                        sqlx::raw_sql(&sql).execute(c.pool).await.unwrap();
                        json!({"rejected_event":kind,"sequence":2})
                    }
                    _ => panic!("unknown renewal case"),
                };
                json!({"after_first_ack":true,"intervention":details})
            };
            let (result, details) = tokio::join!(running, intervention);
            fault = details;
            if matches!(name, "renew-db-failure" | "renew-ack-failure") {
                sqlx::raw_sql("DROP TRIGGER reject_renewal_event ON events; DROP FUNCTION reject_renewal_event();").execute(c.pool).await.unwrap();
            }
            let result = result.unwrap();
            let raw = result
                .observation
                .as_ref()
                .map(|v| serde_json::from_slice::<Value>(v.report_bytes()).unwrap());
            let mut value = serde_json::to_value(result).unwrap();
            if let Some(raw) = raw {
                value["raw_report"] = raw;
            }
            value
        };
        let elapsed = began.elapsed().as_millis();
        let success = matches!(name, "renew-short" | "renew-long");
        let state = if success {
            ExecutionState::Succeeded
        } else if name == "renew-cancel" {
            ExecutionState::Cancelled
        } else {
            ExecutionState::Unknown
        };
        if success {
            assert_eq!(fs::read(data.join("output.txt")).unwrap(), b"persisted");
            let raw = outcome["raw_report"].clone();
            assert_eq!(raw["version"], 2);
            assert_eq!(raw["report"]["outcome"], "succeeded");
            assert_eq!(
                raw["renewal"]["sequence"].as_u64().unwrap() == 0,
                name == "renew-short"
            );
            outcome["raw_report"] = raw;
        } else {
            assert!(!data.join("late.txt").exists());
        }
        if name == "renew-long" {
            assert!(elapsed >= 38000, "{elapsed}");
        }
        if name == "renew-hard" {
            assert!(
                (30000..=45000).contains(&elapsed),
                "hard expiry elapsed {elapsed}: {outcome}"
            );
        }
        let grants:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(g) FROM execution_renewal_grants g WHERE organization=$1 AND execution_id=$2 ORDER BY sequence")
            .bind(c.org.as_str()).bind(&queued.execution_id).fetch_all(c.pool).await.unwrap();
        let acks:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(a) FROM execution_renewal_acks a WHERE organization=$1 AND execution_id=$2 ORDER BY sequence")
            .bind(c.org.as_str()).bind(&queued.execution_id).fetch_all(c.pool).await.unwrap();
        if name == "renew-short" {
            assert!(grants.is_empty() && acks.is_empty());
        } else {
            assert!(!acks.is_empty(), "{name}: {outcome}");
        }
        if name == "renew-long" {
            assert!(acks.len() >= 3);
        }
        if name == "renew-db-failure" {
            assert_eq!((grants.len(), acks.len()), (1, 1));
            assert_eq!(outcome["interrupted_at"], "renewal_authorize");
        }
        if name == "renew-ack-failure" {
            assert_eq!((grants.len(), acks.len()), (2, 1));
            assert_eq!(outcome["interrupted_at"], "renewal_acknowledge");
        }
        let snapshot = c
            .store
            .candidate_execution_dispatch(c.org, &queued.execution_id)
            .await
            .unwrap();
        assert!(snapshot.hard_deadline_at_ms.is_some());
        for g in &grants {
            assert!(g["deadline_at_ms"].as_i64().unwrap() <= snapshot.hard_deadline_at_ms.unwrap());
            assert!(
                g["deadline_at_ms"].as_i64().unwrap()
                    <= g["granted_at_ms"].as_i64().unwrap() + 30000
            );
        }
        let completion_expected = name != "renew-controller-kill";
        assert_eq!(
            !outcome["completion"].is_null(),
            completion_expected,
            "{name}: {outcome}"
        );
        if completion_expected {
            assert_eq!(
                outcome["completion"]["accepted_state"],
                serde_json::to_value(state).unwrap(),
                "{name}: {outcome}"
            );
        }
        let writer = c
            .store
            .reconcile_candidate_writer(c.org, &lease.lease_id)
            .await
            .unwrap();
        assert_eq!(
            writer.state,
            if completion_expected {
                WriterLeaseState::Released
            } else {
                WriterLeaseState::Draining
            }
        );
        let output = if name == "renew-hard" {
            let output = c
                .store
                .candidate_execution_output(c.token, &queued.execution_id)
                .await
                .unwrap();
            if let Some(output) = &output {
                assert_ne!(
                    serde_json::to_value(output.observed_outcome).unwrap(),
                    "succeeded"
                );
            }
            serde_json::to_value(output).unwrap()
        } else {
            outputs::verify(
                c.config,
                c.private,
                c.store,
                c.token,
                c.org,
                &queued.execution_id,
                name,
                &outcome,
            )
            .await
        };
        let recovery = execution::recover_once(
            c.store,
            c.client,
            c.org,
            &queued.execution_id,
            c.storage,
            c.image,
            Path::new(field(&c.config["node"], "spool")),
        )
        .await
        .unwrap();
        assert_eq!(recovery.execution.state, state, "{name}");
        assert_eq!(
            serde_json::to_value(&recovery.completion).unwrap(),
            outcome["completion"]
        );
        assert!(matches!(
            execution::execute_once(
                c.store,
                c.client,
                c.org,
                &queued.execution_id,
                queued.revision,
                serde_json::from_value(c.worker.clone()).unwrap()
            )
            .await,
            Err(Error::DispatchAlreadyStarted)
        ));
        results.push(json!({"case":name,"execution_id":queued.execution_id,"prepared":prepared,"elapsed_ms":elapsed,"renewal_grants":grants,"renewal_acks":acks,"fault":fault,"outcome":outcome,"outputs":output,"recovery":recovery,"pod":c.store.candidate_execution_pod(c.org,&queued.execution_id).await.unwrap(),"watchdog":c.store.candidate_execution_watchdog(c.org,&queued.execution_id).await.unwrap(),"dispatch":snapshot}));
    }
    results
}
