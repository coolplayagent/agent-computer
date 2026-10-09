use super::runtime::{allow, runtime_fixture, runtime_token};
use super::*;
use agent_computer_store::{Error, runtime::*};
use sqlx::Row;

fn start() -> StartRequest {
    StartRequest {
        expected_revision: 1,
        expected_spec_revision: 1,
        max_runtime_seconds: 300,
    }
}
pub(super) async fn grants(db: &Database) {
    for row in sqlx::query("SELECT resource_id,kind FROM resource_definitions WHERE organization='acme' UNION ALL SELECT resource_id,kind FROM catalog_references WHERE organization='acme' AND kind='browser_profile'").fetch_all(&db.pool).await.unwrap() {
        let id: String = row.try_get("resource_id").unwrap();
        let kind: String = row.try_get("kind").unwrap();
        let (kind, permissions): (RuntimeKind, &[RuntimePermission]) = match kind.as_str() {
            "computer" => (RuntimeKind::Computer, &[RuntimePermission::Activate, RuntimePermission::Read, RuntimePermission::Manage]),
            "workspace" => (RuntimeKind::Workspace, &[RuntimePermission::Read, RuntimePermission::Modify]),
            "app" => (RuntimeKind::App, &[RuntimePermission::AppUse, RuntimePermission::Activate]),
            "browser_profile" => (RuntimeKind::BrowserProfile, &[RuntimePermission::AppUse]),
            _ => continue,
        };
        for permission in permissions {
            allow(db, &id, kind, *permission, (*permission == RuntimePermission::Activate).then_some(600)).await;
        }
    }
}
async fn cancel(db: &Database, token: &str, receipt: &StartReceipt) -> ComputerRuntime {
    db.store
        .cancel_queued_computer_start(
            token,
            &key(&format!("cancel-{}", receipt.generation)),
            &receipt.computer_id,
            &CancelQueuedStart {
                expected_revision: receipt.control_revision,
                request_id: receipt.request_id.clone(),
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn start_admission_survives_wal_restart_and_cancel_preserves_generation_history() {
    let (mut db, _, token, computer, _) = runtime_fixture().await;
    grants(&db).await;
    let initial = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(
        (initial.revision, initial.generation, initial.ready),
        (1, 0, false)
    );
    assert_eq!(count(&db, "runtime_controls").await, 0);
    let receipt = db
        .store
        .admit_computer_start(&token, &key("start"), &computer, &start())
        .await
        .unwrap();
    assert_eq!(
        (
            receipt.control_revision,
            receipt.generation,
            receipt.spec_revision
        ),
        (2, 1, 1)
    );
    assert_eq!(
        (
            receipt.cpu_millis,
            receipt.memory_mib,
            receipt.storage_bytes
        ),
        (4000, 6144, 10 * 1024 * 1024 * 1024)
    );
    assert_eq!(receipt.state, StartState::Queued);
    let row = sqlx::query("SELECT snapshot,credential_id,queue_deadline_at_ms > floor(extract(epoch from clock_timestamp())*1000) AS future FROM runtime_start_requests").fetch_one(&db.pool).await.unwrap();
    let snapshot: Value = row.try_get("snapshot").unwrap();
    assert!(snapshot["resources"].as_array().unwrap().len() >= 9);
    assert!(!snapshot.to_string().contains(&token));
    assert!(row.try_get::<bool, _>("future").unwrap());
    assert!(
        !row.try_get::<String, _>("credential_id")
            .unwrap()
            .is_empty()
    );
    assert!(
        sqlx::query("UPDATE runtime_start_requests SET generation=2")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM runtime_start_requests")
            .execute(&db.pool)
            .await
            .is_err()
    );
    db.crash_and_restart().await;
    let replay = db
        .store
        .admit_computer_start(&token, &key("start"), &computer, &start())
        .await
        .unwrap();
    assert_eq!(receipt, replay);
    let stopped = cancel(&db, &token, &receipt).await;
    assert_eq!(
        (stopped.revision, stopped.generation, stopped.ready),
        (3, 1, false)
    );
    assert!(stopped.active_request.is_none());
    assert_eq!(cancel(&db, &token, &receipt).await, stopped);
    // Replaying the admission receipt does not revive a cancelled request.
    assert_eq!(
        db.store
            .admit_computer_start(&token, &key("start"), &computer, &start())
            .await
            .unwrap(),
        receipt
    );
    let next = db
        .store
        .admit_computer_start(
            &token,
            &key("next"),
            &computer,
            &StartRequest {
                expected_revision: 3,
                ..start()
            },
        )
        .await
        .unwrap();
    assert_eq!((next.control_revision, next.generation), (4, 2));
    assert_ne!(next.candidate_id, receipt.candidate_id);
    assert_eq!(count(&db, "runtime_start_requests").await, 2);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM events e JOIN outbox o USING (organization,sequence) WHERE e.kind IN ('computer.start_queued','computer.start_cancelled')").fetch_one(&db.pool).await.unwrap(), 3);
}

#[tokio::test]
async fn concurrent_start_keys_allocate_one_generation_and_detect_changed_intent() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    grants(&db).await;
    let request = start();
    let one = key("same");
    let (a, b) = tokio::join!(
        db.store
            .admit_computer_start(&token, &one, &computer, &request),
        db.store
            .admit_computer_start(&token, &one, &computer, &request)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("same"),
                &computer,
                &StartRequest {
                    max_runtime_seconds: 301,
                    ..start()
                }
            )
            .await,
        Err(Error::IdempotencyConflict)
    ));
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("different"),
                &computer,
                &StartRequest {
                    expected_revision: 2,
                    ..start()
                }
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    assert_eq!(count(&db, "runtime_start_requests").await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM events WHERE kind='computer.start_queued'"
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn start_requires_all_live_runtime_grants_and_never_definition_manage() {
    let (db, definition, token, computer, workspace) = runtime_fixture().await;
    assert!(matches!(
        db.store
            .admit_computer_start(&definition, &key("d"), &computer, &start())
            .await,
        Err(Error::Forbidden)
    ));
    grants(&db).await;
    let profile: String = sqlx::query_scalar(
        "SELECT resource_id FROM catalog_references WHERE kind='browser_profile'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    for (kind, id, permission) in [
        (
            RuntimeKind::Workspace,
            &workspace,
            RuntimePermission::Modify,
        ),
        (
            RuntimeKind::BrowserProfile,
            &profile,
            RuntimePermission::AppUse,
        ),
    ] {
        let grant = RuntimeGrant {
            organization: &org("acme"),
            principal: &principal("alice"),
            kind,
            resource_id: id,
            permission,
            max_runtime_seconds: None,
        };
        db.store.set_runtime_grant(grant, false).await.unwrap();
        assert!(matches!(
            db.store
                .admit_computer_start(&token, &key("retry"), &computer, &start())
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
        assert_eq!(count(&db, "runtime_controls").await, 0);
        allow(&db, id, kind, permission, None).await;
    }
    let narrow = runtime_token(&db, "acme", "alice", &[ServiceScope::RuntimeActivate]).await;
    assert!(matches!(
        db.store
            .admit_computer_start(&narrow, &key("narrow"), &computer, &start())
            .await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("big"),
                &computer,
                &StartRequest {
                    max_runtime_seconds: 601,
                    ..start()
                }
            )
            .await,
        Err(Error::RuntimeBudgetExceeded)
    ));
    db.store
        .admit_computer_start(&token, &key("retry"), &computer, &start())
        .await
        .unwrap();
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::BrowserProfile,
                resource_id: &profile,
                permission: RuntimePermission::AppUse,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .admit_computer_start(&token, &key("retry"), &computer, &start())
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    for (organization, actor) in [("acme", "mallory"), ("other", "alice")] {
        let outsider = runtime_token(&db, organization, actor, &ServiceScope::ALL).await;
        assert!(matches!(
            db.store.computer_runtime(&outsider, &computer).await,
            Err(Error::RuntimeAccessUnavailable)
        ));
        assert!(matches!(
            db.store
                .admit_computer_start(&outsider, &key("retry"), &computer, &start())
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
}

#[tokio::test]
async fn queued_snapshot_pins_definition_versions_and_catalog_must_remain_enabled() {
    let (db, definition, token, computer, _) = runtime_fixture().await;
    grants(&db).await;
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("bad-version"),
                &computer,
                &StartRequest {
                    expected_spec_revision: 2,
                    ..start()
                }
            )
            .await,
        Err(Error::RevisionConflict)
    ));
    let receipt = db
        .store
        .admit_computer_start(&token, &key("start"), &computer, &start())
        .await
        .unwrap();
    let snapshot: Value = sqlx::query_scalar("SELECT snapshot FROM runtime_start_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut updated = example();
    updated["metadata"]["expectedRevision"] = 1.into();
    for name in [
        "volumes",
        "workspaces",
        "sandboxes",
        "apps",
        "agents",
        "computers",
    ] {
        for value in updated["spec"][name].as_array_mut().unwrap() {
            value["expectedRevision"] = 1.into();
        }
    }
    updated["spec"]["sandboxes"][0]["resources"]["cpuMillis"] = 3000.into();
    let plan = db
        .store
        .create_definition_plan(&definition, &key("update-plan"), &checked(&updated))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(
            &definition,
            &key("update-apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    assert_eq!(
        db.store
            .admit_computer_start(&token, &key("start"), &computer, &start())
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        snapshot,
        sqlx::query_scalar::<_, Value>("SELECT snapshot FROM runtime_start_requests")
            .fetch_one(&db.pool)
            .await
            .unwrap()
    );
    cancel(&db, &token, &receipt).await;
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("stale"),
                &computer,
                &StartRequest {
                    expected_revision: 3,
                    ..start()
                }
            )
            .await,
        Err(Error::RevisionConflict)
    ));
    sqlx::query("UPDATE catalog_references SET enabled=false WHERE kind='network_policy'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("disabled"),
                &computer,
                &StartRequest {
                    expected_revision: 3,
                    expected_spec_revision: 2,
                    ..start()
                }
            )
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    sqlx::query("UPDATE catalog_references SET enabled=true WHERE kind='network_policy'")
        .execute(&db.pool)
        .await
        .unwrap();
    let next = db
        .store
        .admit_computer_start(
            &token,
            &key("updated"),
            &computer,
            &StartRequest {
                expected_revision: 3,
                expected_spec_revision: 2,
                ..start()
            },
        )
        .await
        .unwrap();
    assert_eq!(next.cpu_millis, 5000);
    assert_ne!(next.snapshot_digest, receipt.snapshot_digest);
}

async fn multi_fixture(
    n: usize,
    volume_gib: i64,
    same_workspace: bool,
) -> (Database, String, Vec<String>) {
    let (db, definition, mut document) = fixture().await;
    document["spec"]["volumes"][0]["quotaBytes"] = (volume_gib * 1024 * 1024 * 1024).into();
    for i in 1..n {
        let mut computer = document["spec"]["computers"][0].clone();
        computer["name"] = format!("computer-{i}").into();
        if !same_workspace {
            let mut workspace = document["spec"]["workspaces"][0].clone();
            workspace["name"] = format!("workspace-{i}").into();
            computer["workspaceRef"] = workspace["name"].clone();
            document["spec"]["workspaces"]
                .as_array_mut()
                .unwrap()
                .push(workspace);
        }
        document["spec"]["computers"]
            .as_array_mut()
            .unwrap()
            .push(computer);
    }
    let plan = db
        .store
        .create_definition_plan(&definition, &key("multi-plan"), &checked(&document))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(
            &definition,
            &key("multi-apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    grants(&db).await;
    let token = runtime_token(&db, "acme", "alice", &ServiceScope::ALL).await;
    let ids = plan
        .resources
        .into_iter()
        .filter(|r| r.kind == DefinitionKind::Computer)
        .map(|r| r.resource_id)
        .collect();
    (db, token, ids)
}

#[tokio::test]
async fn concurrent_volume_capacity_and_workspace_ownership_are_reserved_until_cancel() {
    for same_workspace in [false, true] {
        let (db, token, ids) =
            multi_fixture(2, if same_workspace { 20 } else { 10 }, same_workspace).await;
        let request = start();
        let a = key("a");
        let b = key("b");
        let (one, two) = tokio::join!(
            db.store.admit_computer_start(&token, &a, &ids[0], &request),
            db.store.admit_computer_start(&token, &b, &ids[1], &request)
        );
        let (receipt, other) = match (one, two) {
            (Ok(receipt), Err(Error::RuntimeCapacityUnavailable)) => (receipt, &ids[1]),
            (Err(Error::RuntimeCapacityUnavailable), Ok(receipt)) => (receipt, &ids[0]),
            result => panic!("unexpected admission result {result:?}"),
        };
        assert_eq!(count(&db, "runtime_start_requests").await, 1);
        cancel(&db, &token, &receipt).await;
        db.store
            .admit_computer_start(&token, &key("after-release"), other, &start())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn per_principal_queue_limit_is_durable_and_rejected_work_does_not_consume_ids() {
    let (db, token, ids) = multi_fixture(9, 100, false).await;
    for (i, id) in ids.iter().take(8).enumerate() {
        db.store
            .admit_computer_start(&token, &key(&format!("start-{i}")), id, &start())
            .await
            .unwrap();
    }
    assert!(matches!(
        db.store
            .admit_computer_start(&token, &key("ninth"), &ids[8], &start())
            .await,
        Err(Error::RuntimeCapacityUnavailable)
    ));
    assert_eq!(count(&db, "runtime_controls").await, 8);
    assert_eq!(count(&db, "runtime_start_requests").await, 8);
}

#[tokio::test]
async fn failed_receipt_and_late_credential_expiry_roll_back_entire_admission() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    grants(&db).await;
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION reject_start_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END $$; CREATE TRIGGER reject_start_receipt BEFORE INSERT ON request_records FOR EACH ROW EXECUTE FUNCTION reject_start_receipt();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .admit_computer_start(&token, &key("retry"), &computer, &start())
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "runtime_controls").await, 0);
    assert_eq!(count(&db, "runtime_start_requests").await, 0);
    assert_eq!(count(&db, "events").await, before);
    sqlx::raw_sql("DROP TRIGGER reject_start_receipt ON request_records; CREATE FUNCTION expire_during_start() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.activate'=ANY(scopes); PERFORM pg_sleep(0.01); RETURN NEW; END $$; CREATE TRIGGER expire_during_start BEFORE INSERT ON request_records FOR EACH ROW EXECUTE FUNCTION expire_during_start();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .admit_computer_start(&token, &key("retry"), &computer, &start())
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "runtime_controls").await, 0);
    assert_eq!(count(&db, "events").await, before);
    sqlx::query("DROP TRIGGER expire_during_start ON request_records")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        db.store
            .admit_computer_start(&token, &key("retry"), &computer, &start())
            .await
            .unwrap()
            .generation,
        1
    );
}

#[tokio::test]
async fn preparing_cannot_be_cancelled_or_reused_without_fencing() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    grants(&db).await;
    let receipt = db
        .store
        .admit_computer_start(&token, &key("start"), &computer, &start())
        .await
        .unwrap();
    sqlx::query("UPDATE runtime_start_requests SET state='Preparing'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .cancel_queued_computer_start(
                &token,
                &key("cancel"),
                &computer,
                &CancelQueuedStart {
                    expected_revision: 2,
                    request_id: receipt.request_id
                }
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    assert!(
        sqlx::query("UPDATE runtime_start_requests SET state='Cancelled'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE runtime_start_requests SET state='Queued'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(matches!(
        db.store
            .admit_computer_start(
                &token,
                &key("new"),
                &computer,
                &StartRequest {
                    expected_revision: 2,
                    ..start()
                }
            )
            .await,
        Err(Error::RuntimeConflict)
    ));
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(current.start_state, Some(StartState::Preparing));
    assert!(!current.ready);
}

#[tokio::test]
async fn grant_revocation_while_waiting_for_admission_lock_is_seen_before_allocation() {
    let (db, _, token, computer, workspace) = runtime_fixture().await;
    grants(&db).await;
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM organization_streams WHERE organization='acme' FOR UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let store = db.store.clone();
    let id = computer.clone();
    let pending = tokio::spawn(async move {
        store
            .admit_computer_start(&token, &key("waiting"), &id, &start())
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!pending.is_finished());
    sqlx::query("DELETE FROM runtime_grants WHERE resource_id=$1 AND permission='modify'")
        .bind(workspace)
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker.commit().await.unwrap();
    assert!(matches!(
        pending.await.unwrap(),
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(count(&db, "runtime_controls").await, 0);
}

#[tokio::test]
async fn admission_migration_preserves_existing_definitions_and_grants_without_autostart() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    grants(&db).await;
    let before: Value = sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(d) ORDER BY resource_id) FROM resource_definitions d",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let permissions = count(&db, "runtime_grants").await;
    db.remove_execution_admission().await;
    sqlx::raw_sql("DROP TABLE candidate_writer_completions; DROP FUNCTION guard_writer_completion(); DROP TABLE candidate_writer_drains,candidate_writer_dispatches,candidate_writer_epochs,candidate_writer_leases; DROP FUNCTION guard_writer_record_insert(); DROP FUNCTION guard_writer_lease_mutation(); DROP TABLE connection_sessions; DROP FUNCTION guard_connection_session_mutation(); DROP TABLE candidate_preparations,runtime_start_inputs,workspace_input_heads,workspace_input_versions; DROP FUNCTION guard_candidate_preparation(); DROP TABLE runtime_controls,runtime_start_requests CASCADE; DROP FUNCTION guard_runtime_start_mutation(); DELETE FROM _sqlx_migrations WHERE version>=7;").execute(&db.pool).await.unwrap();
    assert!(matches!(db.store.ready().await, Err(Error::SchemaNotReady)));
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(
        before,
        sqlx::query_scalar::<_, Value>(
            "SELECT jsonb_agg(to_jsonb(d) ORDER BY resource_id) FROM resource_definitions d"
        )
        .fetch_one(&db.pool)
        .await
        .unwrap()
    );
    assert_eq!(count(&db, "runtime_grants").await, permissions);
    assert_eq!(count(&db, "runtime_controls").await, 0);
    assert_eq!(count(&db, "runtime_start_requests").await, 0);
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .generation,
        0
    );
}

#[tokio::test]
async fn cancelled_capacity_is_not_released_when_outbox_publication_fails() {
    let (db, _, token, computer, _) = runtime_fixture().await;
    grants(&db).await;
    let receipt = db
        .store
        .admit_computer_start(&token, &key("start"), &computer, &start())
        .await
        .unwrap();
    let before = count(&db, "events").await;
    sqlx::raw_sql("CREATE FUNCTION fail_cancel_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END $$; CREATE TRIGGER fail_cancel_outbox BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION fail_cancel_outbox();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .cancel_queued_computer_start(
                &token,
                &key("cancel"),
                &computer,
                &CancelQueuedStart {
                    expected_revision: 2,
                    request_id: receipt.request_id.clone()
                }
            )
            .await,
        Err(Error::Database(_))
    ));
    let current = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(
        (current.revision, current.start_state),
        (2, Some(StartState::Queued))
    );
    assert_eq!(count(&db, "events").await, before);
    sqlx::query("DROP TRIGGER fail_cancel_outbox ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(cancel(&db, &token, &receipt).await.revision, 3);
}
