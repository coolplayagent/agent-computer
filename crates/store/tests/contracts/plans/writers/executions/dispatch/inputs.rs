use super::*;

#[tokio::test]
async fn runtime_inputs_bind_original_preparation_manifest_and_successful_volume_effect() {
    let (mut db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    assert!(matches!(
        db.store
            .candidate_execution_runtime_inputs(&org("acme"), &queued.execution_id)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let attempt = begin(&db, &queued).await;
    let loaded = db
        .store
        .candidate_execution_runtime_inputs(&org("acme"), &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(
        loaded.dispatch.intent_digest,
        attempt.intent().intent_digest
    );
    assert_eq!(loaded.preparation.computer, computer);
    assert_eq!(loaded.preparation.candidate, queued.candidate_id);
    assert_eq!(loaded.preparation.path_ref(), loaded.prepared.path_ref);
    assert_eq!(loaded.preparation.volume_uid, loaded.pvc.uid);
    assert_eq!(loaded.target.pv_uid, loaded.pv.uid);
    assert_eq!(loaded.target.volume_id, loaded.volume.resource_id);
    assert_eq!(loaded.volume.revision, 1);
    assert_eq!(loaded.volume.kind, DefinitionKind::Volume);
    let before = count(&db, "events").await;
    db.crash_and_restart().await;
    assert_eq!(
        serde_json::to_value(
            db.store
                .candidate_execution_runtime_inputs(&org("acme"), &queued.execution_id)
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(loaded).unwrap()
    );
    assert_eq!(count(&db, "events").await, before);
    assert!(matches!(
        db.store
            .candidate_execution_runtime_inputs(&org("other"), &queued.execution_id)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(count(&db, "execution_pod_plans").await, 0);
}

#[tokio::test]
async fn runtime_recovery_inputs_survive_revocation_without_restoring_authority() {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    begin(&db, &queued).await;
    let before = db
        .store
        .candidate_execution_runtime_inputs(&org("acme"), &queued.execution_id)
        .await
        .unwrap();
    sqlx::raw_sql(
        "UPDATE service_credentials SET revoked=true; UPDATE catalog_references SET enabled=false;",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    db.store
        .reconcile_candidate_execution(&org("acme"), &queued.execution_id)
        .await
        .unwrap();
    let after = db
        .store
        .candidate_execution_runtime_inputs(&org("acme"), &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(before.preparation).unwrap(),
        serde_json::to_value(after.preparation).unwrap()
    );
    assert_eq!(before.volume.spec, after.volume.spec);
    assert_eq!(after.dispatch.execution.state, ExecutionState::Unknown);
    assert_eq!(before.dispatch.intent_digest, after.dispatch.intent_digest);
    assert_eq!(count(&db, "execution_pod_plans").await, 0);
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
}

#[tokio::test]
async fn runtime_inputs_reject_missing_or_mismatched_volume_evidence() {
    for fault in [
        "ALTER TABLE reconciliation_objects DISABLE TRIGGER USER; DELETE FROM reconciliation_objects WHERE role='pvc';",
        "ALTER TABLE reconciliation_objects DISABLE TRIGGER USER; UPDATE reconciliation_objects SET binding=jsonb_set(binding,'{uid}','\"replacement\"') WHERE role='pv';",
    ] {
        let (db, token, computer, acquire_input) = setup().await;
        let lease = acquire(&db, &token, &computer, &acquire_input).await;
        let input = submission(&db, &lease).await;
        let queued = submit(&db, &token, &computer, &input).await;
        begin(&db, &queued).await;
        // Deliberate trusted-database corruption, not a supported mutation path.
        sqlx::raw_sql(fault).execute(&db.pool).await.unwrap();
        assert!(
            matches!(
                db.store
                    .candidate_execution_runtime_inputs(&org("acme"), &queued.execution_id)
                    .await,
                Err(Error::ReferenceUnavailable)
            ),
            "{fault}"
        );
    }
}
