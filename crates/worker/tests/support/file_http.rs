use super::*;
use agent_computer_store::runtime::{connections::*, writers::*};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
};

struct Server {
    child: Child,
    address: SocketAddr,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn call(&self, token: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let bytes = body
            .map(|v| serde_json::to_vec(&v).unwrap())
            .unwrap_or_default();
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        write!(stream,"{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
        stream.write_all(&bytes).unwrap();
        let mut response = Vec::new();
        stream
            .take(5 * 1024 * 1024)
            .read_to_end(&mut response)
            .unwrap();
        let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
        let headers = std::str::from_utf8(&response[..split]).unwrap();
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("cache-control: no-store")
        );
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (
            status,
            serde_json::from_slice(&response[split + 4..]).unwrap(),
        )
    }
}
pub async fn verify(c: &file_writer::Context<'_>, previous: &WriterLease) -> (Value, WriterLease) {
    let config_file = PathBuf::from(c.config["observation_file"].as_str().unwrap())
        .with_file_name("http-files.json");
    fs::write(
        &config_file,
        serde_json::to_vec(
            &json!([{"target":c.worker["target"],"mount_root":c.worker["mount_root"]}]),
        )
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600)).unwrap();
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let child = Command::new(c.config["server_binary"].as_str().unwrap())
        .args([
            "serve",
            "--database-url-file",
            c.config["database_url_file"].as_str().unwrap(),
            "--listen",
            &address.to_string(),
            "--file-config",
        ])
        .arg(&config_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Server { child, address };
    for _ in 0..100 {
        if TcpStream::connect(address).is_ok() {
            break;
        }
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "file server exited"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reader = c
        .store
        .issue_credential(IssueCredential {
            organization: c.org,
            principal: c.actor,
            kind: PrincipalKind::Human,
            scopes: &[ServiceScope::RuntimeConnect, ServiceScope::RuntimeRead],
            lifetime: Duration::from_secs(300),
        })
        .await
        .unwrap();
    let session = c
        .store
        .create_connection_session(
            reader.expose_token(),
            &key("http-reader"),
            c.computer,
            &ConnectRequest {
                requested_capabilities: vec![RuntimePermission::Connect, RuntimePermission::Read],
                lifetime_seconds: 300,
            },
        )
        .await
        .unwrap();
    let workspace: String = sqlx::query_scalar(
        "SELECT workspace_id FROM runtime_start_requests WHERE organization=$1 AND request_id=$2",
    )
    .bind(c.org.as_str())
    .bind(&c.start.request_id)
    .fetch_one(c.pool)
    .await
    .unwrap();
    let route = format!(
        "/v1alpha1/workspaces/{workspace}/files?connection_session_id={}&generation={}&candidate_id={}&path=saved.bin",
        session.session_id, c.start.generation, c.start.candidate_id
    );
    let (status, before) = server.call(reader.expose_token(), "GET", &route, None);
    assert_eq!(status, 200);
    assert_eq!(
        before["version"],
        serde_json::to_value(
            previous
                .file_edit
                .as_ref()
                .unwrap()
                .version
                .as_ref()
                .unwrap()
        )
        .unwrap()
    );
    assert_eq!(
        server.call(c.token, "GET", &route, None).0,
        404,
        "other credential cannot read a reader-owned connection"
    );
    let input = AcquireWriterLease {
        scope: WriterScope::Modify,
        connection_session_id: previous.connection_session_id.clone(),
        generation: c.start.generation,
        candidate_id: c.start.candidate_id.clone(),
        duration_seconds: 30,
    };
    let lease = c
        .store
        .acquire_candidate_writer(c.token, &key("http-writer"), c.computer, &input)
        .await
        .unwrap();
    let route_save = format!("/v1alpha1/leases/{}/file", lease.lease_id);
    let save = json!({"lease":{"connection_session_id":lease.connection_session_id,"generation":lease.generation,"epoch":lease.epoch,"expected_revision":lease.revision},"dispatch_id":"http-save","edit":{"path":"saved.bin","expected":before["version"],"content":b"http-saved-and-read".to_vec(),"executable":false}});
    assert_eq!(
        server
            .call(
                reader.expose_token(),
                "POST",
                &route_save,
                Some(save.clone())
            )
            .0,
        403
    );
    let (status, result) = server.call(c.token, "POST", &route_save, Some(save.clone()));
    assert_eq!(status, 200);
    let completed: WriterLease = serde_json::from_value(result.clone()).unwrap();
    assert_eq!(completed.state, WriterLeaseState::Released);
    assert_eq!(
        completed.file_edit.as_ref().unwrap().state,
        agent_computer_storage::files::FileEditState::Applied
    );
    let inode = c.data.join("saved.bin").metadata().unwrap().ino();
    let (status, retry) = server.call(c.token, "POST", &route_save, Some(save.clone()));
    assert_eq!(status, 200);
    assert_eq!(retry["revision"], result["revision"]);
    assert_eq!(c.data.join("saved.bin").metadata().unwrap().ino(), inode);
    let mut changed = save;
    changed["edit"]["content"] = json!([0]);
    assert_eq!(
        server.call(c.token, "POST", &route_save, Some(changed)).0,
        409
    );
    let (status, after) = server.call(reader.expose_token(), "GET", &route, None);
    assert_eq!(status, 200);
    assert_eq!(after["content"], json!(b"http-saved-and-read".to_vec()));
    // A delayed database completion exceeds the HTTP waiting deadline. The
    // owned file job must still finish with the same evidence and release its
    // lease; the response timeout must not cancel or repeat the actual write.
    let lease = c
        .store
        .acquire_candidate_writer(c.token, &key("http-timeout-writer"), c.computer, &input)
        .await
        .unwrap();
    let delayed = json!({"lease":{"connection_session_id":lease.connection_session_id,"generation":lease.generation,"epoch":lease.epoch,"expected_revision":lease.revision},"dispatch_id":"http-timeout","edit":{"path":"saved.bin","expected":after["version"],"content":b"http-saved-and-read".to_vec(),"executable":false}});
    sqlx::raw_sql("CREATE FUNCTION delay_http_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM events WHERE organization=NEW.organization AND sequence=NEW.sequence AND kind='writer.file_completed' AND payload->>'dispatch_id'='http-timeout') THEN PERFORM pg_sleep(11); END IF; RETURN NEW; END $$; CREATE TRIGGER delay_http_completion BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION delay_http_completion();").execute(c.pool).await.unwrap();
    let (status, timeout) = server.call(c.token, "POST", &route_save, Some(delayed.clone()));
    assert_eq!(status, 408);
    assert_eq!(timeout["code"], "request_timeout");
    assert_eq!(server.call(c.token, "GET", "/health", None).0, 200);
    let (status, finished) = server.call(
        c.token,
        "GET",
        &format!("/v1alpha1/leases/{}", lease.lease_id),
        None,
    );
    assert_eq!(status, 200);
    let completed: WriterLease = serde_json::from_value(finished.clone()).unwrap();
    assert_eq!(completed.state, WriterLeaseState::Released);
    assert_eq!(
        completed.file_edit.as_ref().unwrap().state,
        agent_computer_storage::files::FileEditState::Applied
    );
    let inode = c.data.join("saved.bin").metadata().unwrap().ino();
    let (status, retry) = server.call(c.token, "POST", &route_save, Some(delayed));
    assert_eq!(status, 200);
    assert_eq!(retry["revision"], finished["revision"]);
    assert_eq!(c.data.join("saved.bin").metadata().unwrap().ino(), inode);
    sqlx::raw_sql(
        "DROP TRIGGER delay_http_completion ON outbox; DROP FUNCTION delay_http_completion();",
    )
    .execute(c.pool)
    .await
    .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", c.data.join("http-escape")).unwrap();
    assert_eq!(
        server
            .call(
                reader.expose_token(),
                "GET",
                &route.replace("path=saved.bin", "path=http-escape"),
                None
            )
            .0,
        404
    );
    c.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: c.org,
                principal: c.actor,
                kind: RuntimeKind::Workspace,
                resource_id: &workspace,
                permission: RuntimePermission::Read,
                max_runtime_seconds: None,
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        server.call(reader.expose_token(), "GET", &route, None).0,
        404
    );
    c.store
        .set_runtime_grant(
            RuntimeGrant {
                organization: c.org,
                principal: c.actor,
                kind: RuntimeKind::Workspace,
                resource_id: &workspace,
                permission: RuntimePermission::Read,
                max_runtime_seconds: None,
            },
            true,
        )
        .await
        .unwrap();
    c.store
        .close_connection_session(reader.expose_token(), &session.session_id)
        .await
        .unwrap();
    assert_eq!(
        server.call(reader.expose_token(), "GET", &route, None).0,
        410
    );
    (
        json!({"tcp_process":true,"http_timeout_keeps_job_and_exact_retry":true,"read_only_session":true,"save_and_readback":true,"exact_retry_preserves_inode":true,"changed_intent_conflict":true,"cross_credential_denied":true,"symlink_denied":true,"workspace_revoke_denied":true,"closed_session_denied":true,"version":after["version"]}),
        completed,
    )
}
