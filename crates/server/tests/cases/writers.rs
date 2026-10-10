use super::connections::{provision, req};
use crate::support::*;
use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_store::{
    auth::ServiceScope,
    plans::DefinitionKind,
    reconciliation::*,
    runtime::{connections::*, preparation::*, writers::*, *},
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::time::Duration;

pub(super) async fn prepared(
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
fn command(value: &Value) -> Value {
    json!({"connection_session_id":value["connection_session_id"],"generation":value["generation"],"epoch":value["epoch"],"expected_revision":value["revision"]})
}

#[tokio::test]
async fn writer_http_handoff_and_dispatched_release_have_distinct_results() {
    let s = Service::new().await;
    let issued = s.issue("alice", &ServiceScope::ALL).await;
    let token = issued.expose_token();
    let computer = provision(&s.store, token, "alice").await;
    let start = prepared(&s.store, &s.database.pool, token, &computer, "alice").await;
    let session = s
        .store
        .create_connection_session(
            token,
            &IdempotencyKey::new("connection").unwrap(),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::Modify,
                ],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    let path = format!("/v1alpha1/computers/{computer}/leases");
    let body = json!({"scope":"modify","connection_session_id":session.session_id,"generation":start.generation,"candidate_id":start.candidate_id});
    let (code, lease) = s
        .send(req(token, "POST", &path, Some("acquire"), body.clone()))
        .await;
    assert_eq!(code, StatusCode::CREATED, "{lease}");
    let lease_path = format!("/v1alpha1/leases/{}", lease["lease_id"].as_str().unwrap());
    let (code, renewed) = s
        .send(req(
            token,
            "POST",
            &format!("{lease_path}/renew"),
            Some("renew"),
            json!({"lease":command(&lease)}),
        ))
        .await;
    assert_eq!(code, StatusCode::OK, "{renewed}");
    let (code, released) = s
        .send(req(
            token,
            "POST",
            &format!("{lease_path}/release"),
            Some("release"),
            command(&renewed),
        ))
        .await;
    assert_eq!(code, StatusCode::OK, "{released}");
    assert_eq!(released["release_proof"], "no_dispatch");
    let (code, next) = s.send(req(token, "POST", &path, Some("next"), body)).await;
    assert_eq!(code, StatusCode::CREATED);
    assert_eq!(next["epoch"], 2);
    let permit = s
        .store
        .begin_candidate_writer_dispatch(
            token,
            next["lease_id"].as_str().unwrap(),
            &serde_json::from_value(command(&next)).unwrap(),
            WriterDispatch {
                dispatch_id: "writer-once",
                input_digest: &format!("sha256:{}", "b".repeat(64)),
            },
        )
        .await
        .unwrap();
    let (code, pending) = s
        .send(req(
            token,
            "POST",
            &format!("{lease_path}/release"),
            Some("drain"),
            command(&serde_json::to_value(permit.lease()).unwrap()),
        ))
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{pending}");
    assert_eq!(pending["state"], "Draining");
    assert_eq!(pending["release_proof"], Value::Null);
    assert!(!pending.to_string().contains("filesystem"));
    let (code, current) = s
        .send(req(token, "GET", &lease_path, None, Value::Null))
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(current["state"], "Draining");
    assert_eq!(
        s.send(req(
            token,
            "POST",
            &format!("{lease_path}/renew"),
            Some("inactive"),
            json!({"lease":command(&current)})
        ))
        .await
        .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn writer_http_requires_prepared_candidate_scopes_and_non_forgeable_owner() {
    let s = Service::new().await;
    let issued = s.issue("alice", &ServiceScope::ALL).await;
    let token = issued.expose_token();
    let computer = provision(&s.store, token, "alice").await;
    s.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &OrganizationId::new("acme").unwrap(),
                principal: &PrincipalId::new("alice").unwrap(),
                kind: RuntimeKind::Computer,
                resource_id: &computer,
                permission: RuntimePermission::Modify,
                max_runtime_seconds: None,
            },
            true,
        )
        .await
        .unwrap();
    let session = s
        .store
        .create_connection_session(
            token,
            &IdempotencyKey::new("connect").unwrap(),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::Modify,
                ],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    let path = format!("/v1alpha1/computers/{computer}/leases");
    let body = json!({"scope":"modify","connection_session_id":session.session_id,"generation":1,"candidate_id":"not-prepared"});
    assert_eq!(
        s.send(req(token, "POST", &path, Some("unprepared"), body.clone()))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        s.send(req(token, "POST", &path, None, body.clone()))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for (field, value) in [
        ("scope", json!("control")),
        ("duration_seconds", json!(31)),
        ("owner", json!("alice")),
        ("process_stopped", json!(true)),
        ("generation", json!(0)),
    ] {
        let mut forged = body.clone();
        forged[field] = value;
        assert_eq!(
            s.send(req(token, "POST", &path, Some("forged"), forged))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let limited = s.issue("alice", &[ServiceScope::RuntimeConnect]).await;
    assert_eq!(
        s.send(req(
            limited.expose_token(),
            "POST",
            &path,
            Some("limited"),
            body.clone()
        ))
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let other = s.issue("alice", &ServiceScope::ALL).await;
    assert_eq!(
        s.send(req(
            other.expose_token(),
            "POST",
            &path,
            Some("other"),
            body.clone()
        ))
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let mut origin = req(token, "POST", &path, Some("origin"), body);
    origin
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    assert_eq!(s.send(origin).await.0, StatusCode::FORBIDDEN);
}
