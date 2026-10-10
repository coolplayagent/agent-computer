//! Explicit disposable host only, after execution_live has produced revoked mounts.
use agent_computer_csi::{DRIVER, INSTANCE_KEY, rpc, server, wire};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf, time::Duration};
use tonic::{
    Code,
    transport::{Channel, Endpoint},
};
async fn connect() -> Result<Channel, tonic::transport::Error> {
    Endpoint::from_static("http://localhost")
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .connect_with_connector(tower::service_fn(|_| async {
            tokio::net::UnixStream::connect(server::SOCKET)
                .await
                .map(TokioIo::new)
        }))
        .await
}
struct ForeignMount(PathBuf);
impl Drop for ForeignMount {
    fn drop(&mut self) {
        let _ = rustix::mount::unmount(
            &self.0,
            rustix::mount::UnmountFlags::DETACH | rustix::mount::UnmountFlags::NOFOLLOW,
        );
    }
}
#[tokio::test]
async fn real_csi_active_retry_and_restart() {
    assert_eq!(
        std::env::var("AGENT_COMPUTER_CSI_DISPOSABLE_TEST").as_deref(),
        Ok("1")
    );
    assert!(rustix::process::geteuid().is_root());
    let execution = std::env::var("AGENT_COMPUTER_CSI_TEST_EXECUTION")
        .expect("exact running fixture execution");
    let directory = fs::read_dir(PathBuf::from(server::REGISTRY).join("registrations"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            if !p.join("claim.json").exists() || p.join("revoked.json").exists() {
                return false;
            }
            let record: Value =
                serde_json::from_slice(&fs::read(p.join("registration.json")).unwrap()).unwrap();
            PathBuf::from(record["reference"]["path"].as_str().unwrap())
                .file_name()
                .and_then(|s| s.to_str())
                == Some(execution.as_str())
        })
        .expect("one running execution");
    let record: Value =
        serde_json::from_slice(&fs::read(directory.join("registration.json")).unwrap()).unwrap();
    let claim: Value =
        serde_json::from_slice(&fs::read(directory.join("claim.json")).unwrap()).unwrap();
    let publish = publish_request(&record, &claim);
    let mount_line = || {
        fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .lines()
            .find(|l| l.split_whitespace().nth(4) == Some(publish.target_path.as_str()))
            .unwrap()
            .to_owned()
    };
    let before = mount_line();
    let reference: agent_computer_fence::MountReference =
        serde_json::from_value(record["reference"].clone()).unwrap();
    reference.verify().unwrap();
    reference
        .verify_file(&fs::File::open(&publish.target_path).unwrap())
        .unwrap();
    let mut client = rpc::node::node_client::NodeClient::new(connect().await.unwrap());
    client.node_publish_volume(publish.clone()).await.unwrap();
    assert_eq!(mount_line(), before, "retry replaced the mounted instance");
    let mut replacement = publish.clone();
    let uid = "00000000-0000-0000-0000-000000000001";
    replacement.target_path = replacement
        .target_path
        .replace(claim["uid"].as_str().unwrap(), uid);
    replacement
        .volume_context
        .insert("csi.storage.k8s.io/pod.uid".into(), uid.into());
    replacement.volume_id.push_str("-replacement");
    assert_eq!(
        client
            .node_publish_volume(replacement)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(mount_line(), before);
    assert!(
        std::process::Command::new("systemctl")
            .args(["restart", "agent-computer-csi.service"])
            .status()
            .unwrap()
            .success()
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let channel = loop {
        match connect().await {
            Ok(c) => break c,
            Err(e) => {
                assert!(tokio::time::Instant::now() < deadline, "{e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    let mut client = rpc::node::node_client::NodeClient::new(channel);
    client.node_publish_volume(publish.clone()).await.unwrap();
    assert_eq!(
        mount_line(),
        before,
        "publisher restart replaced the mounted instance"
    );
    println!(
        "CSI_ACTIVE_EVIDENCE {}",
        json!({"instance":reference.instance,"exact_retry_retained_mount":true,"replacement_uid_denied":true,"publisher_restart_retained_mount":true})
    );
}
#[tokio::test]
async fn real_csi_cleanup_replay_and_peer_boundary() {
    assert_eq!(
        std::env::var("AGENT_COMPUTER_CSI_DISPOSABLE_TEST").as_deref(),
        Ok("1")
    );
    if !rustix::process::geteuid().is_root() {
        assert!(
            connect().await.is_err(),
            "non-root client reached CSI socket"
        );
        println!("UNPRIVILEGED_CSI_CONNECTION_DENIED");
        return;
    }
    let channel = connect().await.unwrap();
    let mut identity = rpc::identity::identity_client::IdentityClient::new(channel.clone());
    assert_eq!(
        identity
            .get_plugin_info(wire::Empty {})
            .await
            .unwrap()
            .into_inner()
            .name,
        DRIVER
    );
    identity.probe(wire::Empty {}).await.unwrap();
    let mut client = rpc::node::node_client::NodeClient::new(channel);
    assert_eq!(
        client
            .node_get_info(wire::Empty {})
            .await
            .unwrap()
            .into_inner()
            .node_id,
        "ac-component-node"
    );
    client.node_get_capabilities(wire::Empty {}).await.unwrap();
    let root = PathBuf::from(server::REGISTRY);
    let instance =
        std::env::var("AGENT_COMPUTER_CSI_TEST_INSTANCE").expect("exact ended fixture instance");
    assert!(instance.len() == 64 && instance.bytes().all(|b| b.is_ascii_hexdigit()));
    let registration = root.join("registrations").join(instance);
    assert!(registration.join("revoked.json").exists());
    let record: Value =
        serde_json::from_slice(&fs::read(registration.join("registration.json")).unwrap()).unwrap();
    let claim: Value =
        serde_json::from_slice(&fs::read(registration.join("claim.json")).unwrap()).unwrap();
    let unpublish = wire::Unpublish {
        volume_id: claim["volume"].as_str().unwrap().into(),
        target_path: claim["target"].as_str().unwrap().into(),
    };
    for _ in 0..2 {
        client
            .node_unpublish_volume(unpublish.clone())
            .await
            .unwrap();
    }
    let publish = publish_request(&record, &claim);
    assert_eq!(
        client
            .node_publish_volume(publish.clone())
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut replacement = publish.clone();
    replacement.volume_context.insert(
        "csi.storage.k8s.io/pod.uid".into(),
        "00000000-0000-0000-0000-000000000001".into(),
    );
    assert_eq!(
        client
            .node_publish_volume(replacement)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut foreign = publish;
    foreign
        .volume_context
        .insert(INSTANCE_KEY.into(), "0".repeat(64));
    assert_eq!(
        client
            .node_publish_volume(foreign)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut wrong = unpublish.clone();
    wrong.target_path = wrong.target_path.replace(
        claim["uid"].as_str().unwrap(),
        "00000000-0000-0000-0000-000000000001",
    );
    assert_eq!(
        client
            .node_unpublish_volume(wrong)
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    // Cleanup must leave a replaced mount intact, including when the old FUSE
    // connection has gone away. This fixture owns both the ended Pod and bind.
    let target = PathBuf::from(&unpublish.target_path);
    assert!(target.starts_with("/var/lib/kubelet/pods"));
    assert!(target.ends_with("volumes/kubernetes.io~csi/workspace/mount"));
    fs::create_dir_all(&target).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("foreign-test-")
        .tempdir_in(&root)
        .unwrap();
    fs::write(directory.path().join("sentinel"), b"retain foreign mount").unwrap();
    rustix::mount::mount_bind(directory.path(), &target).unwrap();
    let foreign = ForeignMount(target.clone());
    let inode = fs::metadata(&target).unwrap().ino();
    assert_eq!(
        client
            .node_unpublish_volume(unpublish.clone())
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(fs::metadata(&target).unwrap().ino(), inode);
    assert_eq!(
        fs::read(target.join("sentinel")).unwrap(),
        b"retain foreign mount"
    );
    drop(foreign);
    client.node_unpublish_volume(unpublish).await.unwrap();
    assert!(
        std::process::Command::new("setpriv")
            .args(["--reuid=1000", "--regid=1000", "--clear-groups"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "real_csi_cleanup_replay_and_peer_boundary",
                "--nocapture"
            ])
            .status()
            .unwrap()
            .success()
    );
    println!(
        "CSI_RPC_EVIDENCE {}",
        json!({"driver":DRIVER,"instance":record["reference"]["instance"],"cleanup_idempotent":true,"revoked_republish_denied":true,"replacement_pod_denied":true,"unregistered_instance_denied":true,"foreign_target_denied":true,"foreign_mount_retained":true,"non_root_denied":true})
    );
}

fn publish_request(record: &Value, claim: &Value) -> wire::Publish {
    wire::Publish {
        volume_id: claim["volume"].as_str().unwrap().into(),
        target_path: claim["target"].as_str().unwrap().into(),
        volume_capability: Some(wire::Capability {
            access: Some(wire::capability::Access::Mount(wire::Mount::default())),
            access_mode: Some(wire::AccessMode { mode: 1 }),
        }),
        volume_context: [
            (
                INSTANCE_KEY,
                record["reference"]["instance"].as_str().unwrap(),
            ),
            (
                "csi.storage.k8s.io/pod.name",
                record["pod"]["name"].as_str().unwrap(),
            ),
            (
                "csi.storage.k8s.io/pod.namespace",
                record["pod"]["namespace"].as_str().unwrap(),
            ),
            ("csi.storage.k8s.io/pod.uid", claim["uid"].as_str().unwrap()),
            ("csi.storage.k8s.io/serviceAccount.name", "default"),
            ("csi.storage.k8s.io/ephemeral", "true"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect(),
        ..Default::default()
    }
}
