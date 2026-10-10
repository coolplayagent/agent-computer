use super::*;
use agent_computer_store::runtime::preparation::PreparationTarget;
use serde_json::{Value, json};

fn start(revision: i64, artifact: Option<&str>) -> StartRequest {
    StartRequest {
        expected_revision: revision,
        expected_spec_revision: 1,
        max_runtime_seconds: 300,
        input_artifact_id: artifact.map(str::to_owned),
    }
}

#[tokio::test]
async fn artifact_selector_cannot_cross_workspace_even_with_both_workspace_grants() {
    let (db, token, _, workspace, input) = setup().await;
    let artifact = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &artifact.commit_id, &objects).await;
    db.store
        .finish_artifact(
            &lease,
            &verify(&objects.client, &bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
    let document = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"other-workspace"},"spec":{
        "volumes":[{"name":"data","storageClass":"juicefs-workspace","quotaBytes":10737418240_i64,"reclaimPolicy":"Retain"}],
        "workspaces":[{"name":"other","volumeRef":"data","conflictPolicy":"explicit"}],
        "computers":[{"name":"other","workspaceRef":"other","sandboxRefs":[],"appRefs":[],"desiredState":"Stopped"}]
    }});
    let plan = db
        .store
        .create_definition_plan(&token, &key("other-workspace-plan"), &checked(&document))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(
            &token,
            &key("other-workspace-apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    super::super::starts::grants(&db).await;
    let computer = &plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Computer)
        .unwrap()
        .resource_id;
    let before = count(&db, "events").await;
    for selector in [&artifact.commit_id, "artifact_unknown"] {
        assert!(matches!(
            db.store
                .admit_computer_start(
                    &token,
                    &key("wrong-workspace"),
                    computer,
                    &start(1, Some(selector))
                )
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "runtime_start_requests").await, 1);
    assert_eq!(
        db.store
            .admit_computer_start(&token, &key("wrong-workspace"), computer, &start(1, None))
            .await
            .unwrap()
            .input_revision,
        Some(1)
    );
}

#[tokio::test]
async fn migration_twenty_two_preserves_history_and_releases_branch_checkpoint() {
    let (db, token, computer, workspace, mut input) = setup().await;
    db.remove_artifact_continuation().await;
    input.publish_current = false;
    let artifact = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &artifact.commit_id, &objects).await;
    let done = db
        .store
        .finish_artifact(
            &lease,
            &verify(&objects.client, &bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    let command = StopPreparedComputer {
        expected_revision: current.revision,
        request_id: input.request_id.clone(),
    };
    assert!(matches!(
        db.store
            .stop_prepared_computer(&token, &key("migration-stop"), &computer, &command)
            .await,
        Err(Error::RuntimeStopBlocked)
    ));
    let before:Value=sqlx::query_scalar("SELECT jsonb_build_object('start',r.receipt,'input',to_jsonb(i),'artifact',to_jsonb(a)) FROM runtime_start_requests r JOIN runtime_start_inputs i USING(organization,request_id) JOIN artifact_commits a USING(organization,request_id)").fetch_one(&db.pool).await.unwrap();
    assert!(before["start"].get("input_artifact_id").is_none());
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let after:Value=sqlx::query_scalar("SELECT jsonb_build_object('start',r.receipt,'input',to_jsonb(i),'artifact',to_jsonb(a)) FROM runtime_start_requests r JOIN runtime_start_inputs i USING(organization,request_id) JOIN artifact_commits a USING(organization,request_id)").fetch_one(&db.pool).await.unwrap();
    assert_eq!(before, after);
    let stopped = db
        .store
        .stop_prepared_computer(&token, &key("migration-stop"), &computer, &command)
        .await
        .unwrap();
    assert_eq!(stopped.checkpoint.unwrap().artifact_id, done.commit_id);
    let next = db
        .store
        .admit_computer_start(
            &token,
            &key("migration-restart"),
            &computer,
            &start(stopped.control_revision, Some(&done.commit_id)),
        )
        .await
        .unwrap();
    assert_eq!(next.input_revision, Some(2));
    assert_eq!(next.input_artifact_id, Some(done.commit_id));
}

#[tokio::test]
async fn selected_input_guard_and_final_authorization_roll_back_all_admission_effects() {
    let (db, token, computer, workspace, mut input) = setup_many(false, 2).await;
    input.publish_current = false;
    let second = other(&db, &computer).await;
    let artifact = admit(&db, &token, &workspace, &input).await;
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &artifact.commit_id, &objects).await;
    db.store
        .finish_artifact(
            &lease,
            &verify(&objects.client, &bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
    let request = start(1, Some(&artifact.commit_id));
    let tables = [
        "runtime_start_requests",
        "runtime_start_inputs",
        "runtime_controls",
        "events",
        "outbox",
        "request_records",
        "runtime_grants",
    ];
    let mut before = Vec::new();
    for table in tables {
        before.push(count(&db, table).await);
    }
    for sql in [
        "CREATE FUNCTION corrupt_selected_input() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.revision:=1; RETURN NEW; END $$; CREATE TRIGGER aa_corrupt_selected_input BEFORE INSERT ON runtime_start_inputs FOR EACH ROW EXECUTE FUNCTION corrupt_selected_input();",
        "CREATE FUNCTION revoke_selected_input() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN DELETE FROM runtime_grants WHERE kind='workspace' AND permission='read'; RETURN NEW; END $$; CREATE TRIGGER revoke_selected_input BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION revoke_selected_input();",
    ] {
        sqlx::raw_sql(sql).execute(&db.pool).await.unwrap();
        let result = db
            .store
            .admit_computer_start(&token, &key("selected-atomic"), &second, &request)
            .await;
        if sql.contains("corrupt_selected_input") {
            assert!(matches!(result, Err(Error::Database(_))));
            sqlx::query("DROP TRIGGER aa_corrupt_selected_input ON runtime_start_inputs")
                .execute(&db.pool)
                .await
                .unwrap();
        } else {
            assert!(matches!(result, Err(Error::RuntimeAccessUnavailable)));
            sqlx::query("DROP TRIGGER revoke_selected_input ON outbox")
                .execute(&db.pool)
                .await
                .unwrap();
        }
        for (table, expected) in tables.into_iter().zip(&before) {
            assert_eq!(count(&db, table).await, *expected, "{table}");
        }
    }
    assert_eq!(
        db.store
            .admit_computer_start(&token, &key("selected-atomic"), &second, &request)
            .await
            .unwrap()
            .input_revision,
        Some(2)
    );
}
async fn other(db: &Database, computer: &str) -> String {
    sqlx::query_scalar(
        "SELECT resource_id FROM resource_definitions WHERE kind='computer' AND resource_id<>$1",
    )
    .bind(computer)
    .fetch_one(&db.pool)
    .await
    .unwrap()
}
async fn prepare(
    db: &Database,
    token: &str,
    computer: &str,
    receipt: &StartReceipt,
) -> CommitArtifact {
    let target: PreparationTarget = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT binding FROM candidate_preparations ORDER BY request_id LIMIT 1",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
    )
    .unwrap();
    let lease = super::super::preparation::claim(db, receipt, &target, "prepare-other").await;
    db.store.begin_candidate_preparation(&lease).await.unwrap();
    db.store
        .finish_candidate_preparation(&lease, &super::super::preparation::evidence(&lease))
        .await
        .unwrap();
    allow(
        db,
        computer,
        RuntimeKind::Computer,
        RuntimePermission::Modify,
        None,
    )
    .await;
    let current = db.store.computer_runtime(token, computer).await.unwrap();
    CommitArtifact {
        request_id: receipt.request_id.clone(),
        expected_revision: current.revision,
        base_revision: receipt.input_revision.unwrap(),
        base_manifest: receipt.input_manifest_digest.clone().unwrap(),
        publish_current: true,
    }
}
async fn stop(db: &Database, token: &str, computer: &str) -> ComputerStopReceipt {
    let current = db.store.computer_runtime(token, computer).await.unwrap();
    db.store
        .stop_prepared_computer(
            token,
            &key(&format!("stop-{computer}")),
            computer,
            &StopPreparedComputer {
                expected_revision: current.revision,
                request_id: current.active_request.unwrap(),
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn parallel_candidates_conflict_then_restore_distinct_inputs_without_moving_head() {
    let (mut db, token, computer, workspace, input) = setup_many(false, 2).await;
    let second = other(&db, &computer).await;
    let queued = db
        .store
        .admit_computer_start(&token, &key("other-start"), &second, &start(1, None))
        .await
        .unwrap();
    let second_input = prepare(&db, &token, &second, &queued).await;
    let first = admit(&db, &token, &workspace, &input).await;
    let another = db
        .store
        .commit_workspace_artifact(&token, &key("second-artifact"), &workspace, &second_input)
        .await
        .unwrap();
    assert_ne!(first.candidate_id, another.candidate_id);
    let objects = Objects::new();
    let (first_lease, first_bundle) =
        capture_bytes(&db, &first.commit_id, &objects, b"first-author").await;
    let (second_lease, second_bundle) =
        capture_bytes(&db, &another.commit_id, &objects, b"second-author").await;
    let one = db
        .store
        .finish_artifact(
            &first_lease,
            &verify(&objects.client, &first_bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
    let two = db
        .store
        .finish_artifact(
            &second_lease,
            &verify(&objects.client, &second_bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(one.state, ArtifactState::Committed);
    assert_eq!(two.state, ArtifactState::Conflict);
    let first_stop = stop(&db, &token, &computer).await;
    let second_stop = stop(&db, &token, &second).await;
    assert_eq!(first_stop.input_revision, 2);
    assert_eq!(second_stop.input_revision, 3);
    db.crash_and_restart().await;
    let default = db
        .store
        .admit_computer_start(
            &token,
            &key("default"),
            &computer,
            &start(first_stop.control_revision, None),
        )
        .await
        .unwrap();
    let request = start(second_stop.control_revision, Some(&two.commit_id));
    let selected = db
        .store
        .admit_computer_start(&token, &key("selected"), &second, &request)
        .await
        .unwrap();
    assert_eq!(default.input_artifact_id, Some(one.commit_id.clone()));
    assert_eq!(default.input_revision, Some(2));
    assert_eq!(selected.input_artifact_id, Some(two.commit_id.clone()));
    assert_eq!(selected.input_revision, Some(3));
    assert_ne!(selected.candidate_id, two.candidate_id);
    assert_eq!(selected.generation, 2);
    assert_eq!(
        db.store
            .candidate_input_artifact(&org("acme"), &selected.request_id)
            .await
            .unwrap(),
        Some(second_bundle)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM workspace_input_heads")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        2
    );
    let events = count(&db, "events").await;
    assert_eq!(
        db.store
            .admit_computer_start(&token, &key("selected"), &second, &request)
            .await
            .unwrap(),
        selected
    );
    assert_eq!(count(&db, "events").await, events);
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("selected"),
                &second,
                &start(second_stop.control_revision, Some(&one.commit_id))
            )
            .await,
        Err(Error::IdempotencyConflict)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT sum(storage_bytes)::bigint FROM runtime_start_requests"
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        40 * 1024 * 1024 * 1024
    );
    let mut publish = prepare(&db, &token, &second, &selected).await;
    publish.base_revision = one.input_revision.unwrap();
    publish.base_manifest = one.manifest_digest.unwrap();
    assert!(matches!(
        db.store
            .commit_workspace_artifact(&token, &key("selected-publish"), &workspace, &publish)
            .await,
        Err(Error::RuntimeConflict)
    ));
    publish.base_revision = selected.input_revision.unwrap();
    publish.base_manifest = selected.input_manifest_digest.unwrap();
    let retained = db
        .store
        .commit_workspace_artifact(&token, &key("selected-publish"), &workspace, &publish)
        .await
        .unwrap();
    let (lease, bundle) =
        capture_bytes(&db, &retained.commit_id, &objects, b"continued-conflict").await;
    let published = db
        .store
        .finish_artifact(
            &lease,
            &verify(&objects.client, &bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(published.state, ArtifactState::Conflict);
    assert_eq!(published.input_revision, Some(4));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM workspace_input_heads")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn selected_input_requires_published_version_and_fresh_workspace_access() {
    let (db, token, computer, workspace, mut input) = setup_many(false, 2).await;
    input.publish_current = false;
    let second = other(&db, &computer).await;
    let artifact = admit(&db, &token, &workspace, &input).await;
    let request = start(1, Some(&artifact.commit_id));
    assert!(matches!(
        db.store
            .admit_computer_start(&token, &key("selected"), &second, &request)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(count(&db, "runtime_start_requests").await, 1);
    let objects = Objects::new();
    let (lease, bundle) = capture(&db, &artifact.commit_id, &objects).await;
    db.store
        .finish_artifact(
            &lease,
            &verify(&objects.client, &bundle, None).await.unwrap(),
        )
        .await
        .unwrap();
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
            .admit_computer_start(&token, &key("selected"), &second, &request)
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    allow(
        &db,
        &workspace,
        RuntimeKind::Workspace,
        RuntimePermission::Read,
        None,
    )
    .await;
    let receipt = db
        .store
        .admit_computer_start(&token, &key("selected"), &second, &request)
        .await
        .unwrap();
    assert_eq!(receipt.input_revision, Some(2));
    assert_eq!(receipt.input_artifact_id, Some(artifact.commit_id));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM workspace_input_heads")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        1
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
    assert!(
        db.store
            .admit_computer_start(&token, &key("selected"), &second, &request)
            .await
            .is_err()
    );
}

#[test]
fn omitted_artifact_preserves_legacy_start_serialization_for_idempotency() {
    let legacy =
        json!({"expected_revision":1,"expected_spec_revision":1,"max_runtime_seconds":300});
    let request: StartRequest = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), legacy);
}
