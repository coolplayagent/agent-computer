//! Manual real PostgreSQL + CSI + gVisor integration; no synthetic grants/receipts.
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{
    Client,
    volume::{StorageClassBinding, VolumeIdentity, VolumePlan},
};
use agent_computer_store::{
    Error, Store,
    auth::*,
    plans::*,
    reconciliation::*,
    runtime::{connections::*, writers::*, *},
};
use agent_computer_worker::{candidate, execution, reconcile_volume_once};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[path = "support/execution_faults.rs"]
mod faults;
#[path = "support/output_http.rs"]
mod output_http;
#[path = "support/execution_outputs.rs"]
mod outputs;

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}
fn checked(v: &Value) -> agent_computer_definitions::ValidatedDefinition {
    validate_bytes(&serde_json::to_vec(v).unwrap(), Format::Json).unwrap()
}
fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap()
}

// This named service belongs only to the explicitly disposable fixture VM.
struct ReaperPause;
impl ReaperPause {
    fn begin() -> Self {
        assert!(
            std::process::Command::new("systemctl")
                .args(["stop", "ac-execution-reaper-test.service"])
                .status()
                .unwrap()
                .success()
        );
        Self
    }
}
impl Drop for ReaperPause {
    fn drop(&mut self) {
        let _ = std::process::Command::new("systemctl")
            .args(["start", "ac-execution-reaper-test.service"])
            .status();
    }
}

#[tokio::test]
async fn real_candidate_execution_uses_durable_grants_and_observation_only_recovery() {
    let config: Value = serde_json::from_slice(
        &fs::read(
            std::env::var("AGENT_COMPUTER_EXECUTION_TEST_CONFIG")
                .expect("explicit disposable root-owned environment required"),
        )
        .unwrap(),
    )
    .unwrap();
    let url = fs::read_to_string(field(&config, "database_url_file")).unwrap();
    let pool = sqlx::PgPool::connect(url.trim()).await.unwrap();
    let store = Store::new(pool.clone());
    store.migrate().await.unwrap();
    let org = OrganizationId::new(format!(
        "execution_probe_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
    .unwrap();
    let actor = PrincipalId::new("operator").unwrap();
    let credential = store
        .issue_credential(IssueCredential {
            organization: &org,
            principal: &actor,
            kind: PrincipalKind::Human,
            scopes: &ServiceScope::ALL,
            lifetime: Duration::from_secs(1800),
        })
        .await
        .unwrap();
    let token = credential.expose_token();
    // Keep the platform's eight-Candidate per-principal ceiling intact.
    // Additional output scenarios use a separately authorized fixture caller.
    let output_actor = PrincipalId::new("output-operator").unwrap();
    let output_credential = store
        .issue_credential(IssueCredential {
            organization: &org,
            principal: &output_actor,
            kind: PrincipalKind::Human,
            scopes: &ServiceScope::ALL,
            lifetime: Duration::from_secs(1800),
        })
        .await
        .unwrap();
    for kind in [
        DefinitionKind::Declaration,
        DefinitionKind::Volume,
        DefinitionKind::Workspace,
        DefinitionKind::Sandbox,
        DefinitionKind::Computer,
    ] {
        store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &actor,
                    kind,
                    name: "*",
                    permission: DefinitionPermission::Create,
                },
                true,
            )
            .await
            .unwrap();
    }
    let mut catalog = vec![];
    for (kind, name) in [
        (DefinitionKind::StorageClass, "juicefs"),
        (DefinitionKind::NetworkPolicy, "deny-all"),
    ] {
        let id = store
            .register_catalog_reference(&org, kind, name)
            .await
            .unwrap();
        for principal in [&actor, &output_actor] {
            store
                .set_definition_grant(
                    DefinitionGrant {
                        organization: &org,
                        principal,
                        kind,
                        name,
                        permission: DefinitionPermission::Reference,
                    },
                    true,
                )
                .await
                .unwrap();
        }
        catalog.push(format!("id:{id}"));
    }
    let kube = &config["kubernetes"];
    let mut deployment: agent_computer_kubernetes::Deployment =
        serde_json::from_value(kube["deployment"].clone()).unwrap();
    deployment.network_policy_ref = catalog[1].clone();
    let client = Client::new(
        field(kube, "api_url"),
        &fs::read(field(kube, "ca_file")).unwrap(),
        fs::read_to_string(field(kube, "token_file"))
            .unwrap()
            .trim(),
        deployment.clone(),
    )
    .unwrap();
    let mut storage: StorageClassBinding = serde_json::from_value(kube["storage"].clone()).unwrap();
    storage.reference = catalog[0].clone();
    let image = field(&config, "image");
    let names = [
        "normal",
        "command",
        "cancel",
        "lost",
        "rejected",
        "controller-kill",
        "pid1-stop",
        "reaper-missing",
        "output-store-failure",
        "output-db-failure",
        "output-truncated",
        "output-failed",
        "output-timeout",
        "output-revoked",
        "output-unknown",
        "output-completion-retry",
    ];
    let mut document = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"execution-probe"},"spec":{
        "volumes":[{"name":"data","storageClass":storage.reference,"quotaBytes":names.len() as u64 * 10737418240u64,"reclaimPolicy":"Retain"}],"workspaces":[],"sandboxes":[],"computers":[]}});
    for name in names {
        document["spec"]["workspaces"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":name,"volumeRef":"data","conflictPolicy":"explicit"}));
        document["spec"]["sandboxes"].as_array_mut().unwrap().push(json!({"name":name,"runtimeClass":"gvisor","image":image,"resources":{"cpuMillis":500,"memoryMiB":256},"networkPolicyRef":catalog[1],"mounts":[{"workspaceRef":name,"path":"/workspace","readOnly":false}]}));
        document["spec"]["computers"].as_array_mut().unwrap().push(json!({"name":name,"workspaceRef":name,"sandboxRefs":[name],"appRefs":[],"desiredState":"Stopped"}));
    }
    let declaration = store
        .create_definition_plan(token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    let operation = store
        .apply_definition_plan(
            token,
            &key("apply"),
            &declaration.plan_id,
            &declaration.plan_digest,
        )
        .await
        .unwrap();
    let owner = WorkerId::new("execution-worker-probe").unwrap();
    let limit = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        reconcile_volume_once(&store, &client, &storage, &org, &owner)
            .await
            .unwrap();
        let operation = store
            .definition_operation(token, &operation.operation_id)
            .await
            .unwrap();
        let progress = operation
            .progress
            .iter()
            .find(|p| {
                declaration
                    .resources
                    .iter()
                    .any(|r| r.resource_id == p.resource_id && r.kind == DefinitionKind::Volume)
            })
            .unwrap();
        if progress.state == IntentState::Succeeded {
            break;
        }
        assert!(
            tokio::time::Instant::now() < limit,
            "volume provisioning deadline"
        );
        assert!(
            !matches!(progress.state, IntentState::Blocked | IntentState::Failed),
            "{progress:?}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let volume = declaration
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Volume)
        .unwrap();
    let step = &operation
        .progress
        .iter()
        .find(|p| p.resource_id == volume.resource_id)
        .unwrap()
        .step_id;
    let mut spec = document["spec"]["volumes"][0].clone();
    spec["name"] = json!("volume");
    let volume_plan=VolumePlan::new(&checked(&json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"worker"},"spec":{"volumes":[spec]}})),"volume",VolumeIdentity{organization:org.as_str().into(),resource_id:volume.resource_id.clone(),revision:1,step_id:step.clone(),spec_digest:volume.digest.clone()},client.namespace(),storage.clone()).unwrap();
    let pvc = client
        .observe_volume_claim(&volume_plan, None)
        .await
        .unwrap()
        .unwrap();
    let pv = client
        .observe_bound_volume(&volume_plan, &pvc, None)
        .await
        .unwrap()
        .unwrap();
    let mut local = config["candidate"].clone();
    local["target"]["volume_id"] = json!(volume.resource_id);
    local["target"]["pvc_uid"] = json!(pvc.uid());
    local["target"]["pv_uid"] = json!(pv.uid());
    local["target"]["namespace_uid"] = json!(client.namespace_uid());
    local["target"]["volume_path"] = json!(pv.handle());
    let root = PathBuf::from(field(&local, "mount_root")).join(pv.handle());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for resource in &declaration.resources {
        let (kind, permissions): (RuntimeKind, &[RuntimePermission]) = match resource.kind {
            DefinitionKind::Computer => (
                RuntimeKind::Computer,
                &[
                    RuntimePermission::Read,
                    RuntimePermission::Modify,
                    RuntimePermission::Connect,
                    RuntimePermission::Activate,
                ],
            ),
            DefinitionKind::Workspace => (
                RuntimeKind::Workspace,
                &[RuntimePermission::Read, RuntimePermission::Modify],
            ),
            _ => continue,
        };
        for permission in permissions {
            for principal in [&actor, &output_actor] {
                store
                    .set_runtime_grant(
                        RuntimeGrant {
                            organization: &org,
                            principal,
                            kind,
                            resource_id: &resource.resource_id,
                            permission: *permission,
                            max_runtime_seconds: (*permission == RuntimePermission::Activate)
                                .then_some(600),
                        },
                        true,
                    )
                    .await
                    .unwrap();
            }
        }
    }
    let worker = json!({"approved_supervisor_image":image,"storage":storage,"candidate":local,"node":config["node"],"outputs":config["outputs"],"output_spool":config["output_spool"]});
    let mut results = vec![];
    for name in names {
        let token = if name.starts_with("output-") {
            output_credential.expose_token()
        } else {
            token
        };
        let mut case_worker = worker.clone();
        if name == "output-store-failure" {
            case_worker["outputs"]["credentials_file"] =
                config["rejected_output_credentials_file"].clone();
        }
        let make_config =
            || serde_json::from_value::<execution::Configuration>(case_worker.clone()).unwrap();
        let computer = &declaration
            .resources
            .iter()
            .find(|r| r.kind == DefinitionKind::Computer && r.name == name)
            .unwrap()
            .resource_id;
        let sandbox = &declaration
            .resources
            .iter()
            .find(|r| r.kind == DefinitionKind::Sandbox && r.name == name)
            .unwrap()
            .resource_id;
        let start = store
            .admit_computer_start(
                token,
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
                &store,
                &org,
                &start.request_id,
                &owner,
                serde_json::from_value(local.clone()).unwrap()
            )
            .await
            .unwrap(),
            candidate::WorkResult::Prepared
        ));
        let session = store
            .create_connection_session(
                token,
                &key(&format!("connection-{name}")),
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
        let lease = store
            .acquire_candidate_writer(
                token,
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
        let script = if name == "output-failed" {
            "printf persisted > output.txt; /bin/sync output.txt; exit 7"
        } else if name == "output-timeout" {
            "printf persisted > output.txt; /bin/sync output.txt; /bin/sleep 20"
        } else if matches!(name, "output-revoked" | "output-unknown") {
            "printf started > started.txt; /bin/sync started.txt; /bin/sleep 20; printf unexpected > late.txt"
        } else if name == "output-truncated" {
            "i=0; while [ \"$i\" -lt 5000 ]; do printf x; printf y >&2; i=$((i+1)); done; printf persisted > output.txt; /bin/sync output.txt"
        } else if name == "pid1-stop" {
            "kill -STOP 1; /bin/cat /proc/1/status > pid1-status.txt; /bin/sync pid1-status.txt; printf started > started.txt; /bin/sync started.txt; while :; do printf tick >> ticks.txt; /bin/sync ticks.txt; /bin/sleep 0.1; done"
        } else if name == "controller-kill" {
            "printf started > started.txt; /bin/sync started.txt; /bin/sleep 35; printf unexpected > late.txt"
        } else if name == "cancel" {
            "printf started > started.txt; /bin/sync started.txt; /bin/sleep 20; printf unexpected > late.txt"
        } else {
            "/bin/mkdir nested && printf nested > nested/file && /bin/sync nested/file && /bin/mv nested/file nested/renamed && /bin/rm nested/renamed || exit 98; if [ -e .control ] || [ -e nested/.control ]; then exit 99; fi; /bin/rmdir nested || exit 98; printf persisted > output.txt; /bin/sync output.txt; printf done"
        };
        let queued = store
            .submit_candidate_execution(
                token,
                &key(&format!("execution-{name}")),
                computer,
                &SubmitExecution {
                    lease_id: lease.lease_id.clone(),
                    lease: WriterLeaseCommand {
                        connection_session_id: lease.connection_session_id.clone(),
                        generation: lease.generation,
                        epoch: lease.epoch,
                        expected_revision: lease.revision,
                    },
                    sandbox_id: sandbox.clone(),
                    lifetime: ExecutionLifetime::Connection,
                    command: ExecutionCommand {
                        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
                        cwd: String::new(),
                        timeout_seconds: if name == "output-timeout" { 1 } else { 25 },
                        term_grace_ms: 100,
                        output_limit_bytes: 4096,
                    },
                },
            )
            .await
            .unwrap();
        let prepared: Value = sqlx::query_scalar(
            "SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(org.as_str())
        .bind(&start.request_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let data = root.join(field(&prepared, "path_ref"));
        let _reaper_pause = (name == "reaper-missing").then(ReaperPause::begin);
        if name == "output-db-failure" {
            sqlx::raw_sql("CREATE FUNCTION reject_output_publication() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='execution.output_verified' THEN RAISE EXCEPTION 'fixture output acknowledgement failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_output_publication BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION reject_output_publication();").execute(&pool).await.unwrap();
        }
        if name == "output-completion-retry" {
            // Sequences do not roll back: reject exactly the first completion
            // transaction, then require the same live seal to succeed on retry.
            sqlx::raw_sql("CREATE SEQUENCE completion_retry_probe; CREATE FUNCTION reject_first_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='execution.completed' AND nextval('completion_retry_probe')=1 THEN RAISE EXCEPTION 'fixture completion transaction failed'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_first_completion BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION reject_first_completion();").execute(&pool).await.unwrap();
        }
        let outcome = match name {
            "output-revoked" | "output-unknown" => {
                let running = execution::execute_once(
                    &store,
                    &client,
                    &org,
                    &queued.execution_id,
                    queued.revision,
                    make_config(),
                );
                let lower = async {
                    let until = tokio::time::Instant::now() + Duration::from_secs(20);
                    while !data.join("started.txt").exists() {
                        assert!(tokio::time::Instant::now() < until, "authority barrier");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    if name == "output-revoked" {
                        store
                            .set_runtime_grant(
                                RuntimeGrant {
                                    organization: &org,
                                    principal: &output_actor,
                                    kind: RuntimeKind::Computer,
                                    resource_id: computer,
                                    permission: RuntimePermission::Modify,
                                    max_runtime_seconds: None,
                                },
                                false,
                            )
                            .await
                            .unwrap();
                    } else {
                        store
                            .mark_candidate_execution_unknown(&org, &queued.execution_id, 2)
                            .await
                            .unwrap();
                    }
                };
                let (result, ()) = tokio::join!(running, lower);
                let result = result.unwrap();
                assert!(result.observation.is_none());
                assert!(!data.join("late.txt").exists());
                serde_json::to_value(result).unwrap()
            }
            "reaper-missing" => {
                let result = execution::execute_once(
                    &store,
                    &client,
                    &org,
                    &queued.execution_id,
                    queued.revision,
                    make_config(),
                )
                .await
                .unwrap();
                assert!(
                    matches!(result.interrupted_at, Some(execution::Phase::Watchdog)),
                    "{result:?}"
                );
                assert_eq!(
                    result.node_error,
                    Some(agent_computer_node::Error::ReaperUnavailable)
                );
                assert!(result.observation.is_none());
                assert!(!data.join("output.txt").exists());
                serde_json::to_value(result).unwrap()
            }
            "controller-kill" | "pid1-stop" => {
                let private = json!({"api_url":kube["api_url"],"ca_file":kube["ca_file"],"token_file":kube["token_file"],"deployment":deployment,"execution":worker});
                faults::kill_controller(
                    &config,
                    &private,
                    &store,
                    &org,
                    &queued.execution_id,
                    &data,
                    name,
                )
                .await
            }
            "lost" => {
                let attempt = store
                    .begin_candidate_execution_dispatch(&org, &queued.execution_id, queued.revision)
                    .await
                    .unwrap();
                let inputs = store
                    .candidate_execution_runtime_inputs(&org, &queued.execution_id)
                    .await
                    .unwrap();
                let plan = execution::compile_plan(&inputs, &client, &storage, image).unwrap();
                store
                    .register_candidate_execution_pod(
                        &attempt,
                        client.namespace_uid(),
                        &plan.pod_plan().manifest(),
                    )
                    .await
                    .unwrap();
                let created = client.create(plan.pod_plan()).await.unwrap();
                // Creation really succeeded; intentionally lose the database UID acknowledgement.
                assert!(
                    store
                        .candidate_execution_pod(&org, &queued.execution_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .pod_uid
                        .is_none()
                );
                let recovered = execution::recover_once(
                    &store,
                    &client,
                    &org,
                    &queued.execution_id,
                    &storage,
                    image,
                    std::path::Path::new(field(&config["node"], "spool")),
                )
                .await
                .unwrap();
                assert_eq!(
                    store
                        .candidate_execution_pod(&org, &queued.execution_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .pod_uid
                        .as_deref(),
                    Some(created.uid())
                );
                assert!(!data.join("output.txt").exists());
                serde_json::to_value(recovered).unwrap()
            }
            "cancel" => {
                let running = execution::execute_once(
                    &store,
                    &client,
                    &org,
                    &queued.execution_id,
                    queued.revision,
                    make_config(),
                );
                let cancellation = async {
                    let limit = tokio::time::Instant::now() + Duration::from_secs(20);
                    while !data.join("started.txt").exists() {
                        assert!(
                            tokio::time::Instant::now() < limit,
                            "child did not reach cancellation barrier"
                        );
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let csi_test = field(&config, "csi_test_binary").to_owned();
                    let csi_execution = queued.execution_id.clone();
                    let proof = tokio::task::spawn_blocking(move || {
                        std::process::Command::new(csi_test)
                            .env("AGENT_COMPUTER_CSI_DISPOSABLE_TEST", "1")
                            .env("AGENT_COMPUTER_CSI_TEST_EXECUTION", csi_execution)
                            .args([
                                "--exact",
                                "real_csi_active_retry_and_restart",
                                "--nocapture",
                            ])
                            .output()
                            .unwrap()
                    })
                    .await
                    .unwrap();
                    assert!(
                        proof.status.success(),
                        "{} {}",
                        String::from_utf8_lossy(&proof.stdout),
                        String::from_utf8_lossy(&proof.stderr)
                    );
                    println!("{}", String::from_utf8_lossy(&proof.stdout));
                    // Optional independent node inspection before cancellation;
                    // the original execution budget continues to run throughout.
                    if let Some(path) = config["node_observation_file"].as_str() {
                        let journal = store
                            .candidate_execution_pod(&org, &queued.execution_id)
                            .await
                            .unwrap()
                            .unwrap();
                        fs::write(path,serde_json::to_vec(&json!({"namespace":journal.namespace,"name":journal.pod_name,"uid":journal.pod_uid,"prepared":prepared})).unwrap()).unwrap();
                        let marker = PathBuf::from(path).with_extension("inspected");
                        let until = tokio::time::Instant::now() + Duration::from_secs(8);
                        while !marker.exists() {
                            assert!(
                                tokio::time::Instant::now() < until,
                                "node inspection deadline"
                            );
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                    store
                        .cancel_candidate_execution(
                            token,
                            &key("cancel-running"),
                            &queued.execution_id,
                            &CancelExecution {
                                expected_revision: 2,
                            },
                        )
                        .await
                        .unwrap();
                };
                let (result, ()) = tokio::join!(running, cancellation);
                let result = result.unwrap();
                assert!(result.observation.is_none());
                assert!(!data.join("late.txt").exists());
                serde_json::to_value(result).unwrap()
            }
            "command" => {
                let path = PathBuf::from(field(&config, "result_file"))
                    .with_file_name("execution-command.json");
                let private = json!({"api_url":kube["api_url"],"ca_file":kube["ca_file"],"token_file":kube["token_file"],"deployment":deployment,"execution":worker});
                fs::write(&path, serde_json::to_vec(&private).unwrap()).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                let output = std::process::Command::new(field(&config, "server_binary"))
                    .args([
                        "execution-dispatch-once",
                        "--database-url-file",
                        field(&config, "database_url_file"),
                        "--organization",
                        org.as_str(),
                        "--execution-id",
                        &queued.execution_id,
                        "--expected-revision",
                        "1",
                        "--config-file",
                    ])
                    .arg(path)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let result: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert!(result["interrupted_at"].is_null(), "{result}");
                assert_eq!(fs::read(data.join("output.txt")).unwrap(), b"persisted");
                result
            }
            "rejected" => {
                let mut wrong = make_config();
                wrong.approved_supervisor_image =
                    format!("registry.invalid/replaced@sha256:{}", "0".repeat(64));
                let result = execution::execute_once(
                    &store,
                    &client,
                    &org,
                    &queued.execution_id,
                    queued.revision,
                    wrong,
                )
                .await
                .unwrap();
                assert!(matches!(
                    result.interrupted_at,
                    Some(execution::Phase::Preparing)
                ));
                assert!(
                    store
                        .candidate_execution_pod(&org, &queued.execution_id)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(!data.join("output.txt").exists());
                serde_json::to_value(result).unwrap()
            }
            _ => {
                let result = execution::execute_once(
                    &store,
                    &client,
                    &org,
                    &queued.execution_id,
                    queued.revision,
                    make_config(),
                )
                .await
                .unwrap();
                let report: Value = serde_json::from_slice(
                    result
                        .observation
                        .as_ref()
                        .unwrap_or_else(|| panic!("missing raw report: {result:?}"))
                        .report_bytes(),
                )
                .unwrap();
                let expected = match name {
                    "output-failed" => "failed",
                    "output-timeout" => "timed_out",
                    _ => "succeeded",
                };
                assert_eq!(report["report"]["outcome"], expected, "{report}");
                assert_eq!(fs::read(data.join("output.txt")).unwrap(), b"persisted");
                let mut result = serde_json::to_value(result).unwrap();
                result["raw_report"] = report;
                result
            }
        };
        if name == "output-db-failure" {
            sqlx::raw_sql("DROP TRIGGER reject_output_publication ON events; DROP FUNCTION reject_output_publication();").execute(&pool).await.unwrap();
        }
        if name == "output-completion-retry" {
            let attempts: i64 = sqlx::query_scalar("SELECT last_value FROM completion_retry_probe")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                attempts, 2,
                "the first rollback must be retried with the same seal"
            );
            sqlx::raw_sql("DROP TRIGGER reject_first_completion ON events; DROP FUNCTION reject_first_completion(); DROP SEQUENCE completion_retry_probe;").execute(&pool).await.unwrap();
        }
        if !matches!(name, "lost" | "rejected" | "controller-kill" | "pid1-stop") {
            assert_eq!(outcome["publication_revoked"], true, "{outcome}");
            assert_eq!(outcome["io_fence"]["prepared"], prepared, "{outcome}");
            let pod = store
                .candidate_execution_pod(&org, &queued.execution_id)
                .await
                .unwrap()
                .unwrap();
            let binding: Value = serde_json::from_str(
                pod.manifest["metadata"]["annotations"]["agent-computer.io/binding"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                pod.manifest["spec"]["volumes"][2]["csi"]["driver"],
                "csi.agent-computer.io"
            );
            assert!(
                pod.manifest["spec"]["containers"][0]["volumeMounts"][2]
                    .get("subPath")
                    .is_none()
            );
            assert_eq!(
                binding["workspace"]["fence"]["mount"]["instance"],
                outcome["io_fence"]["instance"]
            );
            if let Some(arm) = store
                .candidate_execution_watchdog(&org, &queued.execution_id)
                .await
                .unwrap()
            {
                assert_eq!(
                    arm.evidence["runtime"]["workspace_mount"],
                    binding["workspace"]["fence"]["mount"]
                );
                assert_eq!(arm.evidence["runtime"]["workspace_mount"]["inode"], 1);
            }
        }
        let private = json!({"api_url":"https://127.0.0.1:1","ca_file":"/unavailable-kubernetes-ca","token_file":"/unavailable-kubernetes-token","deployment":deployment,"execution":worker});
        let output_evidence = outputs::verify(
            &config,
            &private,
            &store,
            token,
            &org,
            &queued.execution_id,
            name,
            &outcome,
        )
        .await;
        let completed = !matches!(
            name,
            "lost" | "rejected" | "controller-kill" | "pid1-stop" | "reaper-missing"
        );
        let expected_state = match name {
            "normal" | "command" | "output-truncated" | "output-completion-retry" => {
                ExecutionState::Succeeded
            }
            "output-failed" | "output-timeout" => ExecutionState::Failed,
            "cancel" => ExecutionState::Cancelled,
            _ => ExecutionState::Unknown,
        };
        assert_eq!(
            store
                .reconcile_candidate_writer(&org, &lease.lease_id)
                .await
                .unwrap()
                .state,
            if completed {
                WriterLeaseState::Released
            } else {
                WriterLeaseState::Draining
            }
        );
        assert_eq!(!outcome["completion"].is_null(), completed, "{outcome}");
        if completed {
            assert_eq!(
                outcome["completion"]["accepted_state"],
                serde_json::to_value(expected_state).unwrap()
            );
        }
        assert_eq!(
            store
                .candidate_execution(token, &queued.execution_id)
                .await
                .unwrap()
                .state,
            expected_state
        );
        assert!(matches!(
            execution::execute_once(
                &store,
                &client,
                &org,
                &queued.execution_id,
                queued.revision,
                make_config()
            )
            .await,
            Err(Error::DispatchAlreadyStarted)
        ));
        assert_eq!(
            store
                .candidate_execution_startup(&org, &queued.execution_id)
                .await
                .unwrap()
                .is_some(),
            matches!(
                name,
                "normal"
                    | "command"
                    | "cancel"
                    | "controller-kill"
                    | "pid1-stop"
                    | "output-store-failure"
                    | "output-db-failure"
                    | "output-truncated"
                    | "output-failed"
                    | "output-timeout"
                    | "output-revoked"
                    | "output-unknown"
                    | "output-completion-retry"
            )
        );
        let next_writer = if name == "normal" {
            let next = store
                .acquire_candidate_writer(
                    token,
                    &key("after-execution"),
                    computer,
                    &AcquireWriterLease {
                        connection_session_id: lease.connection_session_id.clone(),
                        generation: lease.generation,
                        candidate_id: lease.candidate_id.clone(),
                        scope: WriterScope::Modify,
                        duration_seconds: 30,
                    },
                )
                .await
                .unwrap();
            assert_eq!(next.epoch, lease.epoch + 1);
            let saved=agent_computer_worker::files::save_once(&store,token,&next.lease_id,
                serde_json::from_value(json!({"lease":{"connection_session_id":next.connection_session_id,"generation":next.generation,"epoch":next.epoch,"expected_revision":next.revision},"dispatch_id":"after-execution-file","edit":{"path":"after-execution.txt","expected":null,"content":b"next-writer","executable":false}})).unwrap(),
                serde_json::from_value(json!({"target":local["target"],"mount_root":local["mount_root"]})).unwrap()
            ).await.unwrap();
            assert_eq!(saved.state, WriterLeaseState::Released);
            assert_eq!(
                fs::read(data.join("after-execution.txt")).unwrap(),
                b"next-writer"
            );
            Some(saved)
        } else {
            None
        };
        let recovery = execution::recover_once(
            &store,
            &client,
            &org,
            &queued.execution_id,
            &storage,
            image,
            std::path::Path::new(field(&config["node"], "spool")),
        )
        .await
        .unwrap();
        let watchdog = store
            .candidate_execution_watchdog(&org, &queued.execution_id)
            .await
            .unwrap();
        assert_eq!(
            watchdog.is_some(),
            matches!(
                name,
                "normal"
                    | "command"
                    | "cancel"
                    | "controller-kill"
                    | "pid1-stop"
                    | "output-store-failure"
                    | "output-db-failure"
                    | "output-truncated"
                    | "output-failed"
                    | "output-timeout"
                    | "output-revoked"
                    | "output-unknown"
                    | "output-completion-retry"
            )
        );
        assert_eq!(recovery.execution.state, expected_state);
        assert_eq!(
            serde_json::to_value(&recovery.completion).unwrap(),
            outcome["completion"]
        );
        if let Some(next) = &next_writer {
            let after = store
                .reconcile_candidate_writer(&org, &lease.lease_id)
                .await
                .unwrap();
            assert_eq!(after.epoch, next.epoch);
            assert_eq!(after.revision, next.revision);
            assert_eq!(after.state, WriterLeaseState::Released);
        }
        results.push(json!({"case":name,"execution_id":queued.execution_id,"prepared":prepared,"outcome":outcome,"outputs":output_evidence,"next_writer":next_writer,"recovery":recovery,"pod":store.candidate_execution_pod(&org,&queued.execution_id).await.unwrap(),"watchdog":watchdog}));
    }
    let drains: i64 =
        sqlx::query_scalar("SELECT count(*) FROM candidate_writer_drains WHERE organization=$1")
            .bind(org.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(drains, 12);
    let evidence = json!({"organization":org.as_str(),"pvc_uid":pvc.uid(),"pv_uid":pv.uid(),"volume_path":pv.handle(),"filesystem_uuid":local["target"]["filesystem_uuid"],"results":results,"writer_drains":drains,"limits":["single VM; actual database grants, node watchdog/reaper admission and CSI mounts","raw process report alone is not accepted completion","API deletion is not physical fencing","controller-loss recovery and multi-node fencing do not recreate live seals"]});
    fs::write(
        field(&config, "result_file"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("EXECUTION_WORKER_DATABASE_CSI_GVISOR_PASSED");
}
