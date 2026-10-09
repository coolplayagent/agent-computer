use super::*;

#[tokio::test]
async fn pinned_catalog_drift_and_connect_revocation_block_dispatched_ownership() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let permit = dispatch(&db, &token, &lease).await;
    // Fault injection models a pinned catalog becoming unavailable.
    sqlx::query("UPDATE catalog_references SET enabled=false WHERE kind='storage_class'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    assert!(matches!(
        db.store
            .renew_candidate_writer(
                &token,
                &key("catalog-disabled"),
                &lease.lease_id,
                &RenewWriterLease {
                    lease: command(permit.lease()),
                    duration_seconds: 30
                }
            )
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    sqlx::query("UPDATE catalog_references SET enabled=true WHERE kind='storage_class'")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Computer,
                resource_id: &computer,
                permission: RuntimePermission::Connect,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    allow(
        &db,
        &computer,
        RuntimeKind::Computer,
        RuntimePermission::Connect,
        None,
    )
    .await;
    assert_eq!(
        db.store
            .connection_session(&token, &input.connection_session_id)
            .await
            .unwrap()
            .state,
        ConnectionState::Revoked
    );
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    let replacement = connection(&db, &token, &computer, "replacement", 900).await;
    assert!(matches!(
        db.store
            .acquire_candidate_writer(
                &token,
                &key("replacement"),
                &computer,
                &AcquireWriterLease {
                    connection_session_id: replacement.session_id,
                    ..input
                }
            )
            .await,
        Err(Error::WriterLeaseBusy)
    ));
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn permissions_are_independent_and_other_credentials_cannot_steal_an_owner() {
    let (db, token, computer, input) = setup().await;
    for (name, scope) in [
        ("missing-modify", ServiceScope::RuntimeRead),
        ("missing-read", ServiceScope::RuntimeModify),
    ] {
        let limited =
            runtime_token(&db, "acme", "alice", &[ServiceScope::RuntimeConnect, scope]).await;
        let session = connection(&db, &limited, &computer, name, 900).await;
        assert!(matches!(
            db.store
                .acquire_candidate_writer(
                    &limited,
                    &key(name),
                    &computer,
                    &AcquireWriterLease {
                        connection_session_id: session.session_id,
                        ..input.clone()
                    }
                )
                .await,
            Err(Error::Forbidden)
        ));
    }
    let other = runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    assert!(matches!(
        db.store
            .acquire_candidate_writer(&other, &key("steal"), &computer, &input)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    for (kind, id, permission) in [
        (RuntimeKind::Computer, &computer, RuntimePermission::Read),
        (RuntimeKind::Computer, &computer, RuntimePermission::Modify),
        (RuntimeKind::Workspace, &workspace, RuntimePermission::Read),
        (
            RuntimeKind::Workspace,
            &workspace,
            RuntimePermission::Modify,
        ),
    ] {
        db.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org("acme"),
                    principal: &principal("alice"),
                    kind,
                    resource_id: id,
                    permission,
                    max_runtime_seconds: None,
                },
                false,
            )
            .await
            .unwrap();
        assert!(matches!(
            db.store
                .acquire_candidate_writer(&token, &key("missing"), &computer, &input)
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
        allow(&db, id, kind, permission, None).await;
    }
    let limited = db
        .store
        .create_connection_session(
            &token,
            &key("limited"),
            &computer,
            &ConnectRequest {
                requested_capabilities: vec![RuntimePermission::Connect, RuntimePermission::Read],
                lifetime_seconds: 900,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .acquire_candidate_writer(
                &token,
                &key("limited"),
                &computer,
                &AcquireWriterLease {
                    connection_session_id: limited.session_id,
                    ..input.clone()
                }
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    for changed in [
        AcquireWriterLease {
            generation: 2,
            ..input.clone()
        },
        AcquireWriterLease {
            candidate_id: "wrong-candidate".into(),
            ..input.clone()
        },
    ] {
        assert!(matches!(
            db.store
                .acquire_candidate_writer(&token, &key("wrong"), &computer, &changed)
                .await,
            Err(Error::RuntimeConflict)
        ));
    }
    let lease = acquire(&db, &token, &computer, &input).await;
    assert!(matches!(
        db.store.candidate_writer(&other, &lease.lease_id).await,
        Err(Error::RuntimeAccessUnavailable)
    ));
}

#[tokio::test]
async fn revocation_and_close_persist_draining_and_regrant_cannot_revive_it() {
    let (db, token, computer, input) = setup().await;
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    for (index, (kind, id, permission)) in [
        (RuntimeKind::Computer, &computer, RuntimePermission::Modify),
        (RuntimeKind::Workspace, &workspace, RuntimePermission::Read),
    ]
    .into_iter()
    .enumerate()
    {
        let lease = db
            .store
            .acquire_candidate_writer(&token, &key(&format!("acquire-{index}")), &computer, &input)
            .await
            .unwrap();
        db.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org("acme"),
                    principal: &principal("alice"),
                    kind,
                    resource_id: id,
                    permission,
                    max_runtime_seconds: None,
                },
                false,
            )
            .await
            .unwrap();
        allow(&db, id, kind, permission, None).await;
        assert_eq!(
            db.store
                .candidate_writer(&token, &lease.lease_id)
                .await
                .unwrap()
                .state,
            WriterLeaseState::Draining
        );
        assert!(matches!(
            db.store
                .renew_candidate_writer(
                    &token,
                    &key("no-revival"),
                    &lease.lease_id,
                    &RenewWriterLease {
                        lease: command(&lease),
                        duration_seconds: 30
                    }
                )
                .await,
            Err(Error::WriterLeaseInactive)
        ));
        assert_eq!(
            db.store
                .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
                .await
                .unwrap()
                .state,
            WriterLeaseState::Released
        );
    }
    let lease = acquire(&db, &token, &computer, &input).await;
    db.store
        .close_connection_session(&token, &input.connection_session_id)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    let event: Value = sqlx::query_scalar(
        "SELECT payload FROM events WHERE kind='connection.closed' ORDER BY sequence DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(event["draining_writer_count"], 1);
    assert_eq!(event["process_termination_confirmed"], false);
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Released
    );
}

#[tokio::test]
async fn collaborator_uses_own_authority_after_starter_credential_revocation() {
    let (db, token, computer, mut input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let original: String = sqlx::query_scalar("SELECT credential_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    db.store
        .revoke_credential(&org("acme"), &original)
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
    let collaborator = runtime_token(&db, "acme", "bob", &ServiceScope::ALL).await;
    let workspace: String = sqlx::query_scalar("SELECT workspace_id FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    for (kind, id, permission) in [
        (RuntimeKind::Computer, &computer, RuntimePermission::Connect),
        (RuntimeKind::Computer, &computer, RuntimePermission::Read),
        (RuntimeKind::Computer, &computer, RuntimePermission::Modify),
        (RuntimeKind::Workspace, &workspace, RuntimePermission::Read),
        (
            RuntimeKind::Workspace,
            &workspace,
            RuntimePermission::Modify,
        ),
    ] {
        db.store
            .set_runtime_grant(
                RuntimeGrant {
                    organization: &org("acme"),
                    principal: &principal("bob"),
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
    input.connection_session_id = connection(&db, &collaborator, &computer, "bob-connect", 900)
        .await
        .session_id;
    let next = db
        .store
        .acquire_candidate_writer(&collaborator, &key("bob-acquire"), &computer, &input)
        .await
        .unwrap();
    assert_eq!(next.epoch, 2);
    db.store
        .disable_principal(&org("acme"), &principal("bob"))
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
}
