//! SQL fixtures prove durable metadata and read authority, not a live runtime.
use super::downloads::Http;
use super::*;
use agent_computer_objects::{Client, Spool};
use agent_computer_sandbox::{
    StartupGrant,
    streaming::{Chunk, Stream},
};
use std::{sync::atomic::Ordering, time::Duration};

struct Fixture {
    db: Database,
    token: String,
    id: String,
    template: Value,
    now: i64,
}
impl Fixture {
    async fn new(http: &Http, renewable: bool) -> Self {
        let (db, attempt, plan, arm, now, token) =
            fixture_with_output_policy(false, renewable, None).await;
        insert(&db, &attempt, &plan, &arm, now, now + 10000)
            .await
            .unwrap();
        assert_eq!(attempt.intent().bootstrap().unwrap().version, 3);
        let challenge = pods::challenge(&attempt);
        let grant = StartupGrant {
            version: 3,
            challenge_digest: challenge.digest().unwrap(),
            lease_budget_ms: 1000,
            hard_budget_ms: renewable.then_some(2000),
        };
        let startup = grant.digest().unwrap();
        let id = attempt.intent().execution.execution_id.clone();
        sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) VALUES ('acme',$1,'pod-one',$2,$3,$4,$5)")
            .bind(&id).bind(serde_json::to_value(challenge).unwrap()).bind(serde_json::to_value(grant).unwrap()).bind(&startup).bind(now).execute(&db.pool).await.unwrap();
        let arm: String = sqlx::query_scalar("SELECT evidence_digest FROM execution_watchdog_arms")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let template = json!({"version":1,"organization":"acme","execution_id":id,"pod_uid":"pod-one","dispatch_digest":attempt.intent().intent_digest,"grant_digest":startup,"arm_digest":arm});
        // No producer capability is manufactured from this stored observation.
        assert!(attempt.output_capture().unwrap().is_some());
        let _ = http.client();
        Self {
            db,
            token,
            id,
            template,
            now,
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn chunk(
        &self,
        client: &Client,
        sequence: u32,
        stream: Stream,
        offset: u64,
        previous: &str,
        bytes: &[u8],
        eof: bool,
    ) -> Value {
        let chunk = Chunk {
            version: 1,
            startup_grant_digest: self.template["grant_digest"].as_str().unwrap().into(),
            sequence,
            previous_digest: previous.into(),
            stream,
            offset,
            bytes: bytes.to_vec(),
            observed_bytes: offset + bytes.len() as u64,
            truncated: false,
            eof,
        };
        let mut m = self.template.clone();
        let wire = serde_json::to_value(&chunk).unwrap();
        for key in [
            "sequence",
            "previous_digest",
            "stream",
            "offset",
            "observed_bytes",
            "truncated",
            "eof",
        ] {
            m[key] = wire[key].clone();
        }
        m["chunk_digest"] = json!(chunk.digest().unwrap());
        m["object"] =
            serde_json::to_value(client.reference("acme", &self.id, bytes).unwrap()).unwrap();
        m
    }
    fn first(&self, http: &Http, eof: bool) -> Value {
        self.chunk(
            &http.client(),
            1,
            Stream::Stdout,
            0,
            self.template["grant_digest"].as_str().unwrap(),
            &http.response.lock().unwrap().1,
            eof,
        )
    }
    async fn put(&self, m: &Value) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO execution_output_chunk_intents (organization,execution_id,sequence,manifest,manifest_digest,created_at_ms) VALUES ('acme',$1,$2,$3,$4,$5)")
            .bind(&self.id).bind(m["sequence"].as_i64().unwrap() as i32).bind(m).bind(digest(m)).bind(self.now).execute(&self.db.pool).await.map(|_|())
    }
    async fn ack(&self, m: &Value) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO execution_output_chunks (organization,execution_id,sequence,manifest_digest,verified_at_ms) VALUES ('acme',$1,$2,$3,$4)")
            .bind(&self.id).bind(m["sequence"].as_i64().unwrap() as i32).bind(digest(m)).bind(self.now).execute(&self.db.pool).await.map(|_|())
    }
    async fn page(&self, after: u32, limit: u32) -> ExecutionChunkPage {
        self.db
            .store
            .candidate_execution_chunks(&self.token, &self.id, after, limit)
            .await
            .unwrap()
    }
}
fn digest(m: &Value) -> String {
    let mut hash = Sha256::new();
    hash.update(b"agent-computer/execution-output-chunk-manifest-v1\0");
    hash.update(serde_json::to_vec(m).unwrap());
    format!("sha256:{:x}", hash.finalize())
}

#[tokio::test]
async fn final_reports_bind_verified_prefix_and_cannot_erase_eof_or_allow_late_chunks() {
    let http = Http::new();
    let f = Fixture::new(&http, false).await;
    let first = f.first(&http, true);
    f.put(&first).await.unwrap();
    let empty = serde_json::to_value(http.client().reference("acme", &f.id, b"").unwrap()).unwrap();
    let summary = |o: &Value, eof| json!({"sha256":o["sha256"],"retained_bytes":o["size"],"observed_bytes":o["size"],"truncated":false,"eof":eof});
    let mut m = f.template.clone();
    m["version"] = json!(3);
    m["objects"] = json!([empty, first["object"], empty, empty]);
    m["stream"] = json!({"sequence":1,"last_digest":first["chunk_digest"]});
    m["summary"] = json!({"observed_outcome":"unknown","stdout":summary(&first["object"],true),"stderr":summary(&empty,false),"supervisor_stderr_bytes":0});
    assert!(intent(&f.db, &m, &m, f.now).await.is_err()); // pending chunk is not a report prefix
    f.ack(&first).await.unwrap();
    for (pointer, value) in [
        ("/stream/sequence", json!(0)),
        ("/stream/last_digest", f.template["grant_digest"].clone()),
        ("/summary/observed_outcome", json!("succeeded")),
        ("/summary/stdout/eof", json!(false)),
        ("/summary/stdout/observed_bytes", json!(10)),
    ] {
        let mut bad = m.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(intent(&f.db, &m, &bad, f.now).await.is_err(), "{pointer}");
    }
    // Use the real canonical digest for a readable synthetic metadata record.
    let mut h = Sha256::new();
    h.update(b"agent-computer/execution-outputs-v1\0");
    h.update(serde_json::to_vec(&m).unwrap());
    let hash = format!("sha256:{:x}", h.finalize());
    sqlx::query("INSERT INTO execution_output_intents (organization,execution_id,pod_uid,dispatch_digest,grant_digest,arm_digest,manifest,manifest_digest,created_at_ms) VALUES ('acme',$1,'pod-one',$2,$3,$4,$5,$6,$7)")
        .bind(&f.id).bind(m["dispatch_digest"].as_str().unwrap()).bind(m["grant_digest"].as_str().unwrap()).bind(m["arm_digest"].as_str().unwrap()).bind(&m).bind(&hash).bind(f.now).execute(&f.db.pool).await.unwrap();
    let page = f.page(0, 32).await;
    assert_eq!(page.final_sequence, Some(1));
    assert!(!page.final_report_verified);
    let late = f.chunk(
        &http.client(),
        2,
        Stream::Stderr,
        0,
        first["chunk_digest"].as_str().unwrap(),
        b"",
        true,
    );
    assert!(f.put(&late).await.is_err());
    publish(&f.db, &m, &hash, f.now).await.unwrap();
    let page = f.page(0, 32).await;
    assert!(page.final_report_verified);
    assert_eq!(page.execution_state, ExecutionState::Dispatching);
}

#[tokio::test]
async fn chunk_intents_require_exact_identity_contiguous_acks_offsets_and_stream_bounds() {
    let http = Http::new();
    for renewable in [false, true] {
        let f = Fixture::new(&http, renewable).await;
        let first = f.first(&http, false);
        for (key, value) in [
            ("organization", json!("foreign")),
            ("execution_id", json!("foreign")),
            ("pod_uid", json!("foreign")),
            ("sequence", json!(2)),
            (
                "previous_digest",
                json!(format!("sha256:{}", "0".repeat(64))),
            ),
            ("offset", json!(1)),
            ("truncated", json!(true)),
            ("observed_bytes", json!(0)),
            ("stream", json!("report")),
            ("eof", json!("false")),
            ("bytes", json!("must-never-enter-db")),
        ] {
            let mut bad = first.clone();
            bad[key] = value;
            assert!(f.put(&bad).await.is_err(), "{key}");
        }
        f.put(&first).await.unwrap();
        let second = f.chunk(
            &http.client(),
            2,
            Stream::Stderr,
            0,
            first["chunk_digest"].as_str().unwrap(),
            b"",
            true,
        );
        assert!(f.put(&second).await.is_err());
        assert!(f.page(0, 32).await.chunks.is_empty());
        f.ack(&first).await.unwrap();
        f.put(&second).await.unwrap();
        f.ack(&second).await.unwrap();
        let third = f.chunk(
            &http.client(),
            3,
            Stream::Stdout,
            9,
            second["chunk_digest"].as_str().unwrap(),
            b"tail",
            true,
        );
        f.put(&third).await.unwrap();
        f.ack(&third).await.unwrap();
        let after_eof = f.chunk(
            &http.client(),
            4,
            Stream::Stdout,
            13,
            third["chunk_digest"].as_str().unwrap(),
            b"late",
            true,
        );
        assert!(f.put(&after_eof).await.is_err());
        let page = f.page(0, 2).await;
        assert_eq!(page.next_sequence, 2);
        assert_eq!(page.available_sequence, 3);
        assert_eq!(page.execution_state, ExecutionState::Dispatching);
        assert!(page.enabled);
        assert!(!page.final_report_verified);
        assert!(page.final_sequence.is_none());
        assert_eq!(f.page(2, 32).await.chunks[0].sequence, 3);
        assert!(f.page(3, 32).await.chunks.is_empty());
        let encoded = serde_json::to_string(&page).unwrap();
        assert!(!encoded.contains("store_digest"));
        assert!(!encoded.contains("execution-outputs/v1"));
        for sql in [
            "UPDATE execution_output_chunk_intents SET sequence=sequence+1",
            "DELETE FROM execution_output_chunk_intents",
            "UPDATE execution_output_chunks SET verified_at_ms=verified_at_ms+1",
            "DELETE FROM execution_output_chunks",
        ] {
            assert!(sqlx::query(sql).execute(&f.db.pool).await.is_err());
        }
    }
}

#[tokio::test]
async fn pending_chunk_recovery_rechecks_bytes_and_ack_outbox_is_atomic_across_wal() {
    let http = Http::new();
    let mut f = Fixture::new(&http, true).await;
    let m = f.first(&http, false);
    f.put(&m).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let spool = Spool::open(dir.path()).unwrap();
    f.db.crash_and_restart().await;
    sqlx::raw_sql("CREATE FUNCTION fail_chunk_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='execution.output_chunk_verified' THEN RAISE EXCEPTION 'chunk ack fault'; END IF; RETURN NEW; END; $$; CREATE TRIGGER chunk_ack_fault BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION fail_chunk_event();").execute(&f.db.pool).await.unwrap();
    assert!(
        f.db.store
            .recover_candidate_execution_chunks(&org("acme"), &f.id, &http.client(), &spool)
            .await
            .is_err()
    );
    assert_eq!(count(&f.db, "execution_output_chunks").await, 0);
    assert!(f.page(0, 32).await.chunks.is_empty());
    sqlx::query("DROP TRIGGER chunk_ack_fault ON events")
        .execute(&f.db.pool)
        .await
        .unwrap();
    let ack =
        f.db.store
            .recover_candidate_execution_chunks(&org("acme"), &f.id, &http.client(), &spool)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(ack.sequence, 1);
    assert!(ack.verified_at_ms.is_some());
    assert!(
        f.db.store
            .recover_candidate_execution_chunks(&org("acme"), &f.id, &http.client(), &spool)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.db.store
            .recover_candidate_execution_chunk(&org("acme"), &f.id, 1, &http.client(), &spool)
            .await
            .unwrap(),
        ack
    );
    *http.response.lock().unwrap() = (200, b"changed!!".to_vec());
    assert!(
        f.db.store
            .recover_candidate_execution_chunk(&org("acme"), &f.id, 1, &http.client(), &spool)
            .await
            .is_err()
    );
    assert_eq!(count(&f.db, "execution_output_chunks").await, 1);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE kind='execution.output_chunk_verified'",
    )
    .fetch_one(&f.db.pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    for table in [
        "execution_outputs",
        "execution_completions",
        "candidate_writer_drains",
    ] {
        assert_eq!(count(&f.db, table).await, 0);
    }
}

#[tokio::test]
async fn chunk_reads_reverify_binary_integrity_and_keep_history_after_disconnect() {
    let http = Http::new();
    let f = Fixture::new(&http, false).await;
    let m = f.first(&http, false);
    f.put(&m).await.unwrap();
    assert!(matches!(
        f.db.store
            .download_candidate_execution_chunk(&f.token, &f.id, 1, &http.client())
            .await,
        Err(Error::ExecutionOutputUnavailable)
    ));
    assert_eq!(http.requests.load(Ordering::SeqCst), 0);
    f.ack(&m).await.unwrap();
    for organization in ["acme", "foreign"] {
        let other = runtime_token(
            &f.db,
            organization,
            "alice",
            &agent_computer_store::auth::ServiceScope::ALL,
        )
        .await;
        assert!(matches!(
            f.db.store
                .candidate_execution_chunks(&other, &f.id, 0, 32)
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
        assert!(matches!(
            f.db.store
                .download_candidate_execution_chunk(&other, &f.id, 1, &http.client())
                .await,
            Err(Error::RuntimeAccessUnavailable)
        ));
    }
    assert_eq!(http.requests.load(Ordering::SeqCst), 0);
    let session: String = sqlx::query_scalar("SELECT session_id FROM execution_requests")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    f.db.store
        .close_connection_session(&f.token, &session)
        .await
        .unwrap();
    sqlx::query("DELETE FROM runtime_grants WHERE permission='modify'")
        .execute(&f.db.pool)
        .await
        .unwrap();
    let bytes =
        f.db.store
            .download_candidate_execution_chunk(&f.token, &f.id, 1, &http.client())
            .await
            .unwrap()
            .bytes;
    assert_eq!(bytes, b"\0\xffstdout\n");
    for response in [
        (200, b"corrupt!!".to_vec()),
        (200, vec![0; 8193]),
        (404, vec![]),
        (503, b"private".to_vec()),
    ] {
        *http.response.lock().unwrap() = response;
        assert!(matches!(
            f.db.store
                .download_candidate_execution_chunk(&f.token, &f.id, 1, &http.client())
                .await,
            Err(Error::ExecutionOutputUnavailable)
        ));
    }
    assert_eq!(count(&f.db, "execution_completions").await, 0);
}

#[tokio::test]
async fn chunk_get_rechecks_all_authority_after_success_and_failed_remote_io() {
    for (mutation, status) in [
        ("UPDATE service_credentials SET revoked=true", 200),
        ("UPDATE principals SET enabled=false", 200),
        (
            "UPDATE service_credentials SET expires_at=clock_timestamp()",
            404,
        ),
        (
            "UPDATE service_credentials SET scopes=array_remove(scopes,'runtime.read')",
            200,
        ),
        (
            "DELETE FROM runtime_grants WHERE kind='computer' AND permission='connect'",
            200,
        ),
        (
            "DELETE FROM runtime_grants WHERE kind='computer' AND permission='read'",
            200,
        ),
        (
            "DELETE FROM runtime_grants WHERE kind='workspace' AND permission='read'",
            404,
        ),
    ] {
        let http = Http::new();
        let f = Fixture::new(&http, true).await;
        let m = f.first(&http, false);
        f.put(&m).await.unwrap();
        f.ack(&m).await.unwrap();
        http.response.lock().unwrap().0 = status;
        http.hold.store(true, Ordering::SeqCst);
        let (store, token, id, client) = (
            f.db.store.clone(),
            f.token.clone(),
            f.id.clone(),
            http.client(),
        );
        let task = tokio::spawn(async move {
            store
                .download_candidate_execution_chunk(&token, &id, 1, &client)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), http.received.notified())
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query(mutation).execute(&f.db.pool),
        )
        .await
        .unwrap()
        .unwrap();
        http.hold.store(false, Ordering::SeqCst);
        assert!(
            matches!(
                task.await.unwrap(),
                Err(Error::Unauthenticated | Error::Forbidden | Error::RuntimeAccessUnavailable)
            ),
            "{mutation}"
        );
        assert!(
            f.db.store
                .candidate_execution_chunks(&f.token, &f.id, 0, 32)
                .await
                .is_err()
        );
    }
}
