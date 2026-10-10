use super::*;
use serde_json::json;

fn fixture() -> (Registration, wire::Publish) {
    let instance = "a".repeat(64);
    let uid = "12345678-1234-1234-1234-123456789abc";
    let reg:Registration=serde_json::from_value(json!({"reference":{"version":1,"instance":instance,"path":"/var/lib/agent-computer-csi/mounts/execution-a","device":42,"inode":1,"boot_id":uid,"mount_namespace":42,"prepared":{"version":1,"request_digest":"sha256:x","filesystem_uuid":"uuid","volume_uid":"pv-uid","path_ref":"candidate/data","data_inode":44,"manifest_digest":"sha256:y","quota_bytes":1073741824}},"pod":{"namespace":"ac-test","name":"pod-a","node":"node-a"}})).unwrap();
    let request = wire::Publish {
        volume_id: "csi-volume-a".into(),
        target_path: format!(
            "/var/lib/kubelet/pods/{uid}/volumes/kubernetes.io~csi/workspace/mount"
        ),
        volume_capability: Some(wire::Capability {
            access: Some(wire::capability::Access::Mount(wire::Mount::default())),
            access_mode: Some(wire::AccessMode { mode: 1 }),
        }),
        volume_context: [
            (INSTANCE_KEY, instance.as_str()),
            ("csi.storage.k8s.io/pod.name", "pod-a"),
            ("csi.storage.k8s.io/pod.namespace", "ac-test"),
            ("csi.storage.k8s.io/pod.uid", uid),
            ("csi.storage.k8s.io/serviceAccount.name", "default"),
            ("csi.storage.k8s.io/ephemeral", "true"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect(),
        ..Default::default()
    };
    (reg, request)
}
#[test]
fn csi_request_requires_exact_node_pod_uid_and_ephemeral_context() {
    let (reg, request) = fixture();
    validate_publish(&request, &reg, "node-a").unwrap();
    assert!(validate_publish(&request, &reg, "node-b").is_err());
    for key in request.volume_context.keys() {
        let mut changed = request.clone();
        changed.volume_context.remove(key);
        assert!(
            validate_publish(&changed, &reg, "node-a").is_err(),
            "missing {key}"
        );
        changed
            .volume_context
            .insert(key.clone(), "replacement".into());
        assert!(
            validate_publish(&changed, &reg, "node-a").is_err(),
            "foreign {key}"
        );
    }
    let mut changed = request.clone();
    changed
        .volume_context
        .insert("unqualified-option".into(), "true".into());
    assert!(validate_publish(&changed, &reg, "node-a").is_err());
    for target in [
        "/tmp/mount",
        "/var/lib/kubelet/pods/../volumes/kubernetes.io~csi/workspace/mount",
        "/var/lib/kubelet/pods/12345678-1234-1234-1234-123456789abc/volumes/kubernetes.io~csi/other/mount",
    ] {
        changed = request.clone();
        changed.target_path = target.into();
        assert!(validate_publish(&changed, &reg, "node-a").is_err());
    }
}
#[test]
fn csi_cannot_request_block_shared_readonly_or_custom_mount_options() {
    let (reg, request) = fixture();
    let mut cases = Vec::new();
    let mut r = request.clone();
    r.readonly = true;
    cases.push(r);
    let mut r = request.clone();
    r.staging_target_path = "/staging".into();
    cases.push(r);
    let mut r = request.clone();
    r.secrets.insert("key".into(), "value".into());
    cases.push(r);
    let mut r = request.clone();
    r.publish_context.insert("key".into(), "value".into());
    cases.push(r);
    let mut r = request.clone();
    r.volume_capability = None;
    cases.push(r);
    for mode in [0, 2, 3, 4, 5, 6, 7] {
        let mut r = request.clone();
        r.volume_capability
            .as_mut()
            .unwrap()
            .access_mode
            .as_mut()
            .unwrap()
            .mode = mode;
        cases.push(r);
    }
    let mut r = request.clone();
    r.volume_capability.as_mut().unwrap().access =
        Some(wire::capability::Access::Block(wire::Empty {}));
    cases.push(r);
    for mount in [
        wire::Mount {
            fs_type: "ext4".into(),
            ..Default::default()
        },
        wire::Mount {
            mount_flags: vec!["bind".into()],
            ..Default::default()
        },
        wire::Mount {
            volume_mount_group: "1000".into(),
            ..Default::default()
        },
    ] {
        let mut r = request.clone();
        r.volume_capability.as_mut().unwrap().access = Some(wire::capability::Access::Mount(mount));
        cases.push(r);
    }
    for r in cases {
        assert!(validate_publish(&r, &reg, "node-a").is_err());
    }
}
#[test]
fn cleanup_mount_identity_rejects_foreign_device_source_and_subtree() {
    let (reg, _) = fixture();
    let r = &reg.reference;
    let mut mount = Mount {
        device: format!(
            "{}:{}",
            rustix::fs::major(r.device),
            rustix::fs::minor(r.device)
        ),
        root: "/".into(),
        fs: "fuse".into(),
        source: format!("agent-computer-{}", r.instance),
        propagating: false,
    };
    match_mount(&mount, r).unwrap();
    mount.root = "/subtree".into();
    assert!(match_mount(&mount, r).is_err());
    mount.root = "/".into();
    mount.device = "0:99".into();
    assert!(match_mount(&mount, r).is_err());
    mount.device = format!(
        "{}:{}",
        rustix::fs::major(r.device),
        rustix::fs::minor(r.device)
    );
    mount.source = "juicefs".into();
    assert!(match_mount(&mount, r).is_err());
}
