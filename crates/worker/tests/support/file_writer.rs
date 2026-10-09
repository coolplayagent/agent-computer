use super::*;
use agent_computer_storage::files::{FileEdit, FileEditState, FileVersion};
use agent_computer_store::{
    Error,
    runtime::{connections::*, writers::*},
};
use agent_computer_worker::files::{Configuration as FileConfig, SaveRequest, save_once};

pub struct Context<'a> {
    pub store: &'a Store,
    pub pool: &'a sqlx::PgPool,
    pub token: &'a str,
    pub org: &'a OrganizationId,
    pub actor: &'a PrincipalId,
    pub computer: &'a str,
    pub start: &'a StartReceipt,
    pub worker: &'a Value,
    pub config: &'a Value,
    pub data: &'a std::path::Path,
}
fn command(lease: &WriterLease) -> WriterLeaseCommand {
    WriterLeaseCommand {
        connection_session_id: lease.connection_session_id.clone(),
        generation: lease.generation,
        epoch: lease.epoch,
        expected_revision: lease.revision,
    }
}
fn edit(bytes: &[u8], expected: Option<FileVersion>) -> FileEdit {
    FileEdit {
        path: "saved.bin".into(),
        expected,
        content: bytes.into(),
        executable: false,
    }
}
fn config(c: &Context<'_>) -> FileConfig {
    serde_json::from_value(json!({"target":c.worker["target"],"mount_root":c.worker["mount_root"]}))
        .unwrap()
}
fn mount(c: &Context<'_>) -> MountedVolume {
    let f = config(c);
    let t = f.target;
    MountedVolume::open(
        &f.mount_root,
        &t.volume_path,
        &t.filesystem_uuid,
        &t.pvc_uid,
        t.writer_uid,
        t.writer_gid,
    )
    .unwrap()
}
async fn acquire(c: &Context<'_>, input: &AcquireWriterLease, name: &str) -> WriterLease {
    c.store
        .acquire_candidate_writer(c.token, &key(name), c.computer, input)
        .await
        .unwrap()
}
async fn execute(
    c: &Context<'_>,
    lease: &WriterLease,
    name: &str,
    edit: &FileEdit,
) -> ClosedWriter {
    let (prepared, _) = c
        .store
        .candidate_writer_storage(c.token, &lease.lease_id, &command(lease))
        .await
        .unwrap();
    let digest = edit.digest(&prepared).unwrap();
    let permit = c
        .store
        .begin_candidate_writer_dispatch(
            c.token,
            &lease.lease_id,
            &command(lease),
            WriterDispatch {
                dispatch_id: name,
                input_digest: &digest,
            },
        )
        .await
        .unwrap();
    permit.edit_file(mount(c), edit).unwrap()
}

pub async fn verify(c: Context<'_>) -> Value {
    let session = c
        .store
        .create_connection_session(
            c.token,
            &key("file-connect"),
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
    let input = AcquireWriterLease {
        scope: WriterScope::Modify,
        connection_session_id: session.session_id,
        generation: c.start.generation,
        candidate_id: c.start.candidate_id.clone(),
        duration_seconds: 30,
    };
    let lease = acquire(&c, &input, "file-acquire").await;
    let config_file = PathBuf::from(c.config["observation_file"].as_str().unwrap())
        .with_file_name("file-worker.json");
    let request_file = config_file.with_file_name("file-request.json");
    let token_file = config_file.with_file_name("file-credential");
    for (path,bytes) in [(&config_file,serde_json::to_vec(&json!({"target":c.worker["target"],"mount_root":c.worker["mount_root"]})).unwrap()),(&request_file,serde_json::to_vec(&json!({"lease":command(&lease),"dispatch_id":"file-cli","edit":edit(b"first-file",None)})).unwrap()),(&token_file,c.token.as_bytes().to_vec())] { fs::write(path,bytes).unwrap();fs::set_permissions(path,fs::Permissions::from_mode(0o600)).unwrap(); }
    let cli = || {
        let output = std::process::Command::new(c.config["server_binary"].as_str().unwrap())
            .args([
                "candidate-file-save-once",
                "--database-url-file",
                c.config["database_url_file"].as_str().unwrap(),
                "--credential-file",
            ])
            .arg(&token_file)
            .args(["--lease-id", &lease.lease_id, "--config-file"])
            .arg(&config_file)
            .arg("--request-file")
            .arg(&request_file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<WriterLease>(&output.stdout).unwrap()
    };
    let first = cli();
    assert_eq!(first.state, WriterLeaseState::Released);
    assert_eq!(first.release_proof.as_deref(), Some("bounded_file_drained"));
    assert_eq!(
        first.file_edit.as_ref().unwrap().state,
        FileEditState::Applied
    );
    let path = c.data.join("saved.bin");
    let inode = path.metadata().unwrap().ino();
    assert_eq!(fs::read(&path).unwrap(), b"first-file");
    let retry = cli();
    assert_eq!(retry.revision, first.revision);
    assert_eq!(path.metadata().unwrap().ino(), inode);
    fs::remove_file(token_file).unwrap();
    let lease = acquire(&c, &input, "file-next").await;
    assert_eq!(lease.epoch, 2);
    let second = save_once(
        c.store,
        c.token,
        &lease.lease_id,
        SaveRequest {
            lease: command(&lease),
            dispatch_id: "file-update".into(),
            edit: edit(
                b"second-file",
                first.file_edit.as_ref().unwrap().version.clone(),
            ),
        },
        config(&c),
    )
    .await
    .unwrap();
    assert_eq!(
        second.file_edit.as_ref().unwrap().state,
        FileEditState::Applied
    );
    assert_ne!(path.metadata().unwrap().ino(), inode);
    let lease = acquire(&c, &input, "file-conflict").await;
    let conflict = save_once(
        c.store,
        c.token,
        &lease.lease_id,
        SaveRequest {
            lease: command(&lease),
            dispatch_id: "file-stale-version".into(),
            edit: edit(
                b"overwrite",
                first.file_edit.as_ref().unwrap().version.clone(),
            ),
        },
        config(&c),
    )
    .await
    .unwrap();
    assert_eq!(conflict.state, WriterLeaseState::Released);
    assert_eq!(
        conflict.file_edit.as_ref().unwrap().state,
        FileEditState::Conflict
    );
    assert_eq!(fs::read(&path).unwrap(), b"second-file");
    let lease = acquire(&c, &input, "file-revoke").await;
    let closed = execute(
        &c,
        &lease,
        "file-revoked-completion",
        &edit(
            b"observed-after-revoke",
            second.file_edit.as_ref().unwrap().version.clone(),
        ),
    )
    .await;
    let grant = || RuntimeGrant {
        organization: c.org,
        principal: c.actor,
        kind: RuntimeKind::Computer,
        resource_id: c.computer,
        permission: RuntimePermission::Modify,
        max_runtime_seconds: None,
    };
    c.store.set_runtime_grant(grant(), false).await.unwrap();
    let revoked = c.store.finish_candidate_file_edit(&closed).await.unwrap();
    assert_eq!(revoked.state, WriterLeaseState::Released);
    assert_eq!(
        revoked.file_edit.as_ref().unwrap().state,
        FileEditState::Unknown
    );
    assert!(revoked.file_edit.as_ref().unwrap().drain_confirmed);
    c.store.set_runtime_grant(grant(), true).await.unwrap();
    let lease = acquire(&c, &input, "file-rollback").await;
    let closed = execute(
        &c,
        &lease,
        "file-rollback-completion",
        &edit(
            b"retained-after-rollback",
            closed.observed().version.clone(),
        ),
    )
    .await;
    sqlx::raw_sql("CREATE FUNCTION reject_file_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM events WHERE organization=NEW.organization AND sequence=NEW.sequence AND kind='writer.file_completed') THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_file_completion BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_file_completion();").execute(c.pool).await.unwrap();
    assert!(matches!(
        c.store.finish_candidate_file_edit(&closed).await,
        Err(Error::Database(_))
    ));
    let pending = c
        .store
        .candidate_writer(c.token, &lease.lease_id)
        .await
        .unwrap();
    assert!(pending.file_edit.is_none());
    assert!(pending.release_proof.is_none());
    sqlx::raw_sql(
        "DROP TRIGGER reject_file_completion ON outbox; DROP FUNCTION reject_file_completion();",
    )
    .execute(c.pool)
    .await
    .unwrap();
    let completed = c.store.finish_candidate_file_edit(&closed).await.unwrap();
    let again = c.store.finish_candidate_file_edit(&closed).await.unwrap();
    assert_eq!(again.revision, completed.revision);
    assert_eq!(completed.state, WriterLeaseState::Released);
    assert_eq!(
        completed.file_edit.as_ref().unwrap().state,
        FileEditState::Applied
    );
    let saved_inode = path.metadata().unwrap().ino();
    assert_eq!(fs::read(&path).unwrap(), b"retained-after-rollback");
    let lease = acquire(&c, &input, "file-unknown").await;
    std::os::unix::fs::symlink("/etc/passwd", c.data.join("escape")).unwrap();
    let unknown = save_once(
        c.store,
        c.token,
        &lease.lease_id,
        SaveRequest {
            lease: command(&lease),
            dispatch_id: "file-unsafe-target".into(),
            edit: FileEdit {
                path: "escape".into(),
                ..edit(b"never-write", None)
            },
        },
        config(&c),
    )
    .await
    .unwrap();
    assert_eq!(unknown.state, WriterLeaseState::Draining);
    assert!(!unknown.file_edit.as_ref().unwrap().drain_confirmed);
    assert_eq!(
        unknown.file_edit.as_ref().unwrap().state,
        FileEditState::Unknown
    );
    assert!(matches!(
        c.store
            .acquire_candidate_writer(c.token, &key("cannot-take-over"), c.computer, &input)
            .await,
        Err(Error::WriterLeaseBusy)
    ));
    assert_eq!(
        c.store
            .reconcile_candidate_writer(c.org, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    json!({"cli_create_and_exact_retry":true,"saved_inode":saved_inode,"final_file":completed.file_edit,"final_path":"saved.bin","stale_version":conflict.file_edit,"revoked_completion":revoked.file_edit,"outbox_rollback_and_same_evidence_retry":true,"uncertain_io_blocks_handoff":unknown,"candidate_ready":false})
}
