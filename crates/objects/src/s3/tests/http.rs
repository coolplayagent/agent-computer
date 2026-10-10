use super::super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    thread::{self, JoinHandle},
};

struct Fixture {
    config: Configuration,
    thread: JoinHandle<()>,
    _private: tempfile::TempDir,
}
impl Fixture {
    fn new(responses: Vec<(&'static str, Option<Vec<u8>>)>) -> Self {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", socket.local_addr().unwrap());
        socket.set_nonblocking(true).unwrap();
        let private = tempfile::tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let credentials = private.path().join("credentials.json");
        std::fs::write(
            &credentials,
            br#"{"access_key":"test-key","secret_key":"fixture-only-secret"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        let thread = thread::spawn(move || {
            for (method, response) in responses {
                let until = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match socket.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < until, "request missing");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut data = Vec::new();
                let mut byte = [0];
                while !data.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    data.push(byte[0]);
                    assert!(data.len() < 8192);
                }
                let headers = String::from_utf8(data).unwrap();
                assert!(
                    headers
                        .starts_with(&format!("{method} /outputs/execution-outputs/v1/org/exec/"))
                );
                let headers = headers.to_ascii_lowercase();
                assert!(headers.contains("authorization: aws4-hmac-sha256 credential=test-key/"));
                assert!(headers.contains("signedheaders="));
                if method == "PUT" {
                    assert!(headers.contains("if-none-match: *\r\n"));
                    let n: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    let mut body = vec![0; n];
                    stream.read_exact(&mut body).unwrap();
                    assert_eq!(body, b"data");
                    assert!(
                        headers.contains(&format!("x-amz-content-sha256: {}", &sha256(&body)[7..]))
                    );
                }
                if let Some(response) = response {
                    stream.write_all(&response).unwrap();
                }
                // None models an accepted PUT whose acknowledgement was lost.
            }
        });
        Self {
            config: Configuration {
                endpoint,
                region: "us-east-1".into(),
                bucket: "outputs".into(),
                credentials_file: credentials,
                ca_file: None,
                allow_http: true,
            },
            thread,
            _private: private,
        }
    }
    fn client(&self) -> Client {
        Client::new(&self.config).unwrap()
    }
    fn finish(self) {
        self.thread.join().unwrap();
    }
}
fn response(status: u16, body: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = format!(
        "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    Some(bytes)
}

#[tokio::test]
async fn conditional_put_and_existing_object_require_full_readback() {
    for status in [200, 412] {
        let fixture = Fixture::new(vec![
            ("PUT", response(status, b"")),
            ("GET", response(200, b"data")),
        ]);
        let client = fixture.client();
        let object = client.reference("org", "exec", b"data").unwrap();
        assert_eq!(
            client
                .put_verified(&object, b"data")
                .await
                .unwrap()
                .reference(),
            &object
        );
        fixture.finish();
    }
    let fixture = Fixture::new(vec![
        ("PUT", response(412, b"")),
        ("GET", response(200, b"evil")),
    ]);
    let client = fixture.client();
    let object = client.reference("org", "exec", b"data").unwrap();
    assert!(matches!(
        client.put_verified(&object, b"data").await,
        Err(Error::Integrity)
    ));
    fixture.finish();
}

#[tokio::test]
async fn lost_put_ack_is_not_a_receipt_and_retry_reads_existing_bytes() {
    let fixture = Fixture::new(vec![
        ("PUT", None),
        ("PUT", response(412, b"")),
        ("GET", response(200, b"data")),
    ]);
    let client = fixture.client();
    let object = client.reference("org", "exec", b"data").unwrap();
    assert!(matches!(
        client.put_verified(&object, b"data").await,
        Err(Error::Transport)
    ));
    assert!(client.put_verified(&object, b"data").await.is_ok());
    fixture.finish();
}

#[tokio::test]
async fn get_rejects_corruption_truncation_oversize_redirect_and_service_errors() {
    for (reply, error) in [
        (response(200, b"evil"), Error::Integrity),
        (response(200, b"dat"), Error::Integrity),
        (response(200, b"datab"), Error::Integrity),
        (Some(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\ndatab\r\n0\r\n\r\n".to_vec()), Error::Limit),
        (Some(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndat".to_vec()), Error::Transport),
        (Some(b"HTTP/1.1 307 redirect\r\nLocation: http://127.0.0.1:1/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()), Error::Rejected),
        (response(404, b""), Error::Missing),
        (response(403, b""), Error::Rejected),
        (response(503, b""), Error::Rejected),
    ] {
        let fixture = Fixture::new(vec![("GET", reply)]);
        let client = fixture.client();
        let object = client.reference("org", "exec", b"data").unwrap();
        assert_eq!(client.get(&object).await, Err(error));
        fixture.finish();
    }
}

#[tokio::test]
async fn put_conflict_and_service_failure_do_not_generate_receipts() {
    for status in [409, 403, 503] {
        let fixture = Fixture::new(vec![("PUT", response(status, b""))]);
        let client = fixture.client();
        let object = client.reference("org", "exec", b"data").unwrap();
        assert!(matches!(
            client.put_verified(&object, b"data").await,
            Err(Error::Rejected)
        ));
        fixture.finish();
    }
}

#[test]
fn credentials_and_endpoint_are_explicit_and_bound() {
    let fixture = Fixture::new(vec![]);
    let client = fixture.client();
    for endpoint in [
        "http://user:password@localhost/",
        "http://localhost/prefix",
        "http://localhost/?query",
        "http://localhost/#fragment",
    ] {
        let mut config = fixture.config.clone();
        config.endpoint = endpoint.into();
        assert!(matches!(Client::new(&config), Err(Error::Configuration)));
    }
    let mut config = fixture.config.clone();
    config.allow_http = false;
    assert!(matches!(Client::new(&config), Err(Error::Configuration)));
    let mut config = fixture.config.clone();
    config.bucket = "different".into();
    let other = Client::new(&config).unwrap();
    let object = client.reference("org", "exec", b"data").unwrap();
    assert_eq!(other.bound(&object), Err(Error::Integrity));
    std::fs::write(
        &config.credentials_file,
        br#"{"access_key":"rotated","secret_key":"rotated-test-secret"}"#,
    )
    .unwrap();
    assert_eq!(fixture.client().store_digest(), client.store_digest());
    std::fs::set_permissions(
        &config.credentials_file,
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(matches!(Client::new(&config), Err(Error::Configuration)));
    fixture.finish();
}
