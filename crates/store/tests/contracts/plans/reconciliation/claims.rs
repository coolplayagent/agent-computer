use super::*;

#[tokio::test]
async fn claims_are_exclusive_ordered_and_complete_only_from_bound_receipts() {
    let (db, token, document) = fixture().await;
    let (plan, operation) = publish(&db, &token, "initial", &document).await;
    assert_eq!(operation.progress.len(), 7);
    assert!(
        operation
            .progress
            .iter()
            .all(|p| p.state == IntentState::Pending)
    );
    let mut handles = vec![];
    for n in 0..8 {
        let store = db.store.clone();
        handles.push(tokio::spawn(async move {
            store
                .claim_reconciliation(
                    &org("acme"),
                    &WorkerId::new(format!("worker-{n}")).unwrap(),
                    Duration::from_secs(30),
                )
                .await
                .unwrap()
        }));
    }
    let mut leases = vec![];
    for handle in handles {
        if let ClaimOutcome::Claimed(lease) = handle.await.unwrap() {
            leases.push(*lease)
        }
    }
    assert_eq!(leases.len(), 1);
    let first = leases.pop().unwrap();
    assert_eq!(first.task().resource_id, plan.resources[0].resource_id);
    assert_eq!(first.epoch(), 1);
    assert!(matches!(
        db.store
            .finish_reconciliation(
                &first,
                ReconcileOutcome::Applied {
                    receipt: receipt(&first)
                }
            )
            .await,
        Err(Error::InvalidReconcileResult)
    ));
    assert!(
        db.store
            .renew_reconciliation(&first, Duration::from_secs(60))
            .await
            .unwrap()
            > first.expires_at_ms()
    );
    succeed(&db, &first).await;
    for resource in &plan.resources[1..] {
        let lease = claim(&db, "continuation", 30).await;
        assert_eq!(lease.task().resource_id, resource.resource_id);
        assert_eq!(lease.task().dependencies, resource.dependencies);
        idle(&db).await;
        succeed(&db, &lease).await;
    }
    let done = db
        .store
        .definition_operation(&token, &operation.operation_id)
        .await
        .unwrap();
    assert_eq!(done.state, "Succeeded");
    assert!(
        done.progress
            .iter()
            .all(|p| p.state == IntentState::Succeeded)
    );
    let retry = db
        .store
        .apply_definition_plan(
            &token,
            &key("initial-apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    assert_eq!(retry.progress, done.progress);
    assert_eq!(retry.state, "Succeeded");
    idle(&db).await;
    assert!(
        sqlx::query("UPDATE reconciliation_results SET response='{}'")
            .execute(&db.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn blocked_shared_resources_exclude_newer_operations_but_not_independent_work() {
    let (db, token, _) = fixture().await;
    let mut doc = agent_document("first", "external");
    let (plan, first) = publish(&db, &token, "one", &doc).await;
    let lease = claim(&db, "one", 30).await;
    db.store
        .finish_reconciliation(
            &lease,
            ReconcileOutcome::Blocked {
                reason: ReconcileReason::BackendUnavailable,
            },
        )
        .await
        .unwrap();
    doc["metadata"]["expectedRevision"] = 1.into();
    doc["spec"]["agents"][0]["expectedRevision"] = 1.into();
    doc["spec"]["agents"][0]["capabilities"] = serde_json::json!(["files.read"]);
    let (_, second) = publish(&db, &token, "two", &doc).await;
    idle(&db).await;
    let (_, independent) = publish(&db, &token, "three", &agent_document("other", "other")).await;
    let other = claim(&db, "other", 30).await;
    assert_eq!(other.task().operation_id, independent.operation_id);
    succeed(&db, &other).await;
    assert!(matches!(
        db.store
            .resume_reconciliation(&org("another"), &first.operation_id)
            .await,
        Err(Error::OperationNotBlocked)
    ));
    db.store
        .resume_reconciliation(&org("acme"), &first.operation_id)
        .await
        .unwrap();
    let resumed = claim(&db, "resume", 30).await;
    assert_eq!(resumed.task().operation_id, first.operation_id);
    assert_eq!(resumed.task().spec_digest, plan.resources[0].digest);
    db.store
        .finish_reconciliation(
            &resumed,
            ReconcileOutcome::Failed {
                reason: ReconcileReason::BackendRejected,
            },
        )
        .await
        .unwrap();
    let newer = claim(&db, "newer", 30).await;
    assert_eq!(newer.task().operation_id, second.operation_id);
    assert_eq!(newer.task().revision, 2);
}
