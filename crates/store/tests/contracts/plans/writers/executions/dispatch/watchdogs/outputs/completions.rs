//! SQL metadata/transaction contracts only. Real opaque node seals are exercised
//! by the disposable VM test; none can be minted from these fixtures.
use super::*;
use sqlx::{Postgres, Transaction};

#[tokio::test]
async fn drain_recovery_discovery_is_expired_scoped_cursor_bound_and_read_only() {
    use agent_computer_kubernetes::NodeIdentity;
    use agent_computer_store::runtime::preparation::PreparationTarget;
    let (mut db, attempt, _, _) = output_fixture_with_fence(true).await;
    let id = &attempt.intent().execution.execution_id;
    let target: PreparationTarget =
        serde_json::from_value(attempt.intent().binding["storage_target"].clone()).unwrap();
    let node = NodeIdentity {
        name: "node-one".into(),
        uid: "node-one".into(),
        boot_id: "later-boot".into(),
    };
    let organization = org("acme");
    assert!(
        db.store
            .candidate_execution_completion_queue(&organization, &target, &node, None)
            .await
            .unwrap()
            .is_empty()
    );
    // Wait for the real pinned deadline; do not rewrite immutable dispatch data
    // or simulate expiration by disabling the database's guards.
    sqlx::query("SELECT pg_sleep(GREATEST(0,($1::bigint-floor(extract(epoch from clock_timestamp())*1000)::bigint)::double precision/1000.0)+0.01)")
        .bind(attempt.intent().deadline_at_ms).execute(&db.pool).await.unwrap();
    let before = count(&db, "events").await;
    assert_eq!(
        db.store
            .candidate_execution_completion_queue(&organization, &target, &node, None)
            .await
            .unwrap(),
        vec![id.clone()]
    );
    assert!(
        db.store
            .candidate_execution_completion_queue(&organization, &target, &node, Some(id))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.store
            .candidate_execution_completion_queue(&org("other"), &target, &node, None)
            .await
            .unwrap()
            .is_empty()
    );
    for field in [
        "volume_id",
        "namespace_uid",
        "pvc_uid",
        "pv_uid",
        "filesystem_uuid",
        "volume_path",
        "writer_uid",
        "writer_gid",
    ] {
        let mut other = serde_json::to_value(&target).unwrap();
        other[field] = if field.starts_with("writer_") {
            json!(12345)
        } else {
            json!("foreign")
        };
        let other = serde_json::from_value(other).unwrap();
        assert!(
            db.store
                .candidate_execution_completion_queue(&organization, &other, &node, None)
                .await
                .unwrap()
                .is_empty(),
            "{field}"
        );
    }
    for field in ["name", "uid"] {
        let mut other = serde_json::to_value(&node).unwrap();
        other[field] = json!("foreign");
        let other = serde_json::from_value(other).unwrap();
        assert!(
            db.store
                .candidate_execution_completion_queue(&organization, &target, &other, None)
                .await
                .unwrap()
                .is_empty(),
            "{field}"
        );
    }
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "execution_completions").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    db.store
        .mark_candidate_execution_unknown(&organization, id, 2)
        .await
        .unwrap();
    db.crash_and_restart().await;
    assert_eq!(
        db.store
            .candidate_execution_completion_queue(&organization, &target, &node, None)
            .await
            .unwrap(),
        vec![id.clone()]
    );
    let mut tx = db.pool.begin().await.unwrap();
    insert_completion(&mut tx, &seal(&db).await, "Unknown", None)
        .await
        .unwrap();
    transition(&mut tx, "Unknown").await;
    tx.commit().await.unwrap();
    assert!(
        db.store
            .candidate_execution_completion_queue(&organization, &target, &node, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!artifact_ready(&db).await);
}

async fn seal(db: &Database) -> Value {
    let arm: Value = sqlx::query_scalar("SELECT evidence FROM execution_watchdog_arms")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    json!({"version":1,"arm":arm,"io":{"version":1,"instance":arm["runtime"]["workspace_mount"]["instance"],"prepared":arm["runtime"]["workspace_mount"]["prepared"],"accepted_mutating_requests":3},"domain":"empty","observed_boottime_ms":1500})
}
async fn insert_completion(
    tx: &mut Transaction<'_, Postgres>,
    seal: &Value,
    state: &str,
    output: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO execution_completions (organization,execution_id,lease_id,epoch,dispatch_digest,arm_digest,seal,seal_digest,output_manifest_digest,accepted_state,completed_at_ms) SELECT r.organization,r.execution_id,r.lease_id,r.epoch,d.intent_digest,w.evidence_digest,$1,$2,$3,$4,floor(extract(epoch from clock_timestamp())*1000) FROM execution_requests r JOIN execution_dispatch_intents d USING(organization,execution_id) JOIN execution_watchdog_arms w USING(organization,execution_id)")
        .bind(seal).bind(format!("sha256:{}","e".repeat(64))).bind(output).bind(state).execute(&mut **tx).await?;
    Ok(())
}
async fn transition(tx: &mut Transaction<'_, Postgres>, state: &str) {
    let reason = match state {
        "Succeeded" | "Failed" => "completed",
        "Cancelled" => "completed_cancelled",
        _ => "completion_unconfirmed",
    };
    sqlx::query("UPDATE execution_requests SET state=$1,reason=$2,revision=revision+1 WHERE state<>'Unknown'").bind(state).bind(reason).execute(&mut **tx).await.unwrap();
    sqlx::query("UPDATE candidate_writer_leases SET state='Draining',revision=revision+1 WHERE state='Held'").execute(&mut **tx).await.unwrap();
    sqlx::query("INSERT INTO candidate_writer_drains (organization,lease_id,epoch,proof) SELECT organization,lease_id,epoch,'execution_drained' FROM candidate_writer_leases").execute(&mut **tx).await.unwrap();
    sqlx::query("UPDATE candidate_writer_leases SET state='Released',revision=revision+1")
        .execute(&mut **tx)
        .await
        .unwrap();
}
async fn artifact_ready(db: &Database) -> bool {
    sqlx::query_scalar(
        "SELECT artifact_candidate_drained(organization,request_id) FROM candidate_writer_leases",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn completion_rejects_transplants_and_cannot_replace_a_live_seal_with_output_metadata() {
    let (db, _, m, now) = output_fixture_with_fence(true).await;
    intent(&db, &m, &m, now).await.unwrap();
    publish(&db, &m, &hash(&m), now).await.unwrap();
    let original = seal(&db).await;
    for (pointer, value) in [
        ("/version", json!(2)),
        ("/arm/runtime/identity/pod_uid", json!("foreign")),
        ("/arm/runtime/identity/node/boot_id", json!("foreign")),
        ("/domain", json!("path_absent")),
        ("/io/version", json!(2)),
        ("/io/instance", json!("d".repeat(64))),
        ("/io/prepared/data_inode", json!(42)),
        ("/io/accepted_mutating_requests", json!(-1)),
        ("/observed_boottime_ms", json!(1199)),
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        let mut tx = db.pool.begin().await.unwrap();
        assert!(
            insert_completion(&mut tx, &bad, "Succeeded", Some(&hash(&m)))
                .await
                .is_err(),
            "{pointer}"
        );
        tx.rollback().await.unwrap();
    }
    for sql in [
        "UPDATE execution_requests SET state='Succeeded',reason='completed',revision=revision+1",
        "INSERT INTO candidate_writer_drains (organization,lease_id,epoch,proof) SELECT organization,lease_id,epoch,'execution_drained' FROM candidate_writer_leases",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
    let mut tx = db.pool.begin().await.unwrap();
    assert!(
        insert_completion(&mut tx, &original, "Succeeded", None)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    assert_eq!(count(&db, "execution_completions").await, 0);
    assert!(!artifact_ready(&db).await);
}

#[tokio::test]
async fn completion_requires_atomic_state_and_writer_drain_and_survives_wal() {
    let (mut db, attempt, m, now) = output_fixture_with_fence(true).await;
    intent(&db, &m, &m, now).await.unwrap();
    publish(&db, &m, &hash(&m), now).await.unwrap();
    let seal = seal(&db).await;
    let mut tx = db.pool.begin().await.unwrap();
    insert_completion(&mut tx, &seal, "Succeeded", Some(&hash(&m)))
        .await
        .unwrap();
    assert!(
        tx.commit()
            .await
            .unwrap_err()
            .to_string()
            .contains("transaction is incomplete")
    );
    assert_eq!(count(&db, "execution_completions").await, 0);
    let mut tx = db.pool.begin().await.unwrap();
    insert_completion(&mut tx, &seal, "Succeeded", Some(&hash(&m)))
        .await
        .unwrap();
    transition(&mut tx, "Succeeded").await;
    tx.commit().await.unwrap();
    assert!(artifact_ready(&db).await);
    for sql in [
        "UPDATE execution_completions SET accepted_state='Unknown'",
        "DELETE FROM execution_completions",
        "UPDATE execution_requests SET state='Unknown',reason='dispatch_unconfirmed',revision=revision+1",
    ] {
        assert!(sqlx::query(sql).execute(&db.pool).await.is_err());
    }
    let id = &attempt.intent().execution.execution_id;
    let receipt = db
        .store
        .candidate_execution_completion(&org("acme"), id)
        .await
        .unwrap()
        .unwrap();
    db.crash_and_restart().await;
    assert_eq!(
        db.store
            .candidate_execution_completion(&org("acme"), id)
            .await
            .unwrap()
            .unwrap(),
        receipt
    );
    let state = db
        .store
        .reconcile_candidate_execution(&org("acme"), id)
        .await
        .unwrap();
    assert_eq!(state.state, ExecutionState::Succeeded);
    assert!(state.dispatch_started);
    assert!(artifact_ready(&db).await);
    assert_eq!(count(&db, "candidate_writer_completions").await, 0);
    let original = db
        .store
        .candidate_execution_runtime_inputs(&org("acme"), id)
        .await
        .unwrap();
    sqlx::raw_sql("UPDATE candidate_writer_leases SET state='Held',epoch=epoch+1,revision=revision+1,expires_at_ms=floor(extract(epoch from clock_timestamp())*1000)+30000; INSERT INTO candidate_writer_epochs(organization,lease_id,epoch,session_id,created_at_ms) SELECT organization,lease_id,epoch,session_id,floor(extract(epoch from clock_timestamp())*1000) FROM candidate_writer_leases;").execute(&db.pool).await.unwrap();
    let old = db
        .store
        .candidate_execution_runtime_inputs(&org("acme"), id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(old).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(
        db.store
            .candidate_execution_completion(&org("acme"), id)
            .await
            .unwrap()
            .unwrap(),
        receipt
    );
    assert!(!artifact_ready(&db).await);
}

#[tokio::test]
async fn unknown_seal_releases_physical_writer_but_preserves_result_and_blocks_artifacts() {
    let (db, attempt, _, _) = output_fixture_with_fence(true).await;
    let id = &attempt.intent().execution.execution_id;
    let before = db
        .store
        .mark_candidate_execution_unknown(&org("acme"), id, 2)
        .await
        .unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    insert_completion(&mut tx, &seal(&db).await, "Unknown", None)
        .await
        .unwrap();
    transition(&mut tx, "Unknown").await;
    tx.commit().await.unwrap();
    let after = db
        .store
        .reconcile_candidate_execution(&org("acme"), id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert!(!artifact_ready(&db).await);
    assert_eq!(
        db.store
            .reconcile_candidate_writer(&org("acme"), &attempt.intent().execution.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Released
    );
}

#[tokio::test]
async fn completion_accepts_failed_report_or_explicit_cancel_but_not_success_without_verified_output()
 {
    for state in ["Failed", "Cancelled"] {
        let (db, _, mut m, now) = output_fixture_with_fence(true).await;
        let digest = if state == "Failed" {
            m["summary"]["observed_outcome"] = json!("timed_out");
            intent(&db, &m, &m, now).await.unwrap();
            let mut tx = db.pool.begin().await.unwrap();
            assert!(
                insert_completion(&mut tx, &seal(&db).await, "Failed", Some(&hash(&m)))
                    .await
                    .is_err()
            );
            tx.rollback().await.unwrap();
            publish(&db, &m, &hash(&m), now).await.unwrap();
            Some(hash(&m))
        } else {
            let mut tx = db.pool.begin().await.unwrap();
            assert!(
                insert_completion(&mut tx, &seal(&db).await, "Cancelled", None)
                    .await
                    .is_err()
            );
            tx.rollback().await.unwrap();
            sqlx::query("UPDATE execution_requests SET state='CancelRequested',reason='user_requested',revision=revision+1").execute(&db.pool).await.unwrap();
            None
        };
        let mut tx = db.pool.begin().await.unwrap();
        insert_completion(&mut tx, &seal(&db).await, state, digest.as_deref())
            .await
            .unwrap();
        transition(&mut tx, state).await;
        tx.commit().await.unwrap();
        assert!(artifact_ready(&db).await);
    }
}

#[tokio::test]
async fn legacy_mount_metadata_and_historical_grants_cannot_be_promoted_by_migration() {
    let (db, _, _, _) = output_fixture().await;
    let mut fake = seal(&db).await;
    fake["io"]["instance"] = json!("c".repeat(64));
    let mut tx = db.pool.begin().await.unwrap();
    assert!(
        insert_completion(&mut tx, &fake, "Unknown", None)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    db.remove_execution_completions().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "execution_startup_grants").await, 1);
    assert_eq!(count(&db, "execution_completions").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn deadline_crossed_during_commit_rolls_back_the_whole_completion() {
    let (db, _, m, now) = output_fixture_with_fence(true).await;
    intent(&db, &m, &m, now).await.unwrap();
    publish(&db, &m, &hash(&m), now).await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    insert_completion(&mut tx, &seal(&db).await, "Succeeded", Some(&hash(&m)))
        .await
        .unwrap();
    transition(&mut tx, "Succeeded").await;
    // Use the actual immutable admission deadline, not a forged clock or a
    // disabled trigger. Draining must roll back with the rejected completion.
    sqlx::query("SELECT pg_sleep(GREATEST(0,(deadline_at_ms-floor(extract(epoch from clock_timestamp())*1000)+20)/1000.0)) FROM execution_dispatch_intents")
        .execute(&mut *tx).await.unwrap();
    assert!(
        tx.commit()
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline elapsed")
    );
    assert_eq!(count(&db, "execution_completions").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    let state: String = sqlx::query_scalar("SELECT state FROM execution_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "Dispatching");
}
