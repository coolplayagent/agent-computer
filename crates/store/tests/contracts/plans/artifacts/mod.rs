use super::runtime::allow;
use super::*;
use agent_computer_objects::{
    artifact::{Bundle, verify},
    sha256,
};
use agent_computer_storage::{Entry, Manifest};
use agent_computer_store::{
    Error,
    reconciliation::WorkerId,
    runtime::{artifacts::*, *},
};
mod continuation;
mod objects;
mod recovery;
use objects::Objects;
async fn setup() -> (Database, String, String, String, CommitArtifact) {
    setup_with_apps(false).await
}
async fn setup_with_apps(apps: bool) -> (Database, String, String, String, CommitArtifact) {
    setup_many(apps, 1).await
}
async fn setup_many(
    apps: bool,
    computers: usize,
) -> (Database, String, String, String, CommitArtifact) {
    let (db, token, computer, start, target) =
        super::preparation::setup_many(60 * 1024 * 1024 * 1024, apps, computers).await;
    let lease = super::preparation::claim(&db, &start, &target, "prepare").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    db.store
        .finish_candidate_preparation(&lease, &super::preparation::evidence(&lease))
        .await
        .unwrap();
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Modify,
        None,
    )
    .await;
    allow(
        &db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Publish,
        None,
    )
    .await;
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    let input = CommitArtifact {
        request_id: start.request_id,
        expected_revision: current.revision,
        base_revision: start.input_revision.unwrap(),
        base_manifest: start.input_manifest_digest.unwrap(),
        publish_current: true,
    };
    (db, token, computer, workspace, input)
}
async fn admit(
    db: &Database,
    token: &str,
    workspace: &str,
    input: &CommitArtifact,
) -> ArtifactCommit {
    db.store
        .commit_workspace_artifact(token, &key("artifact"), workspace, input)
        .await
        .unwrap()
}
async fn capture(db: &Database, id: &str, objects: &Objects) -> (ArtifactLease, Bundle) {
    capture_bytes(db, id, objects, b"retained-file").await
}
async fn capture_bytes(
    db: &Database,
    id: &str,
    objects: &Objects,
    bytes: &[u8],
) -> (ArtifactLease, Bundle) {
    let lease = db
        .store
        .claim_artifact(&org("acme"), id, &WorkerId::new("capture").unwrap())
        .await
        .unwrap()
        .unwrap();
    let hash = sha256(bytes);
    let object = objects
        .client
        .artifact_reference("acme", id, &hash, bytes.len() as u64)
        .unwrap();
    let bundle = Bundle {
        version: 1,
        organization: "acme".into(),
        commit_id: id.into(),
        store_digest: objects.client.store_digest().into(),
        prepared: lease.prepared().clone(),
        manifest: Manifest {
            entries: [(
                "saved.txt".into(),
                Entry::File {
                    sha256: hash,
                    size: bytes.len() as u64,
                    executable: false,
                },
            )]
            .into(),
        },
        chunks: [("saved.txt".into(), vec![object.clone()])].into(),
    };
    // Synthetic capture exercises transaction authority, not real filesystem IO.
    sqlx::query("UPDATE artifact_commits SET capture=$2 WHERE commit_id=$1")
        .bind(id)
        .bind(serde_json::to_value(&bundle).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
    objects
        .bytes
        .lock()
        .unwrap()
        .insert(format!("/artifacts/{}", object.key), bytes.to_vec());
    objects.bytes.lock().unwrap().insert(
        format!("/artifacts/{}", bundle.object(&objects.client).unwrap().key),
        bundle.bytes().unwrap(),
    );
    (lease, bundle)
}
#[tokio::test]
async fn artifact_publication_checkpoint_and_restart_survive_wal_and_retries() {
    let (mut db, token, computer, workspace, input) = setup().await;
    let first = admit(&db, &token, &workspace, &input).await;
    assert_eq!(first.state, ArtifactState::Capturing);
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Sealing)
    );
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    let done = db.store.finish_artifact(&lease, &verified).await.unwrap();
    assert_eq!(done.state, ArtifactState::Committed);
    assert_eq!(done.input_revision, Some(2));
    let events = count(&db, "events").await;
    assert_eq!(
        db.store.finish_artifact(&lease, &verified).await.unwrap(),
        done
    );
    assert_eq!(admit(&db, &token, &workspace, &input).await, done);
    assert_eq!(count(&db, "events").await, events);
    db.crash_and_restart().await;
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap(),
        done
    );
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    let stopped = db
        .store
        .stop_prepared_computer(
            &token,
            &key("stop"),
            &computer,
            &StopPreparedComputer {
                expected_revision: current.revision,
                request_id: input.request_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(stopped.proof, "artifact_checkpoint");
    assert_eq!(stopped.checkpoint.unwrap().artifact_id, done.commit_id);
    assert_eq!(stopped.input_revision, 2);
    let next = db
        .store
        .admit_computer_start(
            &token,
            &key("next"),
            &computer,
            &StartRequest {
                expected_revision: stopped.control_revision,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
                input_artifact_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(next.input_revision, Some(2));
    assert_ne!(next.candidate_id, done.candidate_id);
    assert_eq!(
        db.store
            .candidate_input_artifact(&org("acme"), &next.request_id)
            .await
            .unwrap(),
        Some(bundle)
    );
    assert_eq!(count(&db, "artifact_commits").await, 1);
}
#[tokio::test]
async fn artifact_branch_and_conflict_preserve_fixed_versions_without_overwriting_head() {
    for branch in [true, false] {
        let (db, token, computer, workspace, mut input) = setup().await;
        input.publish_current = !branch;
        let first = admit(&db, &token, &workspace, &input).await;
        let objects = Objects::new();
        let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
        if !branch {
            // A separately committed version races the originally pinned base.
            sqlx::query("INSERT INTO workspace_input_versions (organization,workspace_id,revision,manifest,digest,origin) SELECT organization,workspace_id,2,manifest,digest,origin FROM workspace_input_versions").execute(&db.pool).await.unwrap();
            sqlx::query("UPDATE workspace_input_heads SET revision=2")
                .execute(&db.pool)
                .await
                .unwrap();
        }
        let verified = verify(&objects.client, &bundle, None).await.unwrap();
        let done = db.store.finish_artifact(&lease, &verified).await.unwrap();
        assert_eq!(
            done.state,
            if branch {
                ArtifactState::Committed
            } else {
                ArtifactState::Conflict
            }
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT revision FROM workspace_input_heads")
                .fetch_one(&db.pool)
                .await
                .unwrap(),
            if branch { 1 } else { 2 }
        );
        let current = db.store.computer_runtime(&token, &computer).await.unwrap();
        let stopped = db
            .store
            .stop_prepared_computer(
                &token,
                &key("stop"),
                &computer,
                &StopPreparedComputer {
                    expected_revision: current.revision,
                    request_id: input.request_id,
                },
            )
            .await
            .unwrap();
        assert_eq!(stopped.proof, "artifact_checkpoint");
        assert_eq!(stopped.input_revision, done.input_revision.unwrap());
        assert_eq!(stopped.checkpoint.unwrap().artifact_id, done.commit_id);
        assert_eq!(count(&db, "runtime_stops").await, 1);
    }
}
#[tokio::test]
async fn artifact_outbox_failure_rolls_back_head_and_fresh_authority_is_required() {
    let (db, token, computer, workspace, input) = setup().await;
    let first = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_artifact() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END $$; CREATE TRIGGER reject_artifact BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_artifact();").execute(&db.pool).await.unwrap();
    assert!(db.store.finish_artifact(&lease, &verified).await.is_err());
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Sealing)
    );
    sqlx::query("DROP TRIGGER reject_artifact ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Workspace,
                resource_id: &workspace,
                permission: RuntimePermission::Publish,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(db.store.finish_artifact(&lease, &verified).await.is_err());
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    allow(
        &db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Publish,
        None,
    )
    .await;
    db.store.finish_artifact(&lease, &verified).await.unwrap();
    for sql in [
        "UPDATE artifact_commits SET capture='{}'::jsonb",
        "DELETE FROM artifact_commits",
        "UPDATE runtime_start_requests SET state='Prepared'",
        "UPDATE workspace_input_versions SET manifest='{}'::jsonb",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
}
#[tokio::test]
async fn artifact_retry_with_new_credential_invalidates_the_old_worker_lease() {
    let (db, token, _, workspace, input) = setup().await;
    let first = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let fresh = super::runtime::runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    admit(&db, &fresh, &workspace, &input).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    assert!(matches!(
        db.store.finish_artifact(&lease, &verified).await,
        Err(Error::StaleReconcileLease)
    ));
    let next = db
        .store
        .claim_artifact(
            &org("acme"),
            &first.commit_id,
            &WorkerId::new("replacement").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.capture(), Some(&bundle));
    db.store.finish_artifact(&next, &verified).await.unwrap();
}
#[tokio::test]
async fn artifact_rejects_stale_base_unauthorized_read_and_corrupt_objects() {
    let (db, token, _, workspace, input) = setup().await;
    assert!(matches!(
        db.store
            .commit_workspace_artifact(
                &token,
                &key("bad"),
                &workspace,
                &CommitArtifact {
                    base_revision: 2,
                    ..input.clone()
                }
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    let first = admit(&db, &token, &workspace, &input).await;
    let outsider =
        super::runtime::runtime_token(&db, "elsewhere", "alice", &ServiceScope::ALL).await;
    assert!(matches!(
        db.store
            .workspace_artifact(&outsider, &first.commit_id)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let objects = Objects::new();
    let (_, bundle) = capture(&db, &first.commit_id, &objects).await;
    let path = format!("/artifacts/{}", bundle.chunks["saved.txt"][0].key);
    objects
        .bytes
        .lock()
        .unwrap()
        .insert(path, b"corrupt-file!".to_vec());
    assert!(verify(&objects.client, &bundle, None).await.is_err());
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
}
