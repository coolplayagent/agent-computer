use super::*;
use agent_computer_storage::files::FileEdit;
use agent_computer_store::runtime::{artifacts::*, connections::*, writers::*};
use agent_computer_worker::files::{SaveRequest, save_once};

pub struct Context<'a> {
    pub store: &'a Store,
    pub pool: &'a sqlx::PgPool,
    pub token: &'a str,
    pub org: &'a OrganizationId,
    pub computer: &'a str,
    pub workspace: &'a str,
    pub worker: &'a Value,
    pub config: &'a Value,
    pub root: &'a std::path::Path,
    pub owner: &'a WorkerId,
}
fn private(path: &std::path::Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn publish(c: &Context<'_>, id: &str, config: &std::path::Path) -> std::process::Output {
    std::process::Command::new(c.config["server_binary"].as_str().unwrap())
        .args([
            "artifact-publish-once",
            "--database-url-file",
            c.config["database_url_file"].as_str().unwrap(),
            "--organization",
            c.org.as_str(),
            "--worker-id",
            c.owner.as_str(),
            "--commit-id",
            id,
            "--config-file",
        ])
        .arg(config)
        .output()
        .unwrap()
}
pub async fn verify(c: Context<'_>) -> Value {
    let start = c
        .store
        .admit_computer_start(
            c.token,
            &key("artifact-start"),
            c.computer,
            &StartRequest {
                expected_revision: 1,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        prepare_once(
            c.store,
            c.org,
            &start.request_id,
            c.owner,
            serde_json::from_value(c.worker.clone()).unwrap()
        )
        .await
        .unwrap(),
        WorkResult::Prepared
    ));
    let prepared: Prepared = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(c.org.as_str())
        .bind(&start.request_id)
        .fetch_one(c.pool)
        .await
        .unwrap(),
    )
    .unwrap();
    let data = c.root.join(&prepared.path_ref);
    let session = c
        .store
        .create_connection_session(
            c.token,
            &key("artifact-connect"),
            c.computer,
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
    let acquire = AcquireWriterLease {
        scope: WriterScope::Modify,
        connection_session_id: session.session_id.clone(),
        generation: start.generation,
        candidate_id: start.candidate_id.clone(),
        duration_seconds: 30,
    };
    let files = [
        ("saved.bin", vec![0x5a; 1024 * 1024], false),
        ("empty", vec![], false),
        ("run.sh", b"#!/bin/sh\nprintf restored\\n\n".to_vec(), true),
    ];
    for (i, (path, bytes, executable)) in files.iter().enumerate() {
        let lease = c
            .store
            .acquire_candidate_writer(
                c.token,
                &key(&format!("artifact-write-{i}")),
                c.computer,
                &acquire,
            )
            .await
            .unwrap();
        let done = save_once(
            c.store,
            c.token,
            &lease.lease_id,
            SaveRequest {
                lease: WriterLeaseCommand {
                    connection_session_id: session.session_id.clone(),
                    generation: lease.generation,
                    epoch: lease.epoch,
                    expected_revision: lease.revision,
                },
                dispatch_id: format!("artifact-file-{i}"),
                edit: FileEdit {
                    path: (*path).into(),
                    expected: None,
                    content: bytes.clone(),
                    executable: *executable,
                },
            },
            serde_json::from_value(
                json!({"target":c.worker["target"],"mount_root":c.worker["mount_root"]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(done.state, WriterLeaseState::Released);
        assert_eq!(done.release_proof.as_deref(), Some("bounded_file_drained"));
    }
    let current = c.store.computer_runtime(c.token, c.computer).await.unwrap();
    let input = CommitArtifact {
        request_id: start.request_id.clone(),
        expected_revision: current.revision,
        base_revision: start.input_revision.unwrap(),
        base_manifest: start.input_manifest_digest.clone().unwrap(),
        publish_current: true,
    };
    let admitted = c
        .store
        .commit_workspace_artifact(c.token, &key("artifact-publish"), c.workspace, &input)
        .await
        .unwrap();
    assert!(
        c.store
            .acquire_candidate_writer(c.token, &key("sealed-no-writes"), c.computer, &acquire)
            .await
            .is_err()
    );
    let configuration = PathBuf::from(c.config["observation_file"].as_str().unwrap())
        .with_file_name("artifact-command.json");
    let good = json!({"storage":{"target":c.worker["target"],"mount_root":c.worker["mount_root"]},"objects":c.config["artifacts"],"spool":c.config["artifact_spool"]});
    let mut bad = good.clone();
    bad["objects"]["credentials_file"] = c.config["rejected_artifact_credentials_file"].clone();
    private(&configuration, &bad);
    assert!(
        !publish(&c, &admitted.commit_id, &configuration)
            .status
            .success()
    );
    let captured: Value = sqlx::query_scalar(
        "SELECT capture FROM artifact_commits WHERE organization=$1 AND commit_id=$2",
    )
    .bind(c.org.as_str())
    .bind(&admitted.commit_id)
    .fetch_one(c.pool)
    .await
    .unwrap();
    assert_eq!(
        captured["manifest"]["entries"].as_object().unwrap().len(),
        files.len()
    );
    assert_eq!(
        c.store
            .workspace_artifact(c.token, &admitted.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
    assert!(
        c.store
            .artifact_manifest(c.token, &admitted.commit_id)
            .await
            .unwrap()
            .is_none()
    );
    private(&configuration, &good);
    sqlx::raw_sql("CREATE FUNCTION reject_artifact_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM events WHERE organization=NEW.organization AND sequence=NEW.sequence AND kind='artifact.committed') THEN RAISE EXCEPTION 'injected artifact outbox failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_artifact_outbox BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_artifact_outbox();").execute(c.pool).await.unwrap();
    assert!(
        !publish(&c, &admitted.commit_id, &configuration)
            .status
            .success()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM workspace_input_heads WHERE organization=$1 AND workspace_id=$2"
        )
        .bind(c.org.as_str())
        .bind(c.workspace)
        .fetch_one(c.pool)
        .await
        .unwrap(),
        1
    );
    sqlx::raw_sql(
        "DROP TRIGGER reject_artifact_outbox ON outbox; DROP FUNCTION reject_artifact_outbox();",
    )
    .execute(c.pool)
    .await
    .unwrap();
    // Discard this worker's local spool. A new process must recover from the
    // recorded manifest and S3 objects, without recapturing the Candidate.
    let spool = PathBuf::from(c.config["artifact_spool"].as_str().unwrap());
    fs::rename(&spool, spool.with_extension("retained")).unwrap();
    fs::create_dir(&spool).unwrap();
    fs::set_permissions(&spool, fs::Permissions::from_mode(0o700)).unwrap();
    let output = publish(&c, &admitted.commit_id, &configuration);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let committed: ArtifactCommit = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(committed.state, ArtifactState::Committed);
    assert_eq!(committed.input_revision, Some(2));
    assert_eq!(
        c.store
            .commit_workspace_artifact(c.token, &key("artifact-publish"), c.workspace, &input)
            .await
            .unwrap(),
        committed
    );
    assert_eq!(
        c.store
            .artifact_manifest(c.token, &admitted.commit_id)
            .await
            .unwrap()
            .unwrap()
            .entries
            .len(),
        3
    );
    assert!(
        publish(&c, &admitted.commit_id, &configuration)
            .status
            .success()
    );
    c.store
        .close_connection_session(c.token, &session.session_id)
        .await
        .unwrap();
    let current = c.store.computer_runtime(c.token, c.computer).await.unwrap();
    let stopped = c
        .store
        .stop_prepared_computer(
            c.token,
            &key("artifact-stop"),
            c.computer,
            &StopPreparedComputer {
                expected_revision: current.revision,
                request_id: start.request_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(stopped.proof, "artifact_checkpoint");
    assert_eq!(stopped.input_revision, 2);
    // Privileged fault injection changes the retained old directory. Restoration
    // must use the committed S3 bytes, never copy this retained mutable directory.
    fs::write(
        data.join("saved.bin"),
        b"privileged-retained-directory-fault",
    )
    .unwrap();
    let cache = PathBuf::from(c.worker["object_cache"].as_str().unwrap());
    fs::rename(&cache, cache.with_extension("retained")).unwrap();
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    let next = c
        .store
        .admit_computer_start(
            c.token,
            &key("artifact-restart"),
            c.computer,
            &StartRequest {
                expected_revision: stopped.control_revision,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
            },
        )
        .await
        .unwrap();
    assert_eq!(next.input_revision, Some(2));
    assert_ne!(next.candidate_id, start.candidate_id);
    let mut restoration = c.worker.clone();
    restoration["artifacts"] = c.config["artifacts"].clone();
    assert!(matches!(
        prepare_once(
            c.store,
            c.org,
            &next.request_id,
            c.owner,
            serde_json::from_value(restoration).unwrap()
        )
        .await
        .unwrap(),
        WorkResult::Prepared
    ));
    let restored: Prepared = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2",
        )
        .bind(c.org.as_str())
        .bind(&next.request_id)
        .fetch_one(c.pool)
        .await
        .unwrap(),
    )
    .unwrap();
    let restored_data = c.root.join(&restored.path_ref);
    for (path, bytes, executable) in &files {
        let new = restored_data.join(path);
        assert_eq!(fs::read(&new).unwrap(), *bytes);
        let metadata = new.metadata().unwrap();
        assert_eq!(metadata.mode() & 0o111 != 0, *executable);
        assert_eq!(metadata.nlink(), 1);
        assert_ne!(metadata.ino(), data.join(path).metadata().unwrap().ino());
    }
    // Existing preparation observation needs neither an object download nor S3 configuration.
    assert!(matches!(
        prepare_once(
            c.store,
            c.org,
            &next.request_id,
            c.owner,
            serde_json::from_value(c.worker.clone()).unwrap()
        )
        .await
        .unwrap(),
        WorkResult::Prepared
    ));
    assert!(data.is_dir());
    assert_eq!(
        fs::read(data.join("saved.bin")).unwrap(),
        b"privileged-retained-directory-fault"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM events WHERE organization=$1 AND kind='artifact.committed'"
        )
        .bind(c.org.as_str())
        .fetch_one(c.pool)
        .await
        .unwrap(),
        1
    );
    json!({"commit":committed,"checkpoint_stop":stopped,"capture":captured,"restored":restored,"new_start":next,"rejected_credentials_preserve_sealed_candidate":true,"outbox_rollback":true,"fresh_process_retry_without_local_spool":true,"restored_from_s3_without_cache":true,"retained_source_mutation_did_not_change_restoration":true,"independent_inodes_and_executable_mode":true,"sealed_writer_rejected":true})
}
