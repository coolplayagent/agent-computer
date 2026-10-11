//! Actual CLI/HTTP/PostgreSQL admission; Running journal entries below are synthetic.
use super::*;
use agent_computer_store::runtime::*;
use sha2::{Digest, Sha256};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_cli_app_browser_retry_control_queue_release_cancel_and_revocation() {
    let db = Postgres::new().await;
    let store = Store::new(db.pool.clone());
    store.migrate().await.unwrap();
    let org = OrganizationId::new("acme").unwrap();
    let principal = PrincipalId::new("alice").unwrap();
    let credential = store
        .issue_credential(IssueCredential {
            organization: &org,
            principal: &principal,
            kind: PrincipalKind::Agent,
            scopes: &ServiceScope::ALL,
            lifetime: Duration::from_secs(3600),
        })
        .await
        .unwrap();
    let token = credential.expose_token();
    let computer = runtime::provision_browser(&store, token, "alice").await;
    let app: String =
        sqlx::query_scalar("SELECT resource_id FROM resource_definitions WHERE kind='app'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let profile: String = sqlx::query_scalar(
        "SELECT resource_id FROM catalog_references WHERE kind='browser_profile'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    store
        .register_browser_profile_owner(&org, &profile, &principal)
        .await
        .unwrap();
    for (kind, id, permissions) in [
        (
            RuntimeKind::Computer,
            &computer,
            vec![
                RuntimePermission::AppUse,
                RuntimePermission::Manage,
                RuntimePermission::Observe,
                RuntimePermission::Control,
            ],
        ),
        (
            RuntimeKind::App,
            &app,
            vec![
                RuntimePermission::Read,
                RuntimePermission::AppUse,
                RuntimePermission::Activate,
                RuntimePermission::Manage,
                RuntimePermission::Observe,
                RuntimePermission::Control,
            ],
        ),
        (
            RuntimeKind::BrowserProfile,
            &profile,
            vec![RuntimePermission::Read, RuntimePermission::AppUse],
        ),
    ] {
        for permission in permissions {
            store
                .set_runtime_grant(
                    RuntimeGrant {
                        organization: &org,
                        principal: &principal,
                        kind,
                        resource_id: id,
                        permission,
                        max_runtime_seconds: (permission == RuntimePermission::Activate)
                            .then_some(300),
                    },
                    true,
                )
                .await
                .unwrap();
        }
    }
    let start = runtime::prepared(&store, &db.pool, token, &computer, "alice").await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, token).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = agent_computer_server::router(store.clone());
    let service = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let call = |args: &[&str], body, key, exit| cli(&endpoint, &token_file, args, body, key, exit);
    let session = call(
        &["connect", &computer],
        Some(
            json!({"requested_capabilities":["connect","read","app.use","activate","observe","control"],"lifetime_seconds":300}),
        ),
        Some("browser-session"),
        0,
    );
    let input = json!({"connection_session_id":session["session_id"],"computer_generation":start.generation,"app_id":app,"expected_app_revision":1,"runtime_seconds":300});
    let instance = call(
        &["app", "start", &computer],
        Some(input.clone()),
        Some("app-start"),
        0,
    );
    let id = instance["instance_id"].as_str().unwrap();
    assert_eq!(instance["state"], "Queued");
    assert_eq!(instance["ready"], false);
    for current in [
        call(
            &["app", "start", &computer],
            Some(input.clone()),
            Some("app-start"),
            0,
        ),
        call(&["app", "show", id], None, None, 0),
    ] {
        for field in [
            "instance_id",
            "state",
            "revision",
            "binding_digest",
            "created_at_ms",
            "expires_at_ms",
        ] {
            assert_eq!(current[field], instance[field], "{field}");
        }
    }
    let mut changed = input.clone();
    changed["runtime_seconds"] = json!(299);
    assert_eq!(
        call(
            &["app", "start", &computer],
            Some(changed),
            Some("app-start"),
            1
        )["http_status"],
        409
    );
    let cancel = json!({"app_generation":1,"expected_revision":instance["revision"]});
    for _ in 0..2 {
        assert_eq!(
            call(
                &["app", "cancel", id],
                Some(cancel.clone()),
                Some("app-cancel"),
                0
            )["state"],
            "Cancelled"
        );
    }
    let instance = call(
        &["app", "start", &computer],
        Some(input),
        Some("second-app"),
        0,
    );
    let id = instance["instance_id"].as_str().unwrap();
    assert_eq!(instance["app_generation"], 2);
    // Establish the existing Store test's synthetic Running precondition. No worker is launched.
    let deployment = serde_json::from_value(json!({"namespace":"test","namespace_uid":"ns-uid","approved_image":format!("registry.example.invalid/tools@sha256:{}", "a".repeat(64)),"node":{"name":"node","uid":"node-uid","boot_id":"boot-id"},"viewport":{"width":800,"height":600}})).unwrap();
    let _attempt = store
        .begin_browser_app(&org, id, 1, &deployment)
        .await
        .unwrap();
    let now: i64 =
        sqlx::query_scalar("SELECT (extract(epoch from clock_timestamp())*1000)::bigint")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    for (kind, payload) in [
        ("pod", json!({"pod_uid":"synthetic"})),
        ("arm", json!({"evidence":{},"expires_at_ms":now+30_000})),
        ("launch", json!({"synthetic":true})),
        ("running", json!({"synthetic":true})),
    ] {
        let mut hash = Sha256::new();
        hash.update(b"agent-computer/app-browser-journal-v1\0");
        hash.update(serde_json::to_vec(&json!(["acme", id, kind, payload, now])).unwrap());
        sqlx::query("INSERT INTO app_browser_journal(organization,instance_id,kind,payload,digest,recorded_at_ms) VALUES('acme',$1,$2,$3,$4,$5)").bind(id).bind(kind).bind(payload).bind(format!("sha256:{:x}", hash.finalize())).bind(now).execute(&db.pool).await.unwrap();
    }
    sqlx::query(
        "UPDATE app_instances SET state='Running',revision=revision+1 WHERE instance_id=$1",
    )
    .bind(id)
    .execute(&db.pool)
    .await
    .unwrap();
    let acquire = json!({"connection_session_id":session["session_id"],"app_generation":2,"expected_epoch":0,"lifetime_ms":25000});
    let control = call(
        &["browser", "control", "acquire", id],
        Some(acquire.clone()),
        Some("acquire"),
        0,
    );
    assert_eq!(control["state"], "Owned");
    assert_eq!(
        call(
            &["browser", "control", "acquire", id],
            Some(acquire),
            Some("acquire"),
            0
        ),
        control
    );
    assert_eq!(
        call(&["browser", "control", "show", id], None, None, 0),
        control
    );
    for kind in ["pages", "observe", "act"] {
        let command = if kind == "pages" {
            json!({"kind":kind})
        } else if kind == "observe" {
            json!({"kind":kind,"page_id":"page"})
        } else {
            json!({"kind":kind,"action":{"kind":"create_page"}})
        };
        let submit = json!({"connection_session_id":session["session_id"],"app_generation":2,"control_epoch":control["control_epoch"],"command":command});
        let queued = call(&["browser", kind, id], Some(submit.clone()), Some(kind), 0);
        assert_eq!(queued["state"], "Queued");
        assert_eq!(
            call(&["browser", kind, id], Some(submit), Some(kind), 0),
            queued
        );
        assert_eq!(
            call(
                &["browser", "status", queued["request_id"].as_str().unwrap()],
                None,
                None,
                0
            ),
            queued
        );
    }
    let release = json!({"connection_session_id":session["session_id"],"control_epoch":control["control_epoch"]});
    assert_eq!(
        call(
            &["browser", "control", "release", id],
            Some(release.clone()),
            None,
            0
        )["state"],
        "Draining"
    );
    assert_eq!(
        call(
            &["browser", "control", "release", id],
            Some(release),
            None,
            0
        )["state"],
        "Draining"
    );
    let running = call(&["app", "show", id], None, None, 0);
    let cancel = json!({"app_generation":2,"expected_revision":running["revision"]});
    assert_eq!(
        call(
            &["app", "cancel", id],
            Some(cancel),
            Some("running-cancel"),
            1
        )["http_status"],
        409
    );
    let dispatches: i64 =
        sqlx::query_scalar("SELECT count(*) FROM browser_requests WHERE state='Dispatching'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(dispatches, 0);
    store.disable_principal(&org, &principal).await.unwrap();
    assert_eq!(
        call(&["app", "show", id], None, None, 1)["http_status"],
        401
    );
    service.abort();
    let _ = service.await;
}
