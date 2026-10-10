//! Minimal HTTP object fixture: authority tests only, never S3 certification.
use agent_computer_objects::{Client, Configuration};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
pub(super) struct Objects {
    pub client: Client,
    pub bytes: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    _private: tempfile::TempDir,
}
impl Objects {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let private = tempfile::tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = private.path().join("credentials");
        std::fs::write(
            &file,
            br#"{"access_key":"fixture","secret_key":"fixture-only-secret"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let client = Client::new(&Configuration {
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            region: "us-east-1".into(),
            bucket: "artifacts".into(),
            credentials_file: file,
            ca_file: None,
            allow_http: true,
        })
        .unwrap();
        let bytes = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let shared = bytes.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut headers = Vec::new();
                let mut byte = [0];
                while !headers.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                    assert!(headers.len() < 8192);
                }
                let headers = String::from_utf8(headers).unwrap();
                let first = headers.lines().next().unwrap();
                assert!(first.starts_with("GET "));
                let path = first.split_whitespace().nth(1).unwrap();
                let data = shared.lock().unwrap().get(path).cloned();
                let (code, data) = match data {
                    Some(v) => (200, v),
                    None => (404, Vec::new()),
                };
                write!(
                    stream,
                    "HTTP/1.1 {code} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                )
                .unwrap();
                stream.write_all(&data).unwrap();
            }
        });
        Self {
            client,
            bytes,
            stop,
            thread: Some(thread),
            _private: private,
        }
    }
}
impl Drop for Objects {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
