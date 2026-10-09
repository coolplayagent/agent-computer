use super::*;
use agent_computer_store::runtime::files::ReadFileRequest;

async fn request(db: &Database, input: &AcquireWriterLease) -> (String, ReadFileRequest) {
    let workspace = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    (
        workspace,
        ReadFileRequest {
            connection_session_id: input.connection_session_id.clone(),
            generation: input.generation,
            candidate_id: input.candidate_id.clone(),
            path: "hello.txt".into(),
        },
    )
}
#[tokio::test]
async fn read_only_candidate_access_requires_no_writer_or_modify_scope() {
    let (db, _, computer, input) = setup().await;
    let (workspace, mut read) = request(&db, &input).await;
    let token = runtime_token(
        &db,
        "acme",
        "alice",
        &[ServiceScope::RuntimeConnect, ServiceScope::RuntimeRead],
    )
    .await;
    let session = connection(&db, &token, &computer, "read-only", 900).await;
    read.connection_session_id = session.session_id;
    let admitted = db
        .store
        .candidate_file_read(&token, &workspace, &read)
        .await
        .unwrap();
    assert_eq!(admitted.prepared.volume_uid, "pvc-uid");
    assert_eq!(admitted.target.pvc_uid, "pvc-uid");
    assert_eq!(count(&db, "candidate_writer_leases").await, 0);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
}
#[tokio::test]
async fn read_checks_independent_grants_and_original_connection_credential() {
    let (db, token, computer, input) = setup().await;
    let (workspace, read) = request(&db, &input).await;
    for (kind, id) in [
        (RuntimeKind::Computer, &computer),
        (RuntimeKind::Workspace, &workspace),
    ] {
        db.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org("acme"),
                    principal: &principal("alice"),
                    kind,
                    resource_id: id,
                    permission: RuntimePermission::Read,
                    max_runtime_seconds: None,
                },
                false,
            )
            .await
            .unwrap();
        assert!(matches!(
            db.store
                .candidate_file_read(&token, &workspace, &read)
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
        allow(&db, id, kind, RuntimePermission::Read, None).await;
    }
    for (organization, actor) in [("acme", "alice"), ("acme", "bob"), ("foreign", "alice")] {
        let other = runtime_token(&db, organization, actor, &ServiceScope::ALL).await;
        assert!(matches!(
            db.store
                .candidate_file_read(&other, &workspace, &read)
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
    let token = runtime_token(&db, "acme", "alice", &[ServiceScope::RuntimeConnect]).await;
    assert!(matches!(
        db.store
            .candidate_file_read(&token, &workspace, &read)
            .await,
        Err(Error::Forbidden)
    ));
}
#[tokio::test]
async fn read_never_accepts_a_changed_candidate_generation_workspace_or_catalog() {
    let (db, token, _, input) = setup().await;
    let (workspace, read) = request(&db, &input).await;
    let baseline = db
        .store
        .candidate_file_read(&token, &workspace, &read)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .candidate_file_read(&token, &workspace, &read)
            .await
            .unwrap(),
        baseline
    );
    for altered in [
        ReadFileRequest {
            generation: read.generation + 1,
            ..read.clone()
        },
        ReadFileRequest {
            candidate_id: "wrong-candidate".into(),
            ..read.clone()
        },
    ] {
        assert!(matches!(
            db.store
                .candidate_file_read(&token, &workspace, &altered)
                .await,
            Err(Error::RuntimeConflict)
        ));
    }
    assert!(matches!(
        db.store
            .candidate_file_read(&token, "wrong-workspace", &read)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    sqlx::query("UPDATE catalog_references SET enabled=false WHERE kind='storage_class'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .candidate_file_read(&token, &workspace, &read)
            .await,
        Err(Error::ReferenceUnavailable)
    ));
}
#[tokio::test]
async fn rechecking_read_after_revocation_or_close_rejects_disclosure() {
    let (db, token, computer, input) = setup().await;
    let (workspace, mut read) = request(&db, &input).await;
    db.store
        .candidate_file_read(&token, &workspace, &read)
        .await
        .unwrap();
    db.store
        .close_connection_session(&token, &read.connection_session_id)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .candidate_file_read(&token, &workspace, &read)
            .await,
        Err(Error::ConnectionInactive)
    ));
    read.connection_session_id = connection(&db, &token, &computer, "short-read", 1)
        .await
        .session_id;
    db.store
        .candidate_file_read(&token, &workspace, &read)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(matches!(
        db.store
            .candidate_file_read(&token, &workspace, &read)
            .await,
        Err(Error::ConnectionInactive)
    ));
    read.connection_session_id = connection(&db, &token, &computer, "revoke-read", 900)
        .await
        .session_id;
    db.store
        .candidate_file_read(&token, &workspace, &read)
        .await
        .unwrap();
    db.store
        .disable_principal(&org("acme"), &principal("alice"))
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .candidate_file_read(&token, &workspace, &read)
            .await,
        Err(Error::Unauthenticated)
    ));
}
#[tokio::test]
async fn read_path_validation_and_requested_capability_apply_before_storage() {
    let (db, token, computer, input) = setup().await;
    let (workspace, read) = request(&db, &input).await;
    for path in [
        "../secret",
        "/secret",
        "",
        "a//b",
        "a\\b",
        ".agent-computer-write-stage",
        "a/.agent-computer-write-stage",
    ] {
        assert!(matches!(
            db.store
                .candidate_file_read(
                    &token,
                    &workspace,
                    &ReadFileRequest {
                        path: path.into(),
                        ..read.clone()
                    }
                )
                .await,
            Err(Error::InvalidRuntimeRequest)
        ));
    }
    let session = db
        .store
        .create_connection_session(
            &token,
            &key("no-read-requested"),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![RuntimePermission::Connect],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .candidate_file_read(
                &token,
                &workspace,
                &ReadFileRequest {
                    connection_session_id: session.session_id,
                    ..read
                }
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
}
