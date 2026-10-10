//! Synthetic SQL + bounded HTTP fault server exercise read authority, not execution.
use super::*;
use agent_computer_objects::{Client, Configuration, sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

pub(super) struct Http {
    pub(super) config: Configuration,
    _directory: tempfile::TempDir,
    pub(super) response: Arc<Mutex<(u16, Vec<u8>)>>,
    pub(super) requests: Arc<AtomicUsize>,
    pub(super) hold: Arc<AtomicBool>,
    pub(super) received: Arc<tokio::sync::Notify>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Http {
    pub(super) fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let credentials = directory.path().join("credentials.json");
        std::fs::write(
            &credentials,
            br#"{"access_key":"test","secret_key":"test-secret-key-for-fixture"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = Configuration {
            endpoint: format!("http://{}/", listener.local_addr().unwrap()),
            region: "test".into(),
            bucket: "outputs".into(),
            credentials_file: credentials,
            ca_file: None,
            allow_http: true,
        };
        let response = Arc::new(Mutex::new((200, b"\0\xffstdout\n".to_vec())));
        let requests = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(AtomicBool::new(false));
        let received = Arc::new(tokio::sync::Notify::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (reply, calls, gate, notice, done) = (
            response.clone(),
            requests.clone(),
            hold.clone(),
            received.clone(),
            stop.clone(),
        );
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                let text = String::from_utf8(request).unwrap();
                assert!(text.starts_with("GET /outputs/execution-outputs/v1/acme/exec_"));
                assert!(
                    text.to_lowercase()
                        .contains("authorization: aws4-hmac-sha256")
                );
                calls.fetch_add(1, Ordering::SeqCst);
                notice.notify_one();
                while gate.load(Ordering::SeqCst) && !done.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let (status, body) = reply.lock().unwrap().clone();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Self {
            config,
            _directory: directory,
            response,
            requests,
            hold,
            received,
            stop,
            thread: Some(thread),
        }
    }
    pub(super) fn client(&self) -> Client {
        Client::new(&self.config).unwrap()
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

async fn fixture(http: &Http, verified: bool) -> (Database, String, String) {
    let (db, attempt, mut m, now, token) = output_fixture_with_token(false).await;
    let client = http.client();
    let id = attempt.intent().execution.execution_id.clone();
    let stdout = http.response.lock().unwrap().1.clone();
    let bytes = [Vec::new(), stdout, Vec::new(), Vec::new()];
    for (index, bytes) in bytes.iter().enumerate() {
        m["objects"][index] =
            serde_json::to_value(client.reference("acme", &id, bytes).unwrap()).unwrap();
    }
    for (name, index) in [("stdout", 1), ("stderr", 2)] {
        m["summary"][name] = json!({"sha256":sha256(&bytes[index]),"retained_bytes":bytes[index].len(),"observed_bytes":bytes[index].len()+7,"truncated":true,"eof":true});
    }
    let mut digest = Sha256::new();
    digest.update(b"agent-computer/execution-outputs-v1\0");
    digest.update(serde_json::to_vec(&m).unwrap());
    let digest = format!("sha256:{:x}", digest.finalize());
    sqlx::query("INSERT INTO execution_output_intents (organization,execution_id,pod_uid,dispatch_digest,grant_digest,arm_digest,manifest,manifest_digest,created_at_ms) VALUES ('acme',$1,'pod-one',$2,$3,$4,$5,$6,$7)")
        .bind(&id).bind(m["dispatch_digest"].as_str().unwrap()).bind(m["grant_digest"].as_str().unwrap()).bind(m["arm_digest"].as_str().unwrap()).bind(&m).bind(&digest).bind(now).execute(&db.pool).await.unwrap();
    if verified {
        publish(&db, &m, &digest, now).await.unwrap();
    }
    (db, token, id)
}

#[tokio::test]
async fn output_download_preserves_binary_and_empty_streams_without_accepting_completion() {
    let http = Http::new();
    let (mut db, token, id) = fixture(&http, true).await;
    db.crash_and_restart().await;
    let before = db
        .store
        .candidate_execution_output(&token, &id)
        .await
        .unwrap()
        .unwrap();
    let result = db
        .store
        .download_candidate_execution_output(&token, &id, OutputStream::Stdout, &http.client())
        .await
        .unwrap();
    assert_eq!(result.bytes, b"\0\xffstdout\n");
    assert_eq!(result.output.manifest_digest, before.manifest_digest);
    assert!(result.output.stdout.truncated);
    *http.response.lock().unwrap() = (200, Vec::new());
    let result = db
        .store
        .download_candidate_execution_output(&token, &id, OutputStream::Stderr, &http.client())
        .await
        .unwrap();
    assert!(result.bytes.is_empty());
    assert_eq!(http.requests.load(Ordering::SeqCst), 2);
    assert_eq!(count(&db, "execution_completions").await, 0);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert_eq!(count(&db, "execution_startup_grants").await, 1);
}

#[tokio::test]
async fn output_download_requires_verified_publication_and_owner_before_remote_io() {
    let http = Http::new();
    let (db, token, id) = fixture(&http, false).await;
    assert!(matches!(
        db.store
            .download_candidate_execution_output(&token, &id, OutputStream::Stdout, &http.client())
            .await,
        Err(Error::ExecutionOutputUnavailable)
    ));
    let other = runtime_token(
        &db,
        "acme",
        "alice",
        &agent_computer_store::auth::ServiceScope::ALL,
    )
    .await;
    assert!(matches!(
        db.store
            .download_candidate_execution_output(&other, &id, OutputStream::Stdout, &http.client())
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    let foreign = runtime_token(
        &db,
        "foreign",
        "alice",
        &agent_computer_store::auth::ServiceScope::ALL,
    )
    .await;
    assert!(matches!(
        db.store
            .download_candidate_execution_output(
                &foreign,
                &id,
                OutputStream::Stdout,
                &http.client()
            )
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(http.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn output_download_reverifies_integrity_and_never_uses_a_cached_receipt_as_bytes() {
    let http = Http::new();
    let (db, token, id) = fixture(&http, true).await;
    for response in [
        (200, b"\0\xffchanged".to_vec()),
        (200, b"short".to_vec()),
        (200, vec![1; 4096]),
        (404, Vec::new()),
        (503, b"private error".to_vec()),
    ] {
        *http.response.lock().unwrap() = response;
        assert!(matches!(
            db.store
                .download_candidate_execution_output(
                    &token,
                    &id,
                    OutputStream::Stdout,
                    &http.client()
                )
                .await,
            Err(Error::ExecutionOutputUnavailable)
        ));
    }
    assert_eq!(http.requests.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn output_download_rechecks_revocation_after_success_or_failed_remote_io() {
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
        let (db, token, id) = fixture(&http, true).await;
        http.response.lock().unwrap().0 = status;
        http.hold.store(true, Ordering::SeqCst);
        let (store, client) = (db.store.clone(), http.client());
        let task = tokio::spawn(async move {
            store
                .download_candidate_execution_output(&token, &id, OutputStream::Stdout, &client)
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), http.received.notified())
            .await
            .unwrap();
        // A slow object GET must not hold the organization or credential locks.
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query(mutation).execute(&db.pool),
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
    }
}

#[tokio::test]
async fn output_download_keeps_historical_reads_after_disconnect_but_requires_current_read_grants()
{
    let http = Http::new();
    let (db, token, id) = fixture(&http, true).await;
    let session: String = sqlx::query_scalar("SELECT session_id FROM execution_requests")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    db.store
        .close_connection_session(&token, &session)
        .await
        .unwrap();
    sqlx::query("DELETE FROM runtime_grants WHERE permission='modify'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.store
            .download_candidate_execution_output(&token, &id, OutputStream::Stdout, &http.client())
            .await
            .is_ok()
    );
    sqlx::query("DELETE FROM runtime_grants WHERE kind='workspace' AND permission='read'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .download_candidate_execution_output(&token, &id, OutputStream::Stdout, &http.client())
            .await,
        Err(Error::RuntimeAccessUnavailable)
    ));
    assert_eq!(http.requests.load(Ordering::SeqCst), 1);
}
