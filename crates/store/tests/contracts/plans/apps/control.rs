//! PostgreSQL admission tests use explicit synthetic Running evidence. Actual
//! node/driver/HTTP control acceptance is separately exercised in the VM fixture.
use super::*;
use agent_computer_browser::Action;
use agent_computer_store::runtime::apps::browser::control::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

async fn entry(db: &Database, id: &str, kind: &str, payload: Value) {
    let now: i64 =
        sqlx::query_scalar("SELECT (extract(epoch from clock_timestamp())*1000)::bigint")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let mut hash = Sha256::new();
    hash.update(b"agent-computer/app-browser-journal-v1\0");
    hash.update(serde_json::to_vec(&json!(["acme", id, kind, payload, now])).unwrap());
    sqlx::query("INSERT INTO app_browser_journal(organization,instance_id,kind,payload,digest,recorded_at_ms) VALUES('acme',$1,$2,$3,$4,$5)").bind(id).bind(kind).bind(payload).bind(format!("sha256:{:x}",hash.finalize())).bind(now).execute(&db.pool).await.unwrap();
}
async fn fixture() -> (Database, String, AppInstance, String) {
    let (db, token, computer, input) = setup().await;
    let app = admit(&db, &token, &computer, &input).await;
    let _attempt = db
        .store
        .begin_browser_app(
            &org("acme"),
            &app.instance_id,
            1,
            &super::browser::deployment(),
        )
        .await
        .unwrap();
    entry(&db, &app.instance_id, "pod", json!({"pod_uid":"synthetic"})).await;
    let now: i64 =
        sqlx::query_scalar("SELECT (extract(epoch from clock_timestamp())*1000)::bigint")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    entry(
        &db,
        &app.instance_id,
        "arm",
        json!({"evidence":{},"expires_at_ms":now+30_000}),
    )
    .await;
    entry(&db, &app.instance_id, "launch", json!({"synthetic":true})).await;
    entry(&db, &app.instance_id, "running", json!({"synthetic":true})).await;
    sqlx::query("UPDATE app_instances SET state='Running',revision=revision+1")
        .execute(&db.pool)
        .await
        .unwrap();
    for (kind, id) in [
        (RuntimeKind::Computer, computer.as_str()),
        (RuntimeKind::App, app.app_id.as_str()),
    ] {
        for p in [RuntimePermission::Observe, RuntimePermission::Control] {
            allow(&db, id, kind, p, None).await;
        }
    }
    allow(
        &db,
        &app.profile_id,
        RuntimeKind::BrowserProfile,
        RuntimePermission::Read,
        None,
    )
    .await;
    let session = connection(&db, &token, &computer, "gui").await;
    (db, token, app, session)
}
pub(super) async fn connection(
    db: &Database,
    token: &str,
    computer: &str,
    key_name: &str,
) -> String {
    db.store
        .create_connection_session(
            token,
            &key(key_name),
            computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::AppUse,
                    RuntimePermission::Observe,
                    RuntimePermission::Control,
                ],
                lifetime_seconds: 300,
            },
        )
        .await
        .unwrap()
        .session_id
}
fn acquire(session: &str, epoch: i64) -> AcquireControl {
    AcquireControl {
        connection_session_id: session.into(),
        app_generation: 1,
        expected_epoch: epoch,
        lifetime_ms: 25_000,
    }
}
fn submit(session: &str, epoch: i64, command: BrowserCommand) -> SubmitBrowserCommand {
    SubmitBrowserCommand {
        connection_session_id: session.into(),
        app_generation: 1,
        control_epoch: epoch,
        command,
    }
}

#[tokio::test]
async fn browser_control_race_retry_release_requires_original_drain() {
    let (mut db, token, app, session) = fixture().await;
    let other = connection(&db, &token, &app.computer_id, "other").await;
    let a = acquire(&session, 0);
    let b = acquire(&other, 0);
    let k1 = key("first");
    let k2 = key("second");
    let (r1, r2) = tokio::join!(
        db.store
            .acquire_browser_control(&token, &k1, &app.instance_id, &a),
        db.store
            .acquire_browser_control(&token, &k2, &app.instance_id, &b)
    );
    assert_eq!(usize::from(r1.is_ok()) + usize::from(r2.is_ok()), 1);
    let (winner, winner_input, winner_key) = if let Ok(v) = r1 {
        (v, a, k1)
    } else {
        (r2.unwrap(), b, k2)
    };
    db.crash_and_restart().await;
    let again = db
        .store
        .acquire_browser_control(&token, &winner_key, &app.instance_id, &winner_input)
        .await
        .unwrap();
    assert_eq!(
        (again.control_epoch, again.expires_at_ms),
        (winner.control_epoch, winner.expires_at_ms)
    );
    let released = db
        .store
        .release_browser_control(
            &token,
            &app.instance_id,
            &ReleaseControl {
                connection_session_id: winner.owner_connection.clone(),
                control_epoch: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(released.state, "Draining");
    assert!(
        db.store
            .acquire_browser_control(
                &token,
                &key("premature"),
                &app.instance_id,
                &acquire(&other, 1)
            )
            .await
            .is_err()
    );
    assert!(
        db.store
            .submit_browser_command(
                &token,
                &key("old-action"),
                &app.instance_id,
                &submit(
                    &winner.owner_connection,
                    1,
                    BrowserCommand::Act {
                        action: Action::CreatePage {}
                    }
                )
            )
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE browser_controls SET state='Idle'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert_eq!(count(&db, "browser_requests").await, 0);
}
#[tokio::test]
async fn browser_queue_normalizes_and_retries_without_duplicate_or_identity_override() {
    let (db, token, app, session) = fixture().await;
    db.store
        .acquire_browser_control(
            &token,
            &key("control"),
            &app.instance_id,
            &acquire(&session, 0),
        )
        .await
        .unwrap();
    let input = submit(
        &session,
        1,
        BrowserCommand::Act {
            action: Action::Navigate {
                page_id: "page_one".into(),
                observation_id: "observation_one".into(),
                url: "https://example.invalid".into(),
            },
        },
    );
    let first = db
        .store
        .submit_browser_command(&token, &key("navigate"), &app.instance_id, &input)
        .await
        .unwrap();
    let retry = db
        .store
        .submit_browser_command(&token, &key("navigate"), &app.instance_id, &input)
        .await
        .unwrap();
    assert_eq!(
        (first.request_id.clone(), first.expires_at_ms),
        (retry.request_id, retry.expires_at_ms)
    );
    assert_eq!(count(&db, "browser_requests").await, 1);
    let stored: Value = sqlx::query_scalar("SELECT command FROM browser_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored["action"]["url"], "https://example.invalid/");
    let other = connection(&db, &token, &app.computer_id, "other").await;
    assert!(
        db.store
            .submit_browser_command(
                &token,
                &key("stolen"),
                &app.instance_id,
                &submit(
                    &other,
                    1,
                    BrowserCommand::Act {
                        action: Action::CreatePage {}
                    }
                )
            )
            .await
            .is_err()
    );
    assert!(matches!(
        db.store
            .submit_browser_command(
                &token,
                &key("navigate"),
                &app.instance_id,
                &submit(
                    &session,
                    1,
                    BrowserCommand::Act {
                        action: Action::CreatePage {}
                    }
                )
            )
            .await,
        Err(Error::IdempotencyConflict)
    ));
    for query in [
        "UPDATE browser_requests SET command='{}'",
        "DELETE FROM browser_requests",
        "UPDATE browser_requests SET state='Complete'",
        "UPDATE browser_controls SET epoch=epoch+1",
    ] {
        assert!(
            sqlx::query(query).execute(&db.pool).await.is_err(),
            "{query}"
        );
    }
    let metadata = db
        .store
        .browser_command(&token, &first.request_id)
        .await
        .unwrap();
    assert_eq!(metadata.state, "Queued");
    assert!(metadata.result.is_none());
}
#[tokio::test]
async fn browser_control_revocation_is_sticky_and_observation_has_separate_permission() {
    let (db, token, app, session) = fixture().await;
    db.store
        .acquire_browser_control(
            &token,
            &key("control"),
            &app.instance_id,
            &acquire(&session, 0),
        )
        .await
        .unwrap();
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::App,
                resource_id: &app.app_id,
                permission: RuntimePermission::Control,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    allow(
        &db,
        &app.app_id,
        RuntimeKind::App,
        RuntimePermission::Control,
        None,
    )
    .await;
    assert_eq!(
        db.store
            .browser_control(&token, &app.instance_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "Draining"
    );
    assert!(
        db.store
            .submit_browser_command(
                &token,
                &key("revoked"),
                &app.instance_id,
                &submit(
                    &session,
                    1,
                    BrowserCommand::Act {
                        action: Action::CreatePage {}
                    }
                )
            )
            .await
            .is_err()
    );
    let observed = db
        .store
        .submit_browser_command(
            &token,
            &key("read-only"),
            &app.instance_id,
            &submit(&session, 1, BrowserCommand::Pages),
        )
        .await
        .unwrap();
    assert_eq!(observed.state, "Queued");
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::BrowserProfile,
                resource_id: &app.profile_id,
                permission: RuntimePermission::Read,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(
        db.store
            .browser_command(&token, &observed.request_id)
            .await
            .is_err()
    );
    assert!(
        db.store
            .submit_browser_command(
                &token,
                &key("no-profile"),
                &app.instance_id,
                &submit(&session, 1, BrowserCommand::Pages)
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn browser_control_expiry_never_implicitly_transfers_and_pending_queue_is_bounded() {
    let (db, token, app, session) = fixture().await;
    let mut input = acquire(&session, 0);
    input.lifetime_ms = 1;
    db.store
        .acquire_browser_control(&token, &key("control"), &app.instance_id, &input)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        db.store
            .browser_control(&token, &app.instance_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "Draining"
    );
    assert!(
        db.store
            .acquire_browser_control(
                &token,
                &key("retry-expired"),
                &app.instance_id,
                &acquire(&session, 1)
            )
            .await
            .is_err()
    );
    let command = submit(&session, 1, BrowserCommand::Pages);
    for n in 0..128 {
        db.store
            .submit_browser_command(
                &token,
                &key(&format!("page-{n}")),
                &app.instance_id,
                &command,
            )
            .await
            .unwrap();
    }
    assert!(matches!(
        db.store
            .submit_browser_command(&token, &key("overflow"), &app.instance_id, &command)
            .await,
        Err(Error::RuntimeBudgetExceeded)
    ));
    assert_eq!(count(&db, "browser_requests").await, 128);
}

#[tokio::test]
async fn browser_shared_observation_requires_explicit_profile_read_and_never_implies_control() {
    let (db, token, app, session) = fixture().await;
    let bob = runtime_token(&db, "acme", "bob", &ServiceScope::ALL).await;
    for (kind, id, permissions) in [
        (
            RuntimeKind::Computer,
            app.computer_id.as_str(),
            &[
                RuntimePermission::Connect,
                RuntimePermission::Read,
                RuntimePermission::Observe,
                RuntimePermission::Control,
                RuntimePermission::AppUse,
            ][..],
        ),
        (
            RuntimeKind::App,
            app.app_id.as_str(),
            &[
                RuntimePermission::Read,
                RuntimePermission::Observe,
                RuntimePermission::Control,
                RuntimePermission::AppUse,
            ][..],
        ),
    ] {
        for permission in permissions {
            db.store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org("acme"),
                        principal: &principal("bob"),
                        kind,
                        resource_id: id,
                        permission: *permission,
                        max_runtime_seconds: None,
                    },
                    true,
                )
                .await
                .unwrap();
        }
    }
    let viewer = connection(&db, &bob, &app.computer_id, "bob-view").await;
    let command = submit(&viewer, 0, BrowserCommand::Pages);
    assert!(
        db.store
            .submit_browser_command(&bob, &key("no-profile"), &app.instance_id, &command)
            .await
            .is_err()
    );
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("bob"),
                kind: RuntimeKind::BrowserProfile,
                resource_id: &app.profile_id,
                permission: RuntimePermission::Read,
                max_runtime_seconds: None,
            },
            true,
        )
        .await
        .unwrap();
    let pages = db
        .store
        .submit_browser_command(&bob, &key("profile-read"), &app.instance_id, &command)
        .await
        .unwrap();
    assert_eq!(pages.state, "Queued");
    assert!(
        db.store
            .acquire_browser_control(
                &bob,
                &key("no-profile-use"),
                &app.instance_id,
                &acquire(&viewer, 0)
            )
            .await
            .is_err()
    );
    assert!(
        db.store
            .browser_command(&token, &pages.request_id)
            .await
            .is_err()
    );
    let mut forged = command.clone();
    forged.connection_session_id = session;
    assert!(
        db.store
            .submit_browser_command(&bob, &key("other-session"), &app.instance_id, &forged)
            .await
            .is_err()
    );
    let rotated = runtime_token(&db, "acme", "bob", &ServiceScope::ALL).await;
    assert!(
        db.store
            .browser_command(&rotated, &pages.request_id)
            .await
            .is_err()
    );
    db.store
        .close_connection_session(&bob, &viewer)
        .await
        .unwrap();
    assert!(
        db.store
            .submit_browser_command(&bob, &key("disconnected"), &app.instance_id, &command)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn browser_committed_receipt_survives_lost_status_write_without_redispatch() {
    let (mut db, token, app, session) = fixture().await;
    db.store
        .acquire_browser_control(
            &token,
            &key("control"),
            &app.instance_id,
            &acquire(&session, 0),
        )
        .await
        .unwrap();
    let input = submit(
        &session,
        1,
        BrowserCommand::Act {
            action: Action::CreatePage {},
        },
    );
    let queued = db
        .store
        .submit_browser_command(&token, &key("action"), &app.instance_id, &input)
        .await
        .unwrap();
    // Explicit database-only crash boundary fixture: effect authority still
    // requires the production worker's nonserializable claim and live guard.
    sqlx::query("UPDATE browser_requests SET state='Dispatched',dispatch_nonce='synthetic' WHERE request_id=$1").bind(&queued.request_id).execute(&db.pool).await.unwrap();
    let receipt = json!({"action_id":queued.request_id,"input_digest":format!("sha256:{}","a".repeat(64)),"app_instance_id":app.instance_id,"app_generation":1,"control_epoch":1,"outcome":"delivered","page_id":"page_synthetic"});
    for (kind, value) in [
        ("begin", json!({"synthetic":true})),
        ("complete", receipt.clone()),
    ] {
        let mut hash = Sha256::new();
        hash.update(b"agent-computer/browser-action-journal-v1\0");
        hash.update(serde_json::to_vec(&json!(["acme", queued.request_id, kind, value])).unwrap());
        sqlx::query("INSERT INTO browser_action_journal(organization,request_id,kind,payload,digest) VALUES('acme',$1,$2,$3,$4)").bind(&queued.request_id).bind(kind).bind(value).bind(format!("sha256:{:x}",hash.finalize())).execute(&db.pool).await.unwrap();
    }
    db.crash_and_restart().await;
    let recovered = db
        .store
        .browser_command(&token, &queued.request_id)
        .await
        .unwrap();
    assert_eq!(recovered.state, "Complete");
    assert_eq!(recovered.result.unwrap()["receipt"], receipt);
    let retry = db
        .store
        .submit_browser_command(&token, &key("action"), &app.instance_id, &input)
        .await
        .unwrap();
    assert_eq!(retry.request_id, queued.request_id);
    assert!(retry.result.is_none());
    assert_eq!(count(&db, "browser_requests").await, 1);
    for sql in [
        "UPDATE browser_requests SET state='Queued',dispatch_nonce=NULL",
        "DELETE FROM browser_action_journal",
        "UPDATE browser_action_journal SET payload='{}'",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
}

#[tokio::test]
async fn browser_control_requires_current_observe_and_never_revives_after_its_revocation() {
    let (db, token, app, session) = fixture().await;
    let org = org("acme");
    let actor = principal("alice");
    let revoke = || RuntimeGrant {
        organization: &org,
        principal: &actor,
        kind: RuntimeKind::App,
        resource_id: &app.app_id,
        permission: RuntimePermission::Observe,
        max_runtime_seconds: None,
    };
    db.store.set_runtime_grant(revoke(), false).await.unwrap();
    assert!(
        db.store
            .acquire_browser_control(
                &token,
                &key("no-observe"),
                &app.instance_id,
                &acquire(&session, 0)
            )
            .await
            .is_err()
    );
    assert!(
        db.store
            .browser_control(&token, &app.instance_id)
            .await
            .unwrap()
            .is_none()
    );
    db.store.set_runtime_grant(revoke(), true).await.unwrap();
    db.store
        .acquire_browser_control(
            &token,
            &key("control"),
            &app.instance_id,
            &acquire(&session, 0),
        )
        .await
        .unwrap();
    db.store.set_runtime_grant(revoke(), false).await.unwrap();
    db.store.set_runtime_grant(revoke(), true).await.unwrap();
    assert_eq!(
        db.store
            .browser_control(&token, &app.instance_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "Draining"
    );
    assert!(
        db.store
            .submit_browser_command(
                &token,
                &key("no-revival"),
                &app.instance_id,
                &submit(
                    &session,
                    1,
                    BrowserCommand::Act {
                        action: Action::CreatePage {}
                    }
                )
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn browser_drained_epoch_can_be_acquired_once_by_a_new_connection() {
    let (db, token, app, session) = fixture().await;
    let next = connection(&db, &token, &app.computer_id, "next").await;
    db.store
        .acquire_browser_control(
            &token,
            &key("first"),
            &app.instance_id,
            &acquire(&session, 0),
        )
        .await
        .unwrap();
    db.store
        .release_browser_control(
            &token,
            &app.instance_id,
            &ReleaseControl {
                connection_session_id: session,
                control_epoch: 1,
            },
        )
        .await
        .unwrap();
    // Synthetic completed barrier for the database epoch transition contract.
    // The live fixture independently exercises the private driver acknowledgement.
    sqlx::query("UPDATE browser_controls SET drain_nonce='synthetic' WHERE state='Draining'")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE browser_controls SET state='Idle' WHERE state='Draining'")
        .execute(&db.pool)
        .await
        .unwrap();
    let acquired = db
        .store
        .acquire_browser_control(&token, &key("second"), &app.instance_id, &acquire(&next, 1))
        .await
        .unwrap();
    assert_eq!(
        (
            acquired.control_epoch,
            acquired.owner_connection.as_str(),
            acquired.state.as_str()
        ),
        (2, next.as_str(), "Owned")
    );
    assert!(
        db.store
            .acquire_browser_control(
                &token,
                &key("duplicate"),
                &app.instance_id,
                &acquire(&next, 1)
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn browser_rejected_observation_returns_status_without_a_frame_deadline() {
    let (db, token, app, session) = fixture().await;
    let queued = db
        .store
        .submit_browser_command(
            &token,
            &key("missing-page"),
            &app.instance_id,
            &submit(
                &session,
                0,
                BrowserCommand::Observe {
                    page_id: "page_missing".into(),
                },
            ),
        )
        .await
        .unwrap();
    // Fixture a dispatched driver's negative result. This contains no frame.
    sqlx::query("UPDATE browser_requests SET state='Dispatched',dispatch_nonce='synthetic' WHERE request_id=$1").bind(&queued.request_id).execute(&db.pool).await.unwrap();
    sqlx::query("UPDATE browser_requests SET state='Rejected',result='{\"code\":\"observation_unavailable\"}' WHERE request_id=$1").bind(&queued.request_id).execute(&db.pool).await.unwrap();
    let rejected = db
        .store
        .browser_command(&token, &queued.request_id)
        .await
        .unwrap();
    assert_eq!(rejected.state, "Rejected");
    assert_eq!(
        rejected.result,
        Some(json!({"code":"observation_unavailable"}))
    );
    assert!(rejected.result_valid_until_ms.is_none());
    // Read authority remains mandatory even for a negative result.
    db.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org("acme"),
                principal: &principal("alice"),
                kind: RuntimeKind::BrowserProfile,
                resource_id: &app.profile_id,
                permission: RuntimePermission::Read,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert!(
        db.store
            .browser_command(&token, &queued.request_id)
            .await
            .is_err()
    );
}
