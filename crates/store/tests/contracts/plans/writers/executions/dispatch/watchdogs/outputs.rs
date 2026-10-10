//! Synthetic SQL fixtures check bindings only; no S3 receipt or live guard is invented.
use super::*;

async fn output_fixture() -> (Database, ExecutionDispatchAttempt, Value, i64) {
    let (db, attempt, plan, evidence, now) = fixture().await;
    insert(&db, &attempt, &plan, &evidence, now, now + 10000)
        .await
        .unwrap();
    let d = attempt.intent();
    let grant = format!("sha256:{}", "d".repeat(64));
    sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) VALUES ('acme',$1,'pod-one',$2,'{\"version\":1,\"lease_budget_ms\":1000}'::jsonb,$3,$4)")
        .bind(&d.execution.execution_id).bind(serde_json::to_value(pods::challenge(&attempt)).unwrap()).bind(&grant).bind(now).execute(&db.pool).await.unwrap();
    let arm: String = sqlx::query_scalar("SELECT evidence_digest FROM execution_watchdog_arms")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let hash = format!("sha256:{:x}", Sha256::digest(b""));
    let object = json!({"store_digest":format!("sha256:{}","f".repeat(64)),"key":format!("execution-outputs/v1/acme/{}/{}",d.execution.execution_id,&hash[7..]),"sha256":hash,"size":0});
    let stream =
        json!({"sha256":hash,"retained_bytes":0,"observed_bytes":0,"truncated":false,"eof":true});
    let manifest = json!({"version":1,"organization":"acme","execution_id":d.execution.execution_id,"pod_uid":"pod-one","dispatch_digest":d.intent_digest,"grant_digest":grant,"arm_digest":arm,"objects":[object.clone(),object.clone(),object.clone(),object],"summary":{"observed_outcome":"succeeded","stdout":stream.clone(),"stderr":stream,"supervisor_stderr_bytes":0}});
    (db, attempt, manifest, now)
}
fn hash(manifest: &Value) -> String {
    // The storage reader canonicalizes into its typed manifest; JSON order is
    // intentionally not used as evidence of a verified output in this SQL suite.
    let mut hash = Sha256::new();
    hash.update(b"fixture-output-metadata\0");
    hash.update(serde_json::to_vec(manifest).unwrap());
    format!("sha256:{:x}", hash.finalize())
}
async fn intent(
    db: &Database,
    original: &Value,
    manifest: &Value,
    at: i64,
) -> std::result::Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query("INSERT INTO execution_output_intents (organization,execution_id,pod_uid,dispatch_digest,grant_digest,arm_digest,manifest,manifest_digest,created_at_ms) VALUES ('acme',$1,'pod-one',$2,$3,$4,$5,$6,$7)")
        .bind(original["execution_id"].as_str().unwrap()).bind(original["dispatch_digest"].as_str().unwrap()).bind(original["grant_digest"].as_str().unwrap()).bind(original["arm_digest"].as_str().unwrap()).bind(manifest).bind(hash(manifest)).bind(at).execute(&db.pool).await
}
async fn publish(
    db: &Database,
    m: &Value,
    digest: &str,
    at: i64,
) -> std::result::Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query("INSERT INTO execution_outputs (organization,execution_id,manifest_digest,verified_at_ms) VALUES ('acme',$1,$2,$3)").bind(m["execution_id"].as_str().unwrap()).bind(digest).bind(at).execute(&db.pool).await
}

#[tokio::test]
async fn output_intent_rejects_cross_execution_store_pod_and_grant_references() {
    let (db, _, m, now) = output_fixture().await;
    for (pointer, value) in [
        ("/organization", json!("foreign")),
        ("/execution_id", json!("foreign")),
        ("/pod_uid", json!("foreign")),
        (
            "/dispatch_digest",
            json!(format!("sha256:{}", "0".repeat(64))),
        ),
        ("/grant_digest", json!(format!("sha256:{}", "0".repeat(64)))),
        ("/arm_digest", json!(format!("sha256:{}", "0".repeat(64)))),
        (
            "/objects/0/key",
            json!("execution-outputs/v1/foreign/foreign/object"),
        ),
        ("/objects/0/sha256", json!("invalid")),
        (
            "/objects/0/store_digest",
            json!(format!("sha256:{}", "0".repeat(64))),
        ),
        ("/objects/0/size", json!(8404993)),
        ("/objects", json!([])),
    ] {
        let mut bad = m.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(intent(&db, &m, &bad, now).await.is_err(), "{pointer}");
    }
    let mut foreign = m.clone();
    foreign["grant_digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    assert!(intent(&db, &foreign, &foreign, now).await.is_err());
    intent(&db, &m, &m, now).await.unwrap();
    assert!(intent(&db, &m, &m, now).await.is_err());
    assert_eq!(count(&db, "execution_outputs").await, 0);
}

#[tokio::test]
async fn output_publication_is_immutable_survives_wal_and_never_completes_execution() {
    let (mut db, attempt, m, now) = output_fixture().await;
    intent(&db, &m, &m, now).await.unwrap();
    assert!(
        publish(&db, &m, &format!("sha256:{}", "0".repeat(64)), now)
            .await
            .is_err()
    );
    assert!(publish(&db, &m, &hash(&m), now - 1).await.is_err());
    publish(&db, &m, &hash(&m), now).await.unwrap();
    for query in [
        "UPDATE execution_outputs SET verified_at_ms=verified_at_ms+1",
        "DELETE FROM execution_outputs",
        "UPDATE execution_output_intents SET pod_uid='foreign'",
        "DELETE FROM execution_output_intents",
    ] {
        assert!(sqlx::query(query).execute(&db.pool).await.is_err());
    }
    db.crash_and_restart().await;
    assert_eq!(count(&db, "execution_outputs").await, 1);
    assert_eq!(count(&db, "candidate_writer_completions").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    let state: String = sqlx::query_scalar("SELECT state FROM execution_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "Dispatching");
    assert!(matches!(
        db.store
            .begin_candidate_execution_dispatch(
                &org("acme"),
                &attempt.intent().execution.execution_id,
                1
            )
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
}

#[tokio::test]
async fn migration_nineteen_does_not_infer_outputs_from_historical_grants() {
    let (db, _, _, _) = output_fixture().await;
    db.remove_execution_outputs().await;
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "execution_startup_grants").await, 1);
    assert_eq!(count(&db, "execution_output_intents").await, 0);
    assert_eq!(count(&db, "execution_outputs").await, 0);
}
