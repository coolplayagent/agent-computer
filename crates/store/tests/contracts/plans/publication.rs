use super::*;
use agent_computer_store::Error;

#[tokio::test]
async fn plan_is_side_effect_free_for_resources_and_apply_is_atomic_and_retryable() {
    let (db, token, document) = fixture().await;
    let plan = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    assert_eq!(plan.resources.len(), 7);
    assert!(!plan.deletes_data);
    for resource in &plan.resources {
        assert_eq!(resource.change, Change::Create);
        assert_eq!(resource.revision, 1);
    }
    for table in [
        "resource_definitions",
        "resource_spec_versions",
        "operations",
        "reconcile_intents",
        "declaration_versions",
    ] {
        assert_eq!(count(&db, table).await, 0);
    }
    let retry = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    assert_eq!(retry.plan_digest, plan.plan_digest);
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("wrong"), &plan.plan_id, "sha256:wrong")
            .await,
        Err(Error::PlanDigestMismatch)
    ));
    let operation = db
        .store
        .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
        .await
        .unwrap();
    assert_eq!(operation.state, "Queued");
    for table in [
        "resource_definitions",
        "resource_spec_versions",
        "reconcile_intents",
    ] {
        assert_eq!(count(&db, table).await, 7);
    }
    assert_eq!(count(&db, "declaration_versions").await, 1);
    for request_key in ["apply", "another-key"] {
        let retry = db
            .store
            .apply_definition_plan(&token, &key(request_key), &plan.plan_id, &plan.plan_digest)
            .await
            .unwrap();
        assert_eq!(retry.operation_id, operation.operation_id);
        assert_eq!(retry.event_sequence, operation.event_sequence);
    }
    assert_eq!(count(&db, "operations").await, 1);
    sqlx::query("UPDATE operations SET state='Blocked' WHERE operation_id=$1")
        .bind(&operation.operation_id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await
            .unwrap()
            .state,
        "Blocked"
    );
    assert_eq!(
        db.store
            .definition_operation(&token, &operation.operation_id)
            .await
            .unwrap()
            .state,
        "Blocked"
    );
    assert!(
        sqlx::query("UPDATE resource_spec_versions SET spec='{}'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM definition_plans")
            .execute(&db.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn updates_pin_dependencies_and_keep_old_versions_and_unchanged_digests() {
    let (db, token, mut document) = fixture().await;
    let first = db
        .store
        .create_definition_plan(&token, &key("p1"), &checked(&document))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(&token, &key("a1"), &first.plan_id, &first.plan_digest)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("missing-revision"), &checked(&document))
            .await,
        Err(Error::PreconditionRequired)
    ));
    revisions(&mut document, &first, 1);
    let unchanged = db
        .store
        .create_definition_plan(&token, &key("same"), &checked(&document))
        .await
        .unwrap();
    assert!(
        unchanged
            .resources
            .iter()
            .all(|r| r.change == Change::Unchanged)
    );
    let sandbox = document["spec"]["sandboxes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["name"] == "exec-env")
        .unwrap();
    sandbox["image"] = format!("registry.example.invalid/tools@sha256:{}", "c".repeat(64)).into();
    let update = db
        .store
        .create_definition_plan(&token, &key("changed"), &checked(&document))
        .await
        .unwrap();
    let changed: Vec<_> = update
        .resources
        .iter()
        .filter(|r| r.change == Change::Update)
        .collect();
    assert_eq!(changed.len(), 2);
    assert!(
        changed
            .iter()
            .any(|r| r.name == "exec-env" && r.requires_drain)
    );
    let computer = changed
        .iter()
        .find(|r| r.kind == DefinitionKind::Computer)
        .unwrap();
    assert!(
        computer
            .dependencies
            .iter()
            .any(|d| d.name == "exec-env" && d.revision == 2)
    );
    db.store
        .apply_definition_plan(&token, &key("a2"), &update.plan_id, &update.plan_digest)
        .await
        .unwrap();
    assert_eq!(count(&db, "resource_spec_versions").await, 9);
    assert!(matches!(
        db.store
            .apply_definition_plan(
                &token,
                &key("stale"),
                &unchanged.plan_id,
                &unchanged.plan_digest
            )
            .await,
        Err(Error::RevisionConflict)
    ));
    revisions(&mut document, &update, 2);
    document["spec"]["volumes"][0]["quotaBytes"] = 1.into();
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("shrink"), &checked(&document))
            .await,
        Err(Error::UnsupportedChange)
    ));
}

#[tokio::test]
async fn final_write_failure_rolls_back_definitions_grants_intents_and_events() {
    let (db, token, document) = fixture().await;
    let plan = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    let events = count(&db, "events").await;
    let grants = count(&db, "definition_grants").await;
    sqlx::raw_sql("CREATE FUNCTION fail_apply_receipt() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN IF NEW.operation='definitions.apply.v1' THEN RAISE EXCEPTION 'injected failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER fail_apply BEFORE INSERT ON request_records FOR EACH ROW EXECUTE FUNCTION fail_apply_receipt();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await,
        Err(Error::Database(_))
    ));
    for table in [
        "resource_definitions",
        "resource_spec_versions",
        "reconcile_intents",
        "operations",
        "declaration_heads",
        "declaration_versions",
    ] {
        assert_eq!(count(&db, table).await, 0, "{table}");
    }
    assert_eq!(count(&db, "events").await, events);
    assert_eq!(count(&db, "definition_grants").await, grants);
    sqlx::query("DROP TRIGGER fail_apply ON request_records")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store
        .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_apply_retries_and_competing_plans_publish_only_one_version() {
    let (db, token, document) = fixture().await;
    let plan = db
        .store
        .create_definition_plan(&token, &key("p1"), &checked(&document))
        .await
        .unwrap();
    let mut competitor = document.clone();
    competitor["metadata"]["name"] = "another-set".into();
    let other = db
        .store
        .create_definition_plan(&token, &key("p2"), &checked(&competitor))
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = db.store.clone();
        let token = token.clone();
        let plan = plan.clone();
        tasks.spawn(async move {
            store
                .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
                .await
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    while let Some(result) = tasks.join_next().await {
        ids.insert(result.unwrap().unwrap().operation_id);
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(count(&db, "operations").await, 1);
    assert_eq!(count(&db, "resource_spec_versions").await, 7);
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("other"), &other.plan_id, &other.plan_digest)
            .await,
        Err(Error::RevisionConflict)
    ));
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &other.plan_id, &other.plan_digest)
            .await,
        Err(Error::IdempotencyConflict)
    ));
}
