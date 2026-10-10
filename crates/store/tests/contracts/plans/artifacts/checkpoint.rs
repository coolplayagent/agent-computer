use super::*;
use serde_json::json;

pub(super) fn request(input: &CommitArtifact) -> CheckpointStop {
    CheckpointStop {
        request_id: input.request_id.clone(),
        expected_revision: input.expected_revision,
        publish_current: input.publish_current,
    }
}
pub(super) async fn start(
    db: &Database,
    token: &str,
    computer: &str,
    input: &CommitArtifact,
) -> ArtifactCommit {
    allow(
        db,
        computer,
        RuntimeKind::Computer,
        RuntimePermission::Manage,
        None,
    )
    .await;
    db.store
        .checkpoint_stop_computer(token, &key("checkpoint-stop"), computer, &request(input))
        .await
        .unwrap()
}

#[tokio::test]
async fn checkpoint_stop_publication_is_atomic_and_survives_wal_and_new_generation() {
    let (mut db, token, computer, _, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    assert!(first.stop_after_commit && first.stop_receipt.is_none());
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Sealing)
    );
    assert_eq!(
        db.store
            .checkpoint_stop_computer(&token, &key("checkpoint-stop"), &computer, &request(&input))
            .await
            .unwrap(),
        first
    );
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    let done = db.store.finish_artifact(&lease, &verified).await.unwrap();
    let stopped = done.stop_receipt.as_ref().unwrap();
    assert_eq!(
        stopped.checkpoint.as_ref().unwrap().artifact_id,
        first.commit_id
    );
    assert_eq!(stopped.control_revision, input.expected_revision + 3);
    let stopped_event: i64 =
        sqlx::query_scalar("SELECT sequence FROM events WHERE kind='computer.stopped'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let published_event: i64 =
        sqlx::query_scalar("SELECT sequence FROM events WHERE kind='artifact.committed'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(stopped.event_sequence, stopped_event);
    assert_eq!(stopped_event, published_event + 1);
    db.crash_and_restart().await;
    assert_eq!(
        db.store.finish_artifact(&lease, &verified).await.unwrap(),
        done
    );
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert!(current.active_request.is_none() && !current.ready);
    assert_eq!(current.stop_receipt.as_ref(), Some(stopped));
    let next = db
        .store
        .admit_computer_start(
            &token,
            &key("next"),
            &computer,
            &StartRequest {
                expected_revision: current.revision,
                expected_spec_revision: 1,
                max_runtime_seconds: 300,
                input_artifact_id: Some(first.commit_id.clone()),
            },
        )
        .await
        .unwrap();
    assert_ne!(next.candidate_id, done.candidate_id);
    assert_eq!(next.input_artifact_id.as_ref(), Some(&first.commit_id));
    assert_eq!(
        db.store
            .checkpoint_stop_computer(&token, &key("checkpoint-stop"), &computer, &request(&input))
            .await
            .unwrap(),
        done
    );
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .active_request,
        Some(next.request_id)
    );
}

#[tokio::test]
async fn checkpoint_stop_event_failure_rolls_back_publication_stop_and_head() {
    let (db, token, computer, workspace, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_checkpoint_stop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM events WHERE organization=NEW.organization AND sequence=NEW.sequence AND kind='computer.stopped') THEN RAISE EXCEPTION 'injected stop outbox failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_checkpoint_stop BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_checkpoint_stop();").execute(&db.pool).await.unwrap();
    assert!(db.store.finish_artifact(&lease, &verified).await.is_err());
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "runtime_stops").await, 0);
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM workspace_input_heads WHERE workspace_id=$1"
        )
        .bind(&workspace)
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        input.base_revision
    );
    sqlx::query("DROP TRIGGER reject_checkpoint_stop ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.store
            .finish_artifact(&lease, &verified)
            .await
            .unwrap()
            .stop_receipt
            .is_some()
    );
}

#[tokio::test]
async fn checkpoint_stop_database_constraint_rejects_publication_without_stop() {
    let (db, token, computer, _, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (_, bundle) = capture(&db, &first.commit_id, &objects).await;
    let object = bundle.object(&objects.client).unwrap();
    let error=sqlx::query("UPDATE artifact_commits SET state='Committed',object_ref=$1,input_revision=1,published_at_ms=1 WHERE commit_id=$2").bind(serde_json::to_value(object).unwrap()).bind(&first.commit_id).execute(&db.pool).await.unwrap_err();
    assert!(
        error
            .as_database_error()
            .unwrap()
            .message()
            .contains("checkpoint publication and stop")
    );
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
    assert!(
        sqlx::query("UPDATE artifact_commits SET stop_after_commit=false")
            .execute(&db.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn checkpoint_stop_branch_preserves_workspace_head_and_fixed_checkpoint() {
    let (db, token, computer, workspace, mut input) = setup().await;
    input.publish_current = false;
    let first = start(&db, &token, &computer, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &first.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    let done = db.store.finish_artifact(&lease, &verified).await.unwrap();
    assert_eq!(done.state, ArtifactState::Committed);
    assert_eq!(
        done.stop_receipt.unwrap().input_revision,
        done.input_revision.unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM workspace_input_heads WHERE workspace_id=$1"
        )
        .bind(&workspace)
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        input.base_revision
    );
}

#[tokio::test]
async fn artifact_queue_is_read_only_exact_target_and_excludes_active_claims() {
    let (db, token, computer, _, input) = setup().await;
    let first = start(&db, &token, &computer, &input).await;
    let target: agent_computer_store::runtime::preparation::PreparationTarget =
        serde_json::from_value(
            sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT binding FROM candidate_preparations",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        )
        .unwrap();
    let before = count(&db, "events").await;
    assert_eq!(
        db.store
            .artifact_queue(&org("acme"), &target)
            .await
            .unwrap(),
        std::slice::from_ref(&first.commit_id)
    );
    assert_eq!(count(&db, "events").await, before);
    assert!(
        db.store
            .artifact_queue(&org("foreign"), &target)
            .await
            .unwrap()
            .is_empty()
    );
    for field in [
        "volume_id",
        "namespace_uid",
        "pvc_uid",
        "pv_uid",
        "filesystem_uuid",
        "volume_path",
        "writer_uid",
        "writer_gid",
    ] {
        let mut wrong = serde_json::to_value(&target).unwrap();
        wrong[field] = if field.starts_with("writer_") {
            json!(12345)
        } else {
            json!("other")
        };
        assert!(
            db.store
                .artifact_queue(&org("acme"), &serde_json::from_value(wrong).unwrap())
                .await
                .unwrap()
                .is_empty(),
            "{field}"
        );
    }
    let a = WorkerId::new("a").unwrap();
    let b = WorkerId::new("b").unwrap();
    let organization = org("acme");
    let (a, b) = tokio::join!(
        db.store.claim_artifact(&organization, &first.commit_id, &a),
        db.store.claim_artifact(&organization, &first.commit_id, &b)
    );
    assert_ne!(a.unwrap().is_some(), b.unwrap().is_some());
    assert!(
        db.store
            .artifact_queue(&organization, &target)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn migration_twenty_six_preserves_ordinary_artifact_work() {
    let (db, token, _, workspace, input) = setup().await;
    let first = admit(&db, &token, &workspace, &input).await;
    db.remove_checkpoint_stop_worker().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(
        db.store
            .workspace_artifact(&token, &first.commit_id)
            .await
            .unwrap(),
        first
    );
    assert!(!first.stop_after_commit && first.stop_receipt.is_none());
}
