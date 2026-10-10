use super::*;
use agent_computer_store::runtime::artifacts::*;
async fn input(db: &Database, token: &str, computer: &str) -> (String, CommitArtifact) {
    let current = db.store.computer_runtime(token, computer).await.unwrap();
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    allow(
        db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Publish,
        None,
    )
    .await;
    let digest: String = sqlx::query_scalar("SELECT digest FROM workspace_input_versions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    (
        workspace,
        CommitArtifact {
            request_id: current.active_request.unwrap(),
            expected_revision: current.revision,
            base_revision: 1,
            base_manifest: digest,
            publish_current: true,
        },
    )
}
#[tokio::test]
async fn sealing_serializes_with_writer_acquisition_and_never_reopens_released_epochs() {
    let (db, token, computer, acquire_input) = setup().await;
    let (workspace, input) = input(&db, &token, &computer).await;
    let seal_key = key("seal");
    let acquire_key = key("writer");
    let (seal, writer) = tokio::join!(
        db.store
            .commit_workspace_artifact(&token, &seal_key, &workspace, &input),
        db.store
            .acquire_candidate_writer(&token, &acquire_key, &computer, &acquire_input)
    );
    assert_ne!(seal.is_ok(), writer.is_ok());
    if let Ok(lease) = writer {
        assert!(matches!(seal, Err(Error::WriterLeaseBusy)));
        db.store
            .close_connection_session(&token, &acquire_input.connection_session_id)
            .await
            .unwrap();
        assert_eq!(
            db.store
                .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
                .await
                .unwrap()
                .state,
            WriterLeaseState::Released
        );
        db.store
            .commit_workspace_artifact(&token, &seal_key, &workspace, &input)
            .await
            .unwrap();
        assert!(sqlx::query("UPDATE candidate_writer_leases SET state='Held',epoch=epoch+1,revision=revision+1,expires_at_ms=floor(extract(epoch from clock_timestamp())*1000)+30000").execute(&db.pool).await.is_err());
    }
    assert!(
        db.store
            .acquire_candidate_writer(&token, &key("late"), &computer, &acquire_input)
            .await
            .is_err()
    );
    assert_eq!(count(&db, "artifact_commits").await, 1);
}
#[tokio::test]
async fn unknown_dispatched_writer_blocks_artifact_even_after_connection_closes() {
    let (db, token, computer, acquire_input) = setup().await;
    let (workspace, input) = input(&db, &token, &computer).await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let _permit = dispatch(&db, &token, &lease).await;
    db.store
        .close_connection_session(&token, &acquire_input.connection_session_id)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    assert!(matches!(
        db.store
            .commit_workspace_artifact(&token, &key("seal"), &workspace, &input)
            .await,
        Err(Error::WriterLeaseBusy)
    ));
    assert_eq!(count(&db, "artifact_commits").await, 0);
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Prepared)
    );
}
