//! Synthetic SQL records exercise metadata constraints, never a live node proof.
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
mod outputs;

async fn fixture() -> (
    Database,
    ExecutionDispatchAttempt,
    ExecutionPodPlan,
    Value,
    i64,
) {
    fixture_with_fence(false).await
}

async fn fixture_with_fence(
    fenced: bool,
) -> (
    Database,
    ExecutionDispatchAttempt,
    ExecutionPodPlan,
    Value,
    i64,
) {
    let (db, attempt, plan, evidence, now, _) = fixture_with_token(fenced).await;
    (db, attempt, plan, evidence, now)
}
async fn fixture_with_token(
    fenced: bool,
) -> (
    Database,
    ExecutionDispatchAttempt,
    ExecutionPodPlan,
    Value,
    i64,
    String,
) {
    let (db, token, computer, acquire_input) = setup().await;
    let lease = acquire(&db, &token, &computer, &acquire_input).await;
    let input = submission(&db, &lease).await;
    let queued = submit(&db, &token, &computer, &input).await;
    let attempt = begin(&db, &queued).await;
    let mount = json!({"version":1,"instance":"c".repeat(64),"prepared":attempt.intent().binding["prepared"],"boot_id":"boot-one","inode":1});
    let plan = if fenced {
        // Serialized routing metadata only. This never constructs a node guard.
        let mut manifest = pods::manifest(&attempt);
        let mut binding: Value = serde_json::from_str(
            manifest["metadata"]["annotations"]["agent-computer.io/binding"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        binding["workspace"]["fence"] =
            json!({"mount":mount,"node":{"uid":"node-one","boot_id":"boot-one"}});
        manifest["metadata"]["annotations"]["agent-computer.io/binding"] =
            json!(binding.to_string());
        db.store
            .register_candidate_execution_pod(&attempt, "namespace-uid", &manifest)
            .await
            .unwrap()
    } else {
        pods::register(&db, &attempt).await
    };
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
    let mut evidence = json!({"runtime":{"identity":{"pod_uid":"pod-one","node":{"uid":"node-one","boot_id":"boot-one"},"container_id":"a".repeat(64)},"cgroup_inode":123,"cgroup_path":"fixture-only"},"armed":{"version":1,"event":"armed","request":{"execution_id":queued.execution_id,"boot_id":"boot-one","cgroup_inode":123,"cgroup_path":"fixture-only"}}});
    evidence["version"] = json!(2);
    if fenced {
        evidence["runtime"]["workspace_mount"] = mount;
    }
    evidence["armed"]["request"]["deadline_boottime_ms"] = json!(30000);
    evidence["armed"]["cgroup_device"] = json!(42);
    evidence["armed"]["armed_boottime_ms"] = json!(1000);
    evidence["backup_armed"] = evidence["armed"].clone();
    evidence["backup_armed"]["armed_boottime_ms"] = json!(1100);
    evidence["observed_boottime_ms"] = json!(1200);
    evidence["watchdog_pids"] = json!([100, 101]);
    let first = json!({"id":"journal-primary","device":7,"inode":101,"intent_digest":format!("sha256:{}","a".repeat(64))});
    let second = json!({"id":"journal-backup","device":7,"inode":102,"intent_digest":format!("sha256:{}","b".repeat(64))});
    evidence["armed"]["journal"] = first.clone();
    evidence["backup_armed"]["journal"] = second.clone();
    evidence["reaper"] = json!({"version":1,"instance":"a".repeat(64),"nonce":"b".repeat(64),"request":evidence["armed"]["request"],"journals":[first,second],"cgroup_device":42,"pid":102,"spool_device":7,"spool_inode":8,"observed_boottime_ms":1190});
    (db, attempt, plan, evidence, now, token)
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

#[tokio::test]
async fn redundant_watchdogs_require_matching_timers_and_distinct_live_process_evidence() {
    let (db, attempt, plan, evidence, now) = fixture().await;
    for (pointer, value) in [
        ("/version", json!(1)),
        ("/backup_armed", Value::Null),
        ("/backup_armed/event", json!("empty_observed")),
        ("/backup_armed/request/deadline_boottime_ms", json!(30001)),
        ("/backup_armed/request/cgroup_inode", json!(124)),
        ("/backup_armed/cgroup_device", json!(43)),
        ("/backup_armed/armed_boottime_ms", json!(1201)),
        ("/armed/armed_boottime_ms", Value::Null),
        ("/observed_boottime_ms", json!(30000)),
        ("/observed_boottime_ms", Value::Null),
        ("/watchdog_pids", json!([100, 100])),
        ("/watchdog_pids", json!([100, "100"])),
        ("/watchdog_pids", json!([100, 4294967296u64])),
        ("/watchdog_pids", json!([100, 0])),
        ("/watchdog_pids", json!([100])),
        ("/watchdog_pids", Value::Null),
    ] {
        let mut bad = evidence.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            insert(&db, &attempt, &plan, &bad, now, now + 5000)
                .await
                .is_err(),
            "{pointer}"
        );
    }
    assert_eq!(count(&db, "execution_watchdog_arms").await, 0);
    insert(&db, &attempt, &plan, &evidence, now, now + 5000)
        .await
        .unwrap();
}

#[tokio::test]
async fn migration_seventeen_preserves_single_guard_history_but_denies_new_grants() {
    let (db, attempt, plan, mut evidence, now) = fixture().await;
    db.remove_redundant_watchdogs().await;
    for field in ["version", "backup_armed", "watchdog_pids", "reaper"] {
        evidence.as_object_mut().unwrap().remove(field);
    }
    insert(&db, &attempt, &plan, &evidence, now, now + 5000)
        .await
        .unwrap();
    let id = &attempt.intent().execution.execution_id;
    let before = db
        .store
        .candidate_execution_watchdog(&org("acme"), id)
        .await
        .unwrap()
        .unwrap();
    assert!(db.store.ready().await.is_err());
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let after = db
        .store
        .candidate_execution_watchdog(&org("acme"), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.evidence_digest, after.evidence_digest);
    assert_eq!(before.evidence, after.evidence);
    let error=sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) SELECT organization,execution_id,'pod-one',$1,'{\"version\":1,\"lease_budget_ms\":1000}'::jsonb,$2,floor(extract(epoch from clock_timestamp())*1000) FROM execution_requests")
        .bind(serde_json::to_value(pods::challenge(&attempt)).unwrap()).bind(format!("sha256:{}","a".repeat(64))).execute(&db.pool).await.unwrap_err();
    assert!(error.to_string().contains("watchdog is not armed"));
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn reaper_metadata_requires_fresh_exact_journal_and_challenge_bindings() {
    let (db, attempt, plan, evidence, now) = fixture().await;
    for (pointer, value) in [
        ("/reaper", Value::Null),
        ("/reaper/version", json!(2)),
        ("/reaper/instance", json!("bad")),
        ("/reaper/nonce", json!("A".repeat(64))),
        ("/reaper/pid", json!(0)),
        ("/reaper/spool_device", json!(0)),
        ("/reaper/spool_inode", json!(0)),
        ("/reaper/request/execution_id", json!("other")),
        ("/reaper/cgroup_device", json!(43)),
        ("/reaper/journals/1", evidence["armed"]["journal"].clone()),
        ("/reaper/journals/0/intent_digest", json!("wrong")),
        ("/reaper/journals", json!([])),
        ("/reaper/observed_boottime_ms", json!(999)),
        ("/reaper/observed_boottime_ms", json!(1201)),
        ("/armed/journal", Value::Null),
    ] {
        let mut bad = evidence.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            insert(&db, &attempt, &plan, &bad, now, now + 5000)
                .await
                .is_err(),
            "{pointer}"
        );
    }
    assert_eq!(count(&db, "execution_watchdog_arms").await, 0);
    insert(&db, &attempt, &plan, &evidence, now, now + 5000)
        .await
        .unwrap();
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
}

#[tokio::test]
async fn migration_eighteen_preserves_arms_without_reaper_but_denies_new_startup() {
    let (db, attempt, plan, mut evidence, now) = fixture().await;
    db.remove_reaper_admission().await;
    evidence.as_object_mut().unwrap().remove("reaper");
    evidence["armed"].as_object_mut().unwrap().remove("journal");
    evidence["backup_armed"]
        .as_object_mut()
        .unwrap()
        .remove("journal");
    insert(&db, &attempt, &plan, &evidence, now, now + 5000)
        .await
        .unwrap();
    let id = &attempt.intent().execution.execution_id;
    let before = db
        .store
        .candidate_execution_watchdog(&org("acme"), id)
        .await
        .unwrap()
        .unwrap();
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let after = db
        .store
        .candidate_execution_watchdog(&org("acme"), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.evidence_digest, after.evidence_digest);
    assert_eq!(before.evidence, after.evidence);
    let error=sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) SELECT organization,execution_id,'pod-one',$1,'{\"version\":1,\"lease_budget_ms\":1000}'::jsonb,$2,floor(extract(epoch from clock_timestamp())*1000) FROM execution_requests")
        .bind(serde_json::to_value(pods::challenge(&attempt)).unwrap()).bind(format!("sha256:{}","a".repeat(64))).execute(&db.pool).await.unwrap_err();
    assert!(error.to_string().contains("reaper watchdog is not armed"));
    assert_eq!(count(&db, "execution_startup_grants").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}
