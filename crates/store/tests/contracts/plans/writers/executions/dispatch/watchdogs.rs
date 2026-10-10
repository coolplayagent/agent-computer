//! Synthetic SQL records exercise metadata constraints, never a live node proof.
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};

async fn fixture() -> (
    Database,
    ExecutionDispatchAttempt,
    ExecutionPodPlan,
    Value,
    i64,
) {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let plan = pods::register(&db, &attempt).await;
    let plan = db
        .store
        .record_candidate_execution_pod(
            &org("acme"),
            &queued.execution_id,
            &plan.plan().plan_digest,
            "pod-one",
        )
        .await
        .unwrap();
    let now =
        sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp())*1000)::bigint")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let evidence = json!({"runtime":{"identity":{"pod_uid":"pod-one","node":{"uid":"node-one","boot_id":"boot-one"},"container_id":"a".repeat(64)},"cgroup_inode":123,"cgroup_path":"fixture-only"},"armed":{"version":1,"event":"armed","request":{"execution_id":queued.execution_id,"boot_id":"boot-one","cgroup_inode":123,"cgroup_path":"fixture-only"}}});
    (db, attempt, plan, evidence, now)
}

async fn insert(
    db: &Database,
    attempt: &ExecutionDispatchAttempt,
    plan: &ExecutionPodPlan,
    evidence: &Value,
    now: i64,
    expires: i64,
) -> std::result::Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    let d = attempt.intent();
    let value = json!([
        d.organization,
        d.execution.execution_id,
        d.intent_digest,
        plan.plan_digest,
        evidence,
        now,
        expires
    ]);
    let mut hash = Sha256::new();
    hash.update(b"agent-computer/execution-watchdog-v1\0");
    hash.update(serde_json::to_vec(&value).unwrap());
    sqlx::query("INSERT INTO execution_watchdog_arms (organization,execution_id,pod_uid,node_uid,boot_id,container_id,cgroup_inode,evidence,evidence_digest,registered_at_ms,expires_at_ms) VALUES ('acme',$1,'pod-one','node-one','boot-one',$2,123,$3,$4,$5,$6)")
        .bind(&d.execution.execution_id).bind("a".repeat(64)).bind(evidence).bind(format!("sha256:{:x}",hash.finalize())).bind(now).bind(expires).execute(&db.pool).await
}

#[tokio::test]
async fn watchdog_metadata_rejects_transplants_and_wal_read_cannot_recreate_live_guard() {
    let (mut db, attempt, plan, evidence, now) = fixture().await;
    let expires = now + 5000;
    for (pointer, value) in [
        ("/runtime/identity/pod_uid", json!("foreign")),
        ("/runtime/identity/node/uid", json!("foreign")),
        ("/runtime/identity/node/boot_id", json!("foreign")),
        ("/runtime/identity/container_id", json!("b".repeat(64))),
        ("/runtime/cgroup_inode", json!(124)),
        ("/armed/event", json!("empty_observed")),
        ("/armed/request/execution_id", json!("foreign")),
        ("/armed/request/cgroup_path", json!("foreign")),
    ] {
        let mut bad = evidence.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            insert(&db, &attempt, &plan, &bad, now, expires)
                .await
                .unwrap_err()
                .to_string()
                .contains("watchdog is not admitted"),
            "{pointer}"
        );
    }
    insert(&db, &attempt, &plan, &evidence, now, expires)
        .await
        .unwrap();
    assert!(
        insert(&db, &attempt, &plan, &evidence, now, expires)
            .await
            .is_err()
    );
    for sql in [
        "UPDATE execution_watchdog_arms SET expires_at_ms=expires_at_ms+1",
        "DELETE FROM execution_watchdog_arms",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
    let id = &attempt.intent().execution.execution_id;
    let original = db
        .store
        .candidate_execution_watchdog(&org("acme"), id)
        .await
        .unwrap()
        .unwrap();
    db.crash_and_restart().await;
    let read = db
        .store
        .candidate_execution_watchdog(&org("acme"), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.evidence_digest, original.evidence_digest);
    assert_eq!(read.expires_at_ms, original.expires_at_ms);
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &org("acme"),
                id,
                2,
                "pod-one",
                &pods::challenge(&attempt)
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn watchdog_metadata_enforces_original_deadline_and_caps_sql_startup_budget() {
    let (db, attempt, plan, evidence, now) = fixture().await;
    for (registered, expires) in [
        (now, now - 1),
        (now, attempt.intent().deadline_at_ms + 1),
        (attempt.intent().started_at_ms - 1, now + 1000),
    ] {
        assert!(
            insert(&db, &attempt, &plan, &evidence, registered, expires)
                .await
                .is_err()
        );
    }
    insert(&db, &attempt, &plan, &evidence, now, now + 5000)
        .await
        .unwrap();
    let error=sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) SELECT organization,execution_id,'pod-one',$1,'{\"version\":1,\"lease_budget_ms\":10000}'::jsonb,$2,floor(extract(epoch from clock_timestamp())*1000) FROM execution_requests")
        .bind(serde_json::to_value(pods::challenge(&attempt)).unwrap()).bind(format!("sha256:{}","a".repeat(64))).execute(&db.pool).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("execution watchdog is not armed")
    );
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
}

#[tokio::test]
async fn migration_sixteen_retains_history_without_inventing_a_watchdog() {
    let (db, attempt, _, _, _) = fixture().await;
    db.remove_execution_watchdogs().await;
    let id = &attempt.intent().execution.execution_id;
    let grant = db
        .store
        .authorize_candidate_execution_startup(
            &org("acme"),
            id,
            2,
            "pod-one",
            &pods::challenge(&attempt),
        )
        .await;
    // Current code still requires a live guard even while an old schema lacks the table.
    assert!(grant.is_err());
    assert!(db.store.ready().await.is_err());
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "execution_watchdog_arms").await, 0);
    assert_eq!(count(&db, "execution_pod_observations").await, 1);
    assert!(
        db.store
            .candidate_execution_watchdog(&org("acme"), id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        db.store
            .authorize_candidate_execution_startup(
                &org("acme"),
                id,
                2,
                "pod-one",
                &pods::challenge(&attempt)
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
}
