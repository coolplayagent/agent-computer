use super::*;

#[tokio::test]
async fn committed_files_cannot_silently_omit_required_app_checkpoint_state() {
    let (db, token, computer, workspace, input) = setup_with_apps(true).await;
    let admitted = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &admitted.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    db.store.finish_artifact(&lease, &verified).await.unwrap();
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert!(matches!(
        db.store
            .stop_prepared_computer(
                &token,
                &key("stop"),
                &computer,
                &StopPreparedComputer {
                    expected_revision: current.revision,
                    request_id: input.request_id
                }
            )
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    assert_eq!(count(&db, "runtime_stops").await, 0);
}

#[tokio::test]
async fn migration_twenty_one_preserves_existing_stop_receipts() {
    let (db, token, computer, _, input) = setup().await;
    let stopped = db
        .store
        .stop_prepared_computer(
            &token,
            &key("old-stop"),
            &computer,
            &StopPreparedComputer {
                expected_revision: input.expected_revision,
                request_id: input.request_id,
            },
        )
        .await
        .unwrap();
    db.remove_workspace_artifacts().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .stop_receipt,
        Some(stopped)
    );
    assert_eq!(count(&db, "artifact_commits").await, 0);
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
}

#[tokio::test]
async fn artifact_lease_expiring_during_outbox_cannot_publish() {
    let (db, token, _, workspace, input) = setup().await;
    let admitted = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &admitted.commit_id, &objects).await;
    let verified = verify(&objects.client, &bundle, None).await.unwrap();
    sqlx::query("UPDATE artifact_commits SET lease_until_ms=floor(extract(epoch from clock_timestamp())*1000)+1000").execute(&db.pool).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION delay_artifact() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(1.2); RETURN NEW; END $$; CREATE TRIGGER delay_artifact BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION delay_artifact();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store.finish_artifact(&lease, &verified).await,
        Err(Error::StaleReconcileLease)
    ));
    assert_eq!(count(&db, "workspace_input_versions").await, 1);
    assert_eq!(
        db.store
            .workspace_artifact(&token, &admitted.commit_id)
            .await
            .unwrap()
            .state,
        ArtifactState::Capturing
    );
    sqlx::query("DROP TRIGGER delay_artifact ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    let next = db
        .store
        .claim_artifact(
            &org("acme"),
            &admitted.commit_id,
            &WorkerId::new("restart").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    // An old worker's cleanup must not release a newer owner's lease.
    db.store.release_artifact_worker(&lease).await.unwrap();
    db.store.finish_artifact(&next, &verified).await.unwrap();
}

#[tokio::test]
async fn artifact_manifest_is_workspace_authorized_and_full_file_hash_is_verified() {
    let (db, token, _, workspace, input) = setup().await;
    let admitted = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, mut bundle) = capture(&db, &admitted.commit_id, &objects).await;
    assert!(
        db.store
            .artifact_manifest(&token, &admitted.commit_id)
            .await
            .unwrap()
            .is_none()
    );
    let original = bundle.clone();
    let Entry::File { sha256, .. } = bundle.manifest.entries.get_mut("saved.txt").unwrap() else {
        panic!()
    };
    *sha256 = agent_computer_objects::sha256(b"different-concatenation");
    assert!(verify(&objects.client, &bundle, None).await.is_err());
    for mutate in [0, 1, 2, 3, 4] {
        let mut bad = original.clone();
        match mutate {
            0 => bad.organization = "elsewhere".into(),
            1 => bad.chunks.get_mut("saved.txt").unwrap()[0].size += 1,
            2 => {
                bad.chunks.get_mut("saved.txt").unwrap()[0].store_digest =
                    agent_computer_objects::sha256(b"other-store")
            }
            3 => {
                bad.manifest
                    .entries
                    .insert("../escape".into(), Entry::Directory);
            }
            _ => {
                bad.chunks.insert("directory".into(), vec![]);
            }
        }
        assert!(bad.validate().is_err());
    }
    let verified = verify(&objects.client, &original, None).await.unwrap();
    db.store.finish_artifact(&lease, &verified).await.unwrap();
    assert_eq!(
        db.store
            .artifact_manifest(&token, &admitted.commit_id)
            .await
            .unwrap(),
        Some(original.manifest)
    );
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::Workspace,
                resource_id: &workspace,
                permission: RuntimePermission::Read,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .artifact_manifest(&token, &admitted.commit_id)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
}
