// Metadata fixtures shared in shape with server contract tests. No physical runtime proof.
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_store::{
    Store,
    plans::*,
    reconciliation::*,
    runtime::{preparation::*, *},
};
use serde_json::json;
use std::time::Duration;

pub async fn provision_with_sandbox(
    store: &Store,
    token: &str,
    actor: &str,
    with_sandbox: bool,
) -> String {
    let org = OrganizationId::new("acme").unwrap();
    let principal = PrincipalId::new(actor).unwrap();
    for kind in [
        DefinitionKind::Declaration,
        DefinitionKind::Computer,
        DefinitionKind::Workspace,
        DefinitionKind::Volume,
    ] {
        store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &principal,
                    kind,
                    name: "*",
                    permission: DefinitionPermission::Create,
                },
                true,
            )
            .await
            .unwrap();
    }
    store
        .register_catalog_reference(&org, DefinitionKind::StorageClass, "connection-storage")
        .await
        .unwrap();
    store
        .set_definition_grant(
            DefinitionGrant {
                organization: &org,
                principal: &principal,
                kind: DefinitionKind::StorageClass,
                name: "connection-storage",
                permission: DefinitionPermission::Reference,
            },
            true,
        )
        .await
        .unwrap();
    let mut document = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"connections"},"spec":{
        "volumes":[{"name":"data","storageClass":"connection-storage","quotaBytes":10737418240_i64,"reclaimPolicy":"Retain"}],
        "workspaces":[{"name":"work","volumeRef":"data","conflictPolicy":"explicit"}],
        "computers":[{"name":"computer","workspaceRef":"work","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"}]
    }});
    if with_sandbox {
        store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &principal,
                    kind: DefinitionKind::Sandbox,
                    name: "*",
                    permission: DefinitionPermission::Create,
                },
                true,
            )
            .await
            .unwrap();
        store
            .register_catalog_reference(&org, DefinitionKind::NetworkPolicy, "deny-all")
            .await
            .unwrap();
        store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &principal,
                    kind: DefinitionKind::NetworkPolicy,
                    name: "deny-all",
                    permission: DefinitionPermission::Reference,
                },
                true,
            )
            .await
            .unwrap();
        document["spec"]["sandboxes"] = json!([{"name":"exec","runtimeClass":"gvisor","image":format!("registry.example.invalid/tools@sha256:{}","a".repeat(64)),"resources":{"cpuMillis":500,"memoryMiB":256},"networkPolicyRef":"deny-all"}]);
        document["spec"]["computers"][0]["sandboxRefs"] = json!(["exec"]);
    }
    let plan = store
        .create_definition_plan(
            token,
            &IdempotencyKey::new("connections-plan").unwrap(),
            &validate_bytes(&serde_json::to_vec(&document).unwrap(), Format::Json).unwrap(),
        )
        .await
        .unwrap();
    store
        .apply_definition_plan(
            token,
            &IdempotencyKey::new("connections-apply").unwrap(),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    let computer = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Computer)
        .unwrap()
        .resource_id
        .clone();
    for permission in [RuntimePermission::Connect, RuntimePermission::Read] {
        store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org,
                    principal: &principal,
                    kind: RuntimeKind::Computer,
                    resource_id: &computer,
                    permission,
                    max_runtime_seconds: None,
                },
                true,
            )
            .await
            .unwrap();
    }
    computer
}
pub async fn prepared(
    store: &agent_computer_store::Store,
    pool: &sqlx::PgPool,
    token: &str,
    computer: &str,
    principal: &str,
) -> StartReceipt {
    let org = OrganizationId::new("acme").unwrap();
    let actor = PrincipalId::new(principal).unwrap();
    let workspace: String =
        sqlx::query_scalar("SELECT resource_id FROM resource_definitions WHERE kind='workspace'")
            .fetch_one(pool)
            .await
            .unwrap();
    for (kind, id, permission) in [
        (RuntimeKind::Computer, computer, RuntimePermission::Activate),
        (RuntimeKind::Computer, computer, RuntimePermission::Modify),
        (RuntimeKind::Workspace, &workspace, RuntimePermission::Read),
        (
            RuntimeKind::Workspace,
            &workspace,
            RuntimePermission::Modify,
        ),
    ] {
        store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org,
                    principal: &actor,
                    kind,
                    resource_id: id,
                    permission,
                    max_runtime_seconds: (permission == RuntimePermission::Activate).then_some(300),
                },
                true,
            )
            .await
            .unwrap();
    }
    let worker = WorkerId::new("metadata-test-worker").unwrap();
    let ClaimOutcome::Claimed(volume) = store
        .claim_reconciliation_kind(
            &org,
            &worker,
            Duration::from_secs(180),
            DefinitionKind::Volume,
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let target = PreparationTarget {
        volume_id: volume.task().resource_id.clone(),
        namespace_uid: "namespace".into(),
        pvc_uid: "pvc".into(),
        pv_uid: "pv".into(),
        filesystem_uuid: "filesystem".into(),
        volume_path: "volume".into(),
        writer_uid: 1000,
        writer_gid: 1000,
    };
    store.begin_reconciliation_dispatch(&volume).await.unwrap();
    for (role, uid) in [("pvc", &target.pvc_uid), ("pv", &target.pv_uid)] {
        store
            .record_reconciliation_object(
                &volume,
                role,
                &ReconcileObject {
                    backend: "kubernetes_juicefs".into(),
                    name: format!("{role}-name"),
                    uid: uid.clone(),
                    scope_uid: target.namespace_uid.clone(),
                },
            )
            .await
            .unwrap();
    }
    store
        .finish_reconciliation(
            &volume,
            ReconcileOutcome::Applied {
                receipt: EffectReceipt {
                    step_id: volume.task().step_id.clone(),
                    resource_id: target.volume_id.clone(),
                    revision: volume.task().revision,
                    spec_digest: volume.task().spec_digest.clone(),
                    backend: "kubernetes_juicefs".into(),
                    object_uid: target.pvc_uid.clone(),
                    evidence_id: target.pv_uid.clone(),
                },
            },
        )
        .await
        .unwrap();
    let start = store
        .admit_computer_start(
            token,
            &IdempotencyKey::new("start").unwrap(),
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
    let PreparationClaim::Claimed(lease) = store
        .claim_candidate_preparation(&org, &start.request_id, &worker, &target)
        .await
        .unwrap()
    else {
        panic!()
    };
    store.begin_candidate_preparation(&lease).await.unwrap();
    // Synthetic receipts verify HTTP/DB authority, not physical storage execution.
    let r = lease.request();
    let evidence = serde_json::from_value(json!({"version":1,"request_digest":r.binding_digest(&target.volume_path,target.writer_uid,target.writer_gid).unwrap(),"filesystem_uuid":target.filesystem_uuid,"volume_uid":target.pvc_uid,"path_ref":r.path_ref(),"data_inode":123,"manifest_digest":r.manifest_digest,"quota_bytes":r.quota_bytes})).unwrap();
    store
        .finish_candidate_preparation(&lease, &evidence)
        .await
        .unwrap();
    start
}
