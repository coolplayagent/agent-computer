//! Real service process and S3 bytes produced by the live gVisor worker.
use super::*;
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
    fn get(&self, token: &str, path: &str) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n").unwrap();
        let mut response = Vec::new();
        stream
            .take(2 * 1024 * 1024)
            .read_to_end(&mut response)
            .unwrap();
        let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
        let headers = std::str::from_utf8(&response[..split])
            .unwrap()
            .to_ascii_lowercase();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert!(headers.contains("cache-control: no-store"));
        assert!(headers.contains("x-content-type-options: nosniff"));
        (status, headers, response[split + 4..].to_vec())
    }
}
pub async fn verify(
    config: &Value,
    private: &Value,
    token: &str,
    id: &str,
    report: &Value,
    metadata: &Value,
) -> Value {
    let path = PathBuf::from(field(config, "result_file")).with_file_name("http-outputs.json");
    fs::write(
        &path,
        serde_json::to_vec(&private["execution"]["outputs"]).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let child = Command::new(field(config, "server_binary"))
        .args([
            "serve",
            "--database-url-file",
            field(config, "database_url_file"),
            "--listen",
            &address.to_string(),
            "--output-config",
        ])
        .arg(&path)
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
            "output service exited"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (code, _, caps) = server.get(token, "/v1alpha1/capabilities");
    assert_eq!(code, 200);
    let caps: Value = serde_json::from_slice(&caps).unwrap();
    assert_eq!(
        caps["capabilities"]["execution.output_downloads"],
        "bounded-verified-streams"
    );
    let mut streams = Vec::new();
    for name in ["stdout", "stderr"] {
        let route = format!("/v1alpha1/executions/{id}/output/{name}");
        let (status, headers, bytes) = server.get(token, &route);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let expected: Vec<u8> =
            serde_json::from_value(report["report"][name]["bytes"].clone()).unwrap();
        assert_eq!(bytes, expected);
        assert!(headers.contains(&format!("content-length: {}\r\n", bytes.len())));
        assert!(!headers.contains("transfer-encoding:"));
        assert!(headers.contains("content-type: application/octet-stream"));
        assert!(headers.contains(&format!(
            "content-disposition: attachment; filename=\"{name}.bin\""
        )));
        let sha = agent_computer_objects::sha256(&bytes);
        assert_eq!(sha, metadata[name]["sha256"]);
        assert!(headers.contains(&format!("x-output-sha256: {sha}")));
        assert!(headers.contains(&format!(
            "x-output-manifest-digest: {}",
            field(metadata, "manifest_digest")
        )));
        assert!(headers.contains(&format!(
            "x-output-truncated: {}",
            metadata[name]["truncated"]
        )));
        assert!(headers.contains(&format!(
            "x-output-observed-bytes: {}",
            metadata[name]["observed_bytes"]
        )));
        assert_eq!(server.get("invalid", &route).0, 401);
        streams.push(json!({"stream":name,"status":status,"bytes":bytes.len(),"sha256":sha,"matches_original_report":true,"unauthenticated_status":401}));
    }
    fs::remove_file(path).unwrap();
    json!({"transport":"real_http","object_source":"signed_s3_get","streams":streams})
}
