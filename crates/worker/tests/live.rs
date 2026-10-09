//! Real PostgreSQL + Kubernetes/CSI integration; explicit opt-in, no fixture backend.
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{Client, Deployment, volume::StorageClassBinding};
use agent_computer_store::{
    Store,
    auth::{IssueCredential, PrincipalKind, ServiceScope},
    plans::{DefinitionGrant, DefinitionKind, DefinitionPermission},
    reconciliation::WorkerId,
};
use agent_computer_test_support::Postgres;
use agent_computer_worker::{WorkResult, reconcile_volume_once};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn authorized_volume_intent_provisions_and_persists_actual_pvc_and_pv_uids() {
    let path = std::env::var("AGENT_COMPUTER_VOLUME_TEST_CONFIG")
        .expect("explicit disposable CSI cluster configuration required");
    let config: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let ca = std::fs::read(config["ca_file"].as_str().unwrap()).unwrap();
    let token = std::fs::read_to_string(config["token_file"].as_str().unwrap()).unwrap();
    let deployment: Deployment = serde_json::from_value(config["deployment"].clone()).unwrap();
    let mut storage: StorageClassBinding =
        serde_json::from_value(config["storage"].clone()).unwrap();
    let client = Client::new(
        config["api_url"].as_str().unwrap(),
        &ca,
        token.trim(),
        deployment,
    )
    .unwrap();
    let db = Postgres::new().await;
    let pool = db.pool.clone();
    let store = Store::new(pool.clone());
    store.migrate().await.unwrap();
    let org = OrganizationId::new(format!(
        "volume_probe_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
    .unwrap();
    let principal = PrincipalId::new("operator").unwrap();
    let credential = store
        .issue_credential(IssueCredential {
            organization: &org,
            principal: &principal,
            kind: PrincipalKind::Human,
            scopes: &[ServiceScope::DefinitionsManage],
            lifetime: Duration::from_secs(600),
        })
        .await
        .unwrap();
    for (kind, name, permission) in [
        (
            DefinitionKind::Declaration,
            "*",
            DefinitionPermission::Create,
        ),
        (DefinitionKind::Volume, "*", DefinitionPermission::Create),
        (
            DefinitionKind::StorageClass,
            "juicefs",
            DefinitionPermission::Reference,
        ),
    ] {
        store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &principal,
                    kind,
                    name,
                    permission,
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
    storage.reference = format!("id:{catalog}");
    let definition=validate_bytes(&serde_json::to_vec(&json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"volume-probe"},"spec":{"volumes":[{"name":"data","storageClass":"juicefs","quotaBytes":1073741824u64,"reclaimPolicy":"Retain"}]}})).unwrap(),Format::Json).unwrap();
    let plan = store
        .create_definition_plan(
            credential.expose_token(),
            &IdempotencyKey::new("plan").unwrap(),
            &definition,
        )
        .await
        .unwrap();
    let operation = store
        .apply_definition_plan(
            credential.expose_token(),
            &IdempotencyKey::new("apply").unwrap(),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    let worker = WorkerId::new("volume-test-worker").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let result = reconcile_volume_once(&store, &client, &storage, &org, &worker)
            .await
            .unwrap();
        if let WorkResult::Progress { progress } = result {
            assert_ne!(
                progress.state,
                agent_computer_store::reconciliation::IntentState::Blocked,
                "worker blocked: {progress:?}"
            );
        }
        let current = store
            .definition_operation(credential.expose_token(), &operation.operation_id)
            .await
            .unwrap();
        if current.state == "Succeeded" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "volume did not bind"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let bindings: Vec<(String, Value)> = sqlx::query_as(
        "SELECT role,binding FROM reconciliation_objects WHERE organization=$1 ORDER BY role",
    )
    .bind(org.as_str())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(bindings.len(), 2);
    assert!(matches!(
        reconcile_volume_once(&store, &client, &storage, &org, &worker)
            .await
            .unwrap(),
        WorkResult::Idle
    ));
    if let Some(path) = config["evidence_file"].as_str() {
        std::fs::write(path,serde_json::to_vec_pretty(&json!({"organization":org.as_str(),"operation":operation.operation_id,"bindings":bindings,"resource_id":plan.resources[0].resource_id,"limits":"PV provisioning only; no Workspace readiness or durability claim"})).unwrap()).unwrap();
    }
    println!(
        "real authorized PostgreSQL intent -> JuiceFS CSI PVC/PV binding succeeded; retained data is not deleted by this worker"
    );
    pool.close().await;
}
