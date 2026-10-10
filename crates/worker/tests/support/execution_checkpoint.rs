//! File-only lifecycle after real accepted gVisor execution completion.
use super::*;
use agent_computer_store::runtime::artifacts::CheckpointStop;
use sqlx::Row;
use std::os::unix::fs::MetadataExt;

pub struct Context<'a> {
    pub store: &'a Store,
    pub pool: &'a sqlx::PgPool,
    pub org: &'a OrganizationId,
    pub actor: &'a PrincipalId,
    pub token: &'a str,
    pub config: &'a Value,
    pub local: &'a Value,
    pub normal: &'a Value,
}
pub async fn verify(c: Context<'_>) -> Value {
    let source=sqlx::query("SELECT r.computer_id,r.workspace_id,r.request_id FROM execution_requests e JOIN candidate_writer_leases l USING(organization,lease_id) JOIN runtime_start_requests r ON r.organization=l.organization AND r.request_id=l.request_id WHERE e.organization=$1 AND e.execution_id=$2")
        .bind(c.org.as_str()).bind(c.normal["execution_id"].as_str().unwrap()).fetch_one(c.pool).await.unwrap();
    let computer: String = source.try_get("computer_id").unwrap();
    let workspace: String = source.try_get("workspace_id").unwrap();
    for (kind, id, permission) in [
        (RuntimeKind::Computer, &computer, RuntimePermission::Manage),
        (
            RuntimeKind::Workspace,
            &workspace,
            RuntimePermission::Publish,
        ),
    ] {
        c.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: c.org,
                    principal: c.actor,
                    kind,
                    resource_id: id,
                    permission,
                    max_runtime_seconds: None,
                },
                true,
            )
            .await
            .unwrap();
    }
    let current = c.store.computer_runtime(c.token, &computer).await.unwrap();
    let request = CheckpointStop {
        request_id: source.try_get("request_id").unwrap(),
        expected_revision: current.revision,
        publish_current: true,
    };
    let admitted = c
        .store
        .checkpoint_stop_computer(
            c.token,
            &key("after-execution-checkpoint"),
            &computer,
            &request,
        )
        .await
        .unwrap();
    assert!(admitted.stop_after_commit && admitted.stop_receipt.is_none());
    let spool = PathBuf::from(field(c.config, "output_spool")).join(&admitted.commit_id);
    fs::create_dir(&spool).unwrap();
    fs::set_permissions(&spool, fs::Permissions::from_mode(0o700)).unwrap();
    let config = json!({"storage":{"target":c.local["target"],"mount_root":c.local["mount_root"]},"objects":c.config["outputs"],"spool":spool});
    let committed = agent_computer_worker::artifacts::publish_once(
        c.store,
        c.org,
        &admitted.commit_id,
        &WorkerId::new("checkpoint").unwrap(),
        serde_json::from_value(config).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let stopped = committed.stop_receipt.as_ref().unwrap();
    assert_eq!(stopped.proof, "artifact_checkpoint");
    assert!(
        stopped
            .checkpoint
            .as_ref()
            .unwrap()
            .unfinished_execution_ids
            .is_empty()
    );
    let durable:Value=sqlx::query_scalar("SELECT jsonb_build_object('capture',capture,'object_ref',object_ref) FROM artifact_commits WHERE organization=$1 AND commit_id=$2").bind(c.org.as_str()).bind(&admitted.commit_id).fetch_one(c.pool).await.unwrap();
    let next = c
        .store
        .admit_computer_start(
            c.token,
            &key("restart-after-execution-checkpoint"),
            &computer,
            &StartRequest {
                expected_revision: stopped.control_revision,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
                input_artifact_id: Some(admitted.commit_id.clone()),
            },
        )
        .await
        .unwrap();
    assert_ne!(next.candidate_id, committed.candidate_id);
    let cache = spool.join("fresh-restoration-cache");
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    let mut restore = c.local.clone();
    restore["object_cache"] = json!(cache);
    restore["artifacts"] = c.config["outputs"].clone();
    assert!(matches!(
        candidate::prepare_once(
            c.store,
            c.org,
            &next.request_id,
            &WorkerId::new("restore-checkpoint").unwrap(),
            serde_json::from_value(restore).unwrap()
        )
        .await
        .unwrap(),
        candidate::WorkResult::Prepared
    ));
    let prepared: Value = sqlx::query_scalar(
        "SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
    )
    .bind(c.org.as_str())
    .bind(&next.request_id)
    .fetch_one(c.pool)
    .await
    .unwrap();
    let root =
        PathBuf::from(field(c.local, "mount_root")).join(field(&c.local["target"], "volume_path"));
    let original = root.join(field(&c.normal["prepared"], "path_ref"));
    let restored = root.join(field(&prepared, "path_ref"));
    for (path, bytes) in [
        ("output.txt", b"persisted".as_slice()),
        ("after-execution.txt", b"next-writer"),
        ("queue-concurrent.txt", b"concurrent"),
        ("queue-started.txt", b"active"),
    ] {
        assert_eq!(fs::read(restored.join(path)).unwrap(), bytes);
        assert_ne!(
            restored.join(path).metadata().unwrap().ino(),
            original.join(path).metadata().unwrap().ino()
        );
    }
    assert!(
        !c.store
            .computer_runtime(c.token, &computer)
            .await
            .unwrap()
            .ready
    );
    json!({"commit":committed,"durable":durable,"new_start":next,"restored":prepared,"fresh_cache":true,"independent_inodes":true,"files_preserved":["output.txt","after-execution.txt","queue-concurrent.txt","queue-started.txt"]})
}
