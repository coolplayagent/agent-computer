//! Explicit disposable environment only: real control PostgreSQL, Kubernetes/CSI,
//! JuiceFS metadata PostgreSQL and S3. No synthetic backend receipts are inserted.
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{
    Client,
    volume::{StorageClassBinding, VolumeIdentity, VolumePlan},
};
use agent_computer_storage::{MountedVolume, ObjectCache, Prepared, quota::JuiceFsQuota};
use agent_computer_store::{
    Store,
    auth::*,
    plans::*,
    reconciliation::*,
    runtime::{preparation::*, *},
};
use agent_computer_worker::{
    candidate::{Configuration, WorkResult, prepare_once},
    reconcile_volume_once,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[path = "support/artifacts.rs"]
mod artifacts;
#[path = "support/continuation.rs"]
mod continuation;
#[path = "support/file_http.rs"]
mod file_http;
#[path = "support/file_writer.rs"]
mod file_writer;

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}
fn checked(value: &Value) -> agent_computer_definitions::ValidatedDefinition {
    validate_bytes(&serde_json::to_vec(value).unwrap(), Format::Json).unwrap()
}

// Operator subprocesses must not starve SQLx's asynchronous rollback after an
// intentionally rejected request in the parent test process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_volume_preparation_commits_receipts_and_observes_lost_acknowledgement() {
    let path = std::env::var("AGENT_COMPUTER_CANDIDATE_TEST_CONFIG")
        .expect("explicit disposable root-owned environment required");
    let config: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let url = fs::read_to_string(config["database_url_file"].as_str().unwrap()).unwrap();
    let pool = sqlx::PgPool::connect(url.trim()).await.unwrap();
    let store = Store::new(pool.clone());
    store.migrate().await.unwrap();
    let kube = &config["kubernetes"];
    let mut storage: StorageClassBinding = serde_json::from_value(kube["storage"].clone()).unwrap();
    let client = Client::new(
        kube["api_url"].as_str().unwrap(),
        &fs::read(kube["ca_file"].as_str().unwrap()).unwrap(),
        fs::read_to_string(kube["token_file"].as_str().unwrap())
            .unwrap()
            .trim(),
        serde_json::from_value(kube["deployment"].clone()).unwrap(),
    )
    .unwrap();
    let org = OrganizationId::new(format!(
        "candidate_probe_{}",
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
    for kind in [
        DefinitionKind::Declaration,
        DefinitionKind::Volume,
        DefinitionKind::Workspace,
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
    let catalog = store
        .register_catalog_reference(&org, DefinitionKind::StorageClass, "juicefs")
        .await
        .unwrap();
    store
        .set_definition_grant(
            DefinitionGrant {
                organization: &org,
                principal: &actor,
                kind: DefinitionKind::StorageClass,
                name: "juicefs",
                permission: DefinitionPermission::Reference,
            },
            true,
        )
        .await
        .unwrap();
    storage.reference = format!("id:{catalog}");
    let document = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"candidate-probe"},"spec":{
        "volumes":[{"name":"data","storageClass":storage.reference,"quotaBytes":107374182400_u64,"reclaimPolicy":"Retain"}],
        "workspaces":[{"name":"one","volumeRef":"data","conflictPolicy":"explicit"},{"name":"two","volumeRef":"data","conflictPolicy":"explicit"},{"name":"three","volumeRef":"data","conflictPolicy":"explicit"},{"name":"artifact","volumeRef":"data","conflictPolicy":"explicit"},{"name":"parallel","volumeRef":"data","conflictPolicy":"explicit"}],
        "computers":[{"name":"one","workspaceRef":"one","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"},{"name":"two","workspaceRef":"two","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"},{"name":"three","workspaceRef":"three","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"},{"name":"artifact","workspaceRef":"artifact","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"},{"name":"parallel-one","workspaceRef":"parallel","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"},{"name":"parallel-two","workspaceRef":"parallel","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"}]
    }});
    let plan = store
        .create_definition_plan(credential.expose_token(), &key("plan"), &checked(&document))
        .await
        .unwrap();
    let operation = store
        .apply_definition_plan(
            credential.expose_token(),
            &key("apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    let owner = WorkerId::new("candidate-live-worker").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        reconcile_volume_once(&store, &client, &storage, &org, &owner)
            .await
            .unwrap();
        let progress = store
            .definition_operation(credential.expose_token(), &operation.operation_id)
            .await
            .unwrap();
        let volume = progress
            .progress
            .iter()
            .find(|p| {
                plan.resources
                    .iter()
                    .any(|r| r.resource_id == p.resource_id && r.kind == DefinitionKind::Volume)
            })
            .unwrap();
        if volume.state == IntentState::Succeeded {
            break;
        }
        assert!(
            !matches!(volume.state, IntentState::Blocked | IntentState::Failed),
            "{volume:?}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "volume provisioning deadline"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let volume = plan
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
    // The worker compiles a normalized single-volume declaration, independent
    // of the original ComputerSet's local resource name.
    let mut normalized_volume = document["spec"]["volumes"][0].clone();
    normalized_volume["name"] = "volume".into();
    let normalized = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"worker"},"spec":{"volumes":[normalized_volume]}});
    let volume_plan = VolumePlan::new(
        &checked(&normalized),
        "volume",
        VolumeIdentity {
            organization: org.as_str().into(),
            resource_id: volume.resource_id.clone(),
            revision: 1,
            step_id: step.clone(),
            spec_digest: volume.digest.clone(),
        },
        client.namespace(),
        storage,
    )
    .unwrap();
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
    let mut worker_config = config["candidate"].clone();
    worker_config["target"]["volume_id"] = volume.resource_id.clone().into();
    worker_config["target"]["pvc_uid"] = pvc.uid().into();
    worker_config["target"]["pv_uid"] = pv.uid().into();
    worker_config["target"]["namespace_uid"] = client.namespace_uid().into();
    worker_config["target"]["volume_path"] = pv.handle().into();
    let root = PathBuf::from(worker_config["mount_root"].as_str().unwrap()).join(pv.handle());
    assert!(
        root.is_dir(),
        "CSI-created Volume directory must already exist"
    );
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let target: PreparationTarget =
        serde_json::from_value(worker_config["target"].clone()).unwrap();
    for resource in &plan.resources {
        let (kind, permissions): (RuntimeKind, &[RuntimePermission]) = match resource.kind {
            DefinitionKind::Computer => (
                RuntimeKind::Computer,
                &[
                    RuntimePermission::Read,
                    RuntimePermission::Activate,
                    RuntimePermission::Connect,
                    RuntimePermission::Modify,
                    RuntimePermission::Manage,
                ],
            ),
            DefinitionKind::Workspace => (
                RuntimeKind::Workspace,
                &[
                    RuntimePermission::Read,
                    RuntimePermission::Modify,
                    RuntimePermission::Publish,
                ],
            ),
            _ => continue,
        };
        for permission in permissions {
            store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org,
                        principal: &actor,
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
    let mut observations = vec![];
    let mut file_evidence = Value::Null;
    for (index, name) in ["one", "two", "three"].into_iter().enumerate() {
        let computer = &plan
            .resources
            .iter()
            .find(|r| r.kind == DefinitionKind::Computer && r.name == name)
            .unwrap()
            .resource_id;
        let admitted = store
            .admit_computer_start(
                credential.expose_token(),
                &key(name),
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
        let make_config =
            || serde_json::from_value::<Configuration>(worker_config.clone()).unwrap();
        let mut expected_inode = None;
        let mut absent_path = None;
        if index > 0 {
            let PreparationClaim::Claimed(lease) = store
                .claim_candidate_preparation(&org, &admitted.request_id, &owner, &target)
                .await
                .unwrap()
            else {
                panic!()
            };
            let permit = store.begin_candidate_preparation(&lease).await.unwrap();
            if index == 2 {
                let path = root.join(permit.lease().request().path_ref());
                assert!(!path.try_exists().unwrap());
                absent_path = Some(path);
            }
            if index == 1 {
                let c = make_config();
                let mount = MountedVolume::open(
                    &c.mount_root,
                    &target.volume_path,
                    &target.filesystem_uuid,
                    &target.pvc_uid,
                    target.writer_uid,
                    target.writer_gid,
                )
                .unwrap();
                let receipt = mount
                    .prepare(
                        permit.lease().request(),
                        &ObjectCache::open(&c.object_cache).unwrap(),
                        &JuiceFsQuota::new(c.quota).unwrap(),
                    )
                    .unwrap();
                expected_inode = Some(receipt.data_inode);
                // Real publication succeeded; its database acknowledgement is lost.
                let file =
                    fs::File::create(root.join(&receipt.path_ref).join("preserved.txt")).unwrap();
                use std::io::Write;
                (&file).write_all(b"preserve-before-observation").unwrap();
                file.sync_all().unwrap();
            }
            // Fault injection affects the coordination clock only, not storage.
            sqlx::query("UPDATE candidate_preparations SET lease_until_ms=1 WHERE organization=$1 AND request_id=$2").bind(org.as_str()).bind(&admitted.request_id).execute(&pool).await.unwrap();
        }
        let outcome = if index == 0 {
            let command_config = PathBuf::from(config["observation_file"].as_str().unwrap())
                .with_file_name("candidate-command.json");
            fs::write(&command_config, serde_json::to_vec(&worker_config).unwrap()).unwrap();
            fs::set_permissions(&command_config, fs::Permissions::from_mode(0o600)).unwrap();
            let output = std::process::Command::new(config["server_binary"].as_str().unwrap())
                .args([
                    "candidate-prepare-once",
                    "--database-url-file",
                    config["database_url_file"].as_str().unwrap(),
                    "--organization",
                    org.as_str(),
                    "--worker-id",
                    owner.as_str(),
                    "--request-id",
                    &admitted.request_id,
                    "--config-file",
                ])
                .arg(command_config)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                "prepared"
            );
            WorkResult::Prepared
        } else {
            prepare_once(&store, &org, &admitted.request_id, &owner, make_config())
                .await
                .unwrap()
        };
        if index == 2 {
            assert!(matches!(outcome, WorkResult::StorageUnknown));
            assert!(!absent_path.unwrap().try_exists().unwrap());
            assert_eq!(
                store
                    .computer_runtime(credential.expose_token(), computer)
                    .await
                    .unwrap()
                    .start_state,
                Some(StartState::Preparing)
            );
            observations.push(json!({"request_id":admitted.request_id,"state":"Preparing","missing_publication_not_recreated":true}));
            continue;
        }
        assert!(matches!(outcome, WorkResult::Prepared));
        let receipt:Prepared=serde_json::from_value(sqlx::query_scalar::<_,Value>("SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2").bind(org.as_str()).bind(&admitted.request_id).fetch_one(&pool).await.unwrap()).unwrap();
        let data = root.join(&receipt.path_ref);
        assert_eq!(data.metadata().unwrap().ino(), receipt.data_inode);
        assert_eq!(data.metadata().unwrap().uid(), 1000);
        if let Some(inode) = expected_inode {
            assert_eq!(receipt.data_inode, inode);
            assert_eq!(
                fs::read(data.join("preserved.txt")).unwrap(),
                b"preserve-before-observation"
            );
        }
        assert!(matches!(
            prepare_once(&store, &org, &admitted.request_id, &owner, make_config())
                .await
                .unwrap(),
            WorkResult::Prepared
        ));
        let current = store
            .computer_runtime(credential.expose_token(), computer)
            .await
            .unwrap();
        assert_eq!(current.start_state, Some(StartState::Prepared));
        assert!(!current.ready);
        if index == 0 {
            file_evidence = file_writer::verify(file_writer::Context {
                store: &store,
                pool: &pool,
                token: credential.expose_token(),
                org: &org,
                actor: &actor,
                computer,
                start: &admitted,
                worker: &worker_config,
                config: &config,
                data: &data,
            })
            .await;
        }
        observations.push(json!({"request_id":admitted.request_id,"receipt":receipt,"control_revision":current.revision,"ready":current.ready,"observed_existing_publication":index==1}));
    }
    let artifact = artifacts::verify(artifacts::Context {
        store: &store,
        pool: &pool,
        token: credential.expose_token(),
        org: &org,
        computer: &plan
            .resources
            .iter()
            .find(|r| r.kind == DefinitionKind::Computer && r.name == "artifact")
            .unwrap()
            .resource_id,
        workspace: &plan
            .resources
            .iter()
            .find(|r| r.kind == DefinitionKind::Workspace && r.name == "artifact")
            .unwrap()
            .resource_id,
        worker: &worker_config,
        config: &config,
        root: &root,
        owner: &owner,
    })
    .await;
    let continuation = continuation::verify(continuation::Context {
        store: &store,
        pool: &pool,
        token: credential.expose_token(),
        org: &org,
        computers: ["parallel-one", "parallel-two"].map(|name| {
            plan.resources
                .iter()
                .find(|r| r.kind == DefinitionKind::Computer && r.name == name)
                .unwrap()
                .resource_id
                .as_str()
        }),
        workspace: &plan
            .resources
            .iter()
            .find(|r| r.kind == DefinitionKind::Workspace && r.name == "parallel")
            .unwrap()
            .resource_id,
        worker: &worker_config,
        config: &config,
        root: &root,
        owner: &owner,
    })
    .await;
    let evidence = json!({"continuation":continuation,"artifact":artifact,"organization":org.as_str(),"pvc_uid":pvc.uid(),"pv_uid":pv.uid(),"volume_path":pv.handle(),"filesystem_uuid":target.filesystem_uuid,"observations":observations,"file_writer":file_evidence,"limits":["single VM","file-only checkpoint; App state and general process fencing pending","bounded file gateway only; no product Pod launch or general process fencing","no power loss or HA test"]});
    fs::write(
        config["observation_file"].as_str().unwrap(),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("CANDIDATE_WORKER_REAL_PREPARATION_AND_OBSERVATION_PASSED");
}
