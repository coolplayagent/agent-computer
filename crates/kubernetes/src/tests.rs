use super::*;
use crate::plan::{BINDING, IDENTITY};
use agent_computer_definitions::{Format, validate_bytes};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

mod attach;

fn identity() -> InstanceIdentity {
    InstanceIdentity {
        organization: "org_a".into(),
        computer: "computer_a".into(),
        sandbox: "sandbox_a".into(),
        instance: "instance_a".into(),
        generation: 1,
        spec_revision: 1,
    }
}

fn definition(mount: bool) -> agent_computer_definitions::ValidatedDefinition {
    let mut value = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet",
        "metadata":{"name":"test"},"spec":{"sandboxes":[{"name":"sandbox","runtimeClass":"gvisor",
            "image":format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            "resources":{"cpuMillis":1000,"memoryMiB":256},"networkPolicyRef":"deny-all","mounts":[]}]}});
    if mount {
        value["spec"]["sandboxes"][0]["mounts"] =
            json!([{"workspaceRef":"id:workspace_a","path":"/workspace","readOnly":false}]);
    }
    validate_bytes(&serde_json::to_vec(&value).unwrap(), Format::Json).unwrap()
}

pub(super) fn plan() -> EphemeralSandboxPlan {
    EphemeralSandboxPlan::new(
        &definition(false),
        "sandbox",
        identity(),
        "ac-test",
        vec!["/bin/sleep".into(), "300".into()],
    )
    .unwrap()
}

fn deployment() -> Deployment {
    Deployment {
        namespace: "ac-test".into(),
        namespace_uid: "namespace-uid".into(),
        runtime_class_uid: "runtime-uid".into(),
        deny_policy_uid: "policy-uid".into(),
        network_policy_ref: "deny-all".into(),
    }
}

fn prerequisites() -> Vec<Value> {
    vec![
        json!({"metadata":{"name":"ac-test","uid":"namespace-uid","labels":{"pod-security.kubernetes.io/enforce":"restricted"}},"status":{"phase":"Active"}}),
        json!({"metadata":{"name":"gvisor","uid":"runtime-uid"},"handler":"runsc"}),
        json!({"items":[{"metadata":{"namespace":"ac-test","uid":"policy-uid"},"spec":{"podSelector":{},"policyTypes":["Ingress","Egress"]}}]}),
    ]
}

pub(super) fn pod(plan: &EphemeralSandboxPlan) -> Value {
    let mut pod = plan.manifest();
    pod["metadata"]["uid"] = json!("pod-uid");
    pod["metadata"]["resourceVersion"] = json!("42");
    pod["status"] = json!({"phase":"Pending"});
    pod
}

#[test]
fn one_instance_name_survives_changed_generation_and_spec_but_cannot_be_adopted() {
    let first = plan();
    let mut id = identity();
    id.generation = 2;
    id.spec_revision = 2;
    let changed = EphemeralSandboxPlan::new(
        &definition(false),
        "sandbox",
        id,
        "ac-test",
        vec!["/bin/true".into()],
    )
    .unwrap();
    assert_eq!(first.pod_name(), changed.pod_name());
    assert_eq!(
        verify::pod(&changed, &pod(&first), None).unwrap_err(),
        Error::IdentityMismatch
    );
}

#[test]
fn workspace_mounts_and_invalid_runtime_identity_are_rejected() {
    assert_eq!(
        EphemeralSandboxPlan::new(
            &definition(true),
            "sandbox",
            identity(),
            "ac-test",
            vec!["true".into()]
        )
        .unwrap_err(),
        Error::UnsupportedSandbox
    );
    for (namespace, generation) in [("../../default", 1), ("ac-test", 0), ("ac-test", u64::MAX)] {
        let mut id = identity();
        id.generation = generation;
        assert_eq!(
            EphemeralSandboxPlan::new(
                &definition(false),
                "sandbox",
                id,
                namespace,
                vec!["true".into()]
            )
            .unwrap_err(),
            Error::InvalidIdentity
        );
    }
    assert_eq!(
        EphemeralSandboxPlan::new(
            &definition(false),
            "sandbox",
            identity(),
            "ac-test",
            vec!["a\0b".into()]
        )
        .unwrap_err(),
        Error::InvalidCommand
    );
}

#[test]
fn adoption_requires_binding_label_actual_uid_and_resource_version() {
    let plan = plan();
    let source = pod(&plan);
    assert_eq!(verify::pod(&plan, &source, None).unwrap().uid(), "pod-uid");
    assert_eq!(
        verify::pod(&plan, &source, Some("replacement")).unwrap_err(),
        Error::IdentityMismatch
    );
    for path in [
        vec!["metadata", "annotations", BINDING],
        vec!["metadata", "labels", IDENTITY],
        vec!["metadata", "namespace"],
    ] {
        let mut changed = source.clone();
        let mut v = &mut changed;
        for key in path {
            v = &mut v[key];
        }
        *v = json!("foreign");
        assert_eq!(
            verify::pod(&plan, &changed, None).unwrap_err(),
            Error::IdentityMismatch
        );
    }
    let mut changed = source;
    changed["metadata"]["resourceVersion"] = Value::Null;
    assert_eq!(
        verify::pod(&plan, &changed, None).unwrap_err(),
        Error::InvalidResponse
    );
}

#[test]
fn admission_cannot_inject_sidecars_mounts_or_weaken_security() {
    let plan = plan();
    let mutations = [
        ("/spec/hostNetwork", json!(true)),
        ("/spec/runtimeClassName", json!("runc")),
        ("/spec/automountServiceAccountToken", json!(true)),
        (
            "/spec/containers/0/securityContext/allowPrivilegeEscalation",
            json!(true),
        ),
        (
            "/spec/containers/0/securityContext/readOnlyRootFilesystem",
            json!(false),
        ),
        ("/spec/containers/0/command", json!(["changed"])),
        (
            "/spec/volumes",
            json!([{"name":"host","hostPath":{"path":"/"}}]),
        ),
    ];
    for (path, value) in mutations {
        let mut changed = pod(&plan);
        *changed.pointer_mut(path).unwrap() = value;
        assert_eq!(
            verify::pod(&plan, &changed, None).unwrap_err(),
            Error::IdentityMismatch,
            "{path}"
        );
    }
    for field in [
        "initContainers",
        "ephemeralContainers",
        "dnsConfig",
        "hostAliases",
    ] {
        let mut changed = pod(&plan);
        changed["spec"][field] = json!([]);
        assert_eq!(
            verify::pod(&plan, &changed, None).unwrap_err(),
            Error::IdentityMismatch
        );
    }
    let mut changed = pod(&plan);
    changed["spec"]["containers"][0]["env"] = json!([]);
    assert_eq!(
        verify::pod(&plan, &changed, None).unwrap_err(),
        Error::IdentityMismatch
    );
    let mut changed = pod(&plan);
    let sidecar = changed["spec"]["containers"][0].clone();
    changed["spec"]["containers"]
        .as_array_mut()
        .unwrap()
        .push(sidecar);
    assert_eq!(
        verify::pod(&plan, &changed, None).unwrap_err(),
        Error::IdentityMismatch
    );
}

#[test]
fn known_api_defaults_and_omitted_false_values_are_safe_but_unknown_fields_fail() {
    let plan = plan();
    let mut p = pod(&plan);
    p["spec"].as_object_mut().unwrap().remove("hostNetwork");
    p["spec"]["dnsPolicy"] = json!("ClusterFirst");
    p["spec"]["serviceAccountName"] = json!("default");
    p["spec"]["nodeName"] = json!("node-1");
    p["spec"]["containers"][0]["terminationMessagePolicy"] = json!("File");
    assert!(verify::pod(&plan, &p, None).is_ok());
    p["spec"]["serviceAccountName"] = json!("admin");
    assert_eq!(
        verify::pod(&plan, &p, None).unwrap_err(),
        Error::IdentityMismatch
    );
}

#[test]
fn omitted_protections_and_unexpected_runtime_annotations_are_rejected() {
    let plan = plan();
    for key in ["automountServiceAccountToken", "enableServiceLinks"] {
        let mut p = pod(&plan);
        p["spec"].as_object_mut().unwrap().remove(key);
        assert_eq!(
            verify::pod(&plan, &p, None).unwrap_err(),
            Error::IdentityMismatch
        );
    }
    let mut p = pod(&plan);
    p["spec"]["containers"][0]["securityContext"]
        .as_object_mut()
        .unwrap()
        .remove("allowPrivilegeEscalation");
    assert_eq!(
        verify::pod(&plan, &p, None).unwrap_err(),
        Error::IdentityMismatch
    );
    let mut p = pod(&plan);
    p["metadata"]["annotations"]["dev.gvisor.spec.mount.tmp.options"] = json!("rw,shared");
    assert_eq!(
        verify::pod(&plan, &p, None).unwrap_err(),
        Error::IdentityMismatch
    );
}

#[test]
fn namespace_runtime_and_deny_policy_are_bound_to_actual_uids() {
    let d = deployment();
    let original = prerequisites();
    assert!(verify::deployment(&d, &original[0], &original[1], &original[2]).is_ok());
    for index in 0..3 {
        let mut changed = original.clone();
        if index == 2 {
            changed[index]["items"][0]["metadata"]["uid"] = json!("replacement");
        } else {
            changed[index]["metadata"]["uid"] = json!("replacement");
        }
        assert_eq!(
            verify::deployment(&d, &changed[0], &changed[1], &changed[2]),
            Err(Error::PreconditionFailed)
        );
    }
    let mut changed = original.clone();
    changed[1]["handler"] = json!("runc");
    assert_eq!(
        verify::deployment(&d, &changed[0], &changed[1], &changed[2]),
        Err(Error::PreconditionFailed)
    );
    let mut changed = original.clone();
    changed[2]["items"].as_array_mut().unwrap().push(json!({}));
    assert_eq!(
        verify::deployment(&d, &changed[0], &changed[1], &changed[2]),
        Err(Error::PreconditionFailed)
    );
    let mut changed = original;
    changed[2]["items"][0]["spec"]["egress"] = json!([{}]);
    assert_eq!(
        verify::deployment(&d, &changed[0], &changed[1], &changed[2]),
        Err(Error::PreconditionFailed)
    );
}

type Captured = Arc<Mutex<Vec<(String, Value)>>>;
enum Reply {
    Json(u16, Value),
    Drop,
    Raw(&'static str),
    Upgrade(Box<dyn FnOnce(std::net::TcpStream, String) + Send>),
}

fn fixture(replies: Vec<Reply>) -> (Client, Captured, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    listener.set_nonblocking(true).unwrap();
    let captured: Captured = Arc::default();
    let copy = captured.clone();
    let handle = thread::spawn(move || {
        let mut upgrades = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut buffer = [0u8; 2048];
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|s| s.parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while bytes.len() < header_end + length {
                let mut buffer = [0; 2048];
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let first = String::from_utf8_lossy(&bytes[..header_end])
                .lines()
                .next()
                .unwrap()
                .to_owned();
            let body = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
            };
            copy.lock().unwrap().push((first, body));
            match reply {
                Reply::Upgrade(script) => {
                    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    upgrades.push(thread::spawn(move || script(stream, headers)));
                }
                Reply::Drop => (),
                Reply::Raw(raw) => {
                    let _ = stream.write_all(raw.as_bytes());
                }
                Reply::Json(status, value) => {
                    let body = serde_json::to_vec(&value).unwrap();
                    let header = format!(
                        "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                }
            }
        }
        for handle in upgrades {
            handle.join().unwrap();
        }
    });
    // This HTTP constructor exists only in unit tests; production Client::new requires TLS.
    let tls = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth();
    let http = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    (
        Client {
            http,
            endpoint,
            deployment: deployment(),
        },
        captured,
        handle,
    )
}

fn probe_replies() -> Vec<Reply> {
    prerequisites()
        .into_iter()
        .map(|v| Reply::Json(200, v))
        .collect()
}

#[tokio::test]
async fn lost_create_ack_is_observed_by_same_identity_without_repeating_post() {
    let plan = plan();
    let mut replies = probe_replies();
    replies.push(Reply::Drop);
    replies.extend(probe_replies());
    replies.push(Reply::Json(200, pod(&plan)));
    let (client, captured, handle) = fixture(replies);
    assert_eq!(
        client.create(&plan).await.unwrap_err(),
        Error::MutationUnconfirmed
    );
    assert_eq!(
        client.observe(&plan, None).await.unwrap().unwrap().uid(),
        "pod-uid"
    );
    handle.join().unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(r, _)| r.starts_with("POST "))
            .count(),
        1
    );
    assert_eq!(requests[3].1, plan.manifest());
}

#[tokio::test]
async fn creation_conflicts_and_mutated_success_do_not_trigger_replacement() {
    for (status, body, expected) in [
        (409, json!({}), Error::ExistingObject),
        (201, json!({}), Error::MutationUnconfirmed),
    ] {
        let mut replies = probe_replies();
        replies.push(Reply::Json(status, body));
        let (client, requests, handle) = fixture(replies);
        assert_eq!(client.create(&plan()).await.unwrap_err(), expected);
        handle.join().unwrap();
        assert_eq!(requests.lock().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn failed_preflight_performs_no_mutation() {
    let mut prerequisites = prerequisites();
    prerequisites[1]["handler"] = json!("runc");
    let (client, requests, handle) = fixture(
        prerequisites
            .into_iter()
            .map(|v| Reply::Json(200, v))
            .collect(),
    );
    assert_eq!(
        client.create(&plan()).await.unwrap_err(),
        Error::PreconditionFailed
    );
    handle.join().unwrap();
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|(r, _)| r.starts_with("GET "))
    );
}

#[tokio::test]
async fn deletion_is_uid_and_version_conditional_and_absence_is_only_api_absence() {
    let plan = plan();
    let observed = verify::pod(&plan, &pod(&plan), None).unwrap();
    let (client, requests, handle) = fixture(vec![
        Reply::Json(409, json!({})),
        Reply::Json(404, json!({})),
    ]);
    assert_eq!(
        client.delete(&plan, &observed).await.unwrap_err(),
        Error::PreconditionFailed
    );
    assert_eq!(
        client.delete(&plan, &observed).await.unwrap(),
        DeleteOutcome::AlreadyAbsent
    );
    handle.join().unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests[0].1["preconditions"],
        json!({"uid":"pod-uid","resourceVersion":"42"})
    );
    assert_eq!(requests[0].1["gracePeriodSeconds"], 30);
}

#[tokio::test]
async fn transport_rejects_redirects_large_responses_and_redacts_error_bodies() {
    let (client, requests, handle) = fixture(vec![
        Reply::Raw(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ),
        Reply::Raw("HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n"),
        Reply::Json(403, json!({"message":"private-secret-value"})),
    ]);
    for error in [
        Error::ApiRejected,
        Error::ResponseLimit,
        Error::AccessDenied,
    ] {
        let actual = client
            .request(reqwest::Method::GET, "/test", None)
            .await
            .unwrap_err();
        assert_eq!(actual, error);
        assert!(!format!("{actual:?} {actual}").contains("private-secret"));
    }
    handle.join().unwrap();
    assert_eq!(requests.lock().unwrap().len(), 3);
}

#[test]
fn production_configuration_rejects_insecure_endpoints_ambient_auth_and_empty_ca() {
    for endpoint in [
        "http://127.0.0.1/",
        "https://user:secret@api/",
        "https://api/prefix",
        "https://api/?token=secret",
        "https://api/#secret",
    ] {
        assert!(matches!(
            Client::new(endpoint, b"invalid", "token", deployment()),
            Err(Error::InvalidConfiguration)
        ));
    }
    assert!(matches!(
        Client::new("https://api/", b"", "token", deployment()),
        Err(Error::InvalidConfiguration)
    ));
    assert!(matches!(
        Client::new(
            "https://api/",
            b"invalid",
            "token\r\nHeader: injection",
            deployment()
        ),
        Err(Error::InvalidConfiguration)
    ));
}

fn storage_binding() -> crate::volume::StorageClassBinding {
    crate::volume::StorageClassBinding {
        reference: "juicefs".into(),
        name: "ac-juicefs".into(),
        uid: "sc-uid".into(),
        driver_uid: "driver-uid".into(),
        secret_name: "juicefs-secret".into(),
        secret_namespace: "kube-system".into(),
    }
}
fn volume_plan() -> crate::volume::VolumePlan {
    let doc = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"volume"},"spec":{"volumes":[{"name":"data","storageClass":"juicefs","quotaBytes":1073741824u64,"reclaimPolicy":"Retain"}]}});
    let validated = validate_bytes(&serde_json::to_vec(&doc).unwrap(), Format::Json).unwrap();
    crate::volume::VolumePlan::new(
        &validated,
        "data",
        crate::volume::VolumeIdentity {
            organization: "org_a".into(),
            resource_id: "res_a".into(),
            revision: 1,
            step_id: "step_a".into(),
            spec_digest: format!("sha256:{}", "a".repeat(64)),
        },
        "ac-test",
        storage_binding(),
    )
    .unwrap()
}
fn storage_prerequisites() -> Vec<Value> {
    let mut values = prerequisites();
    values.push(json!({"metadata":{"name":"ac-juicefs","uid":"sc-uid"},"provisioner":"csi.juicefs.com","reclaimPolicy":"Retain","volumeBindingMode":"Immediate","mountOptions":["writeback=false"],"parameters":{
        "csi.storage.k8s.io/fstype":"juicefs","csi.storage.k8s.io/provisioner-secret-name":"juicefs-secret","csi.storage.k8s.io/provisioner-secret-namespace":"kube-system","csi.storage.k8s.io/node-publish-secret-name":"juicefs-secret","csi.storage.k8s.io/node-publish-secret-namespace":"kube-system"}}));
    values.push(json!({"metadata":{"name":"csi.juicefs.com","uid":"driver-uid"},"spec":{"attachRequired":false,"volumeLifecycleModes":["Persistent"]}}));
    values
}
fn pvc_fixture(plan: &crate::volume::VolumePlan, bound: bool) -> Value {
    let mut pvc = plan.manifest();
    pvc["metadata"]["uid"] = json!("pvc-uid");
    if bound {
        pvc["spec"]["volumeName"] = json!("pvc-volume");
        pvc["status"] = json!({"phase":"Bound","capacity":{"storage":"1Gi"}});
    } else {
        pvc["status"] = json!({"phase":"Pending"});
    }
    pvc
}
fn pv_fixture(plan: &crate::volume::VolumePlan) -> Value {
    json!({"apiVersion":"v1","kind":"PersistentVolume","metadata":{"name":"pvc-volume","uid":"pv-uid"},"status":{"phase":"Bound"},
        "spec":{"capacity":{"storage":"1Gi"},"accessModes":["ReadWriteMany"],"volumeMode":"Filesystem","persistentVolumeReclaimPolicy":"Retain","storageClassName":"ac-juicefs","mountOptions":["writeback=false"],
            "claimRef":{"name":plan.name(),"namespace":"ac-test","uid":"pvc-uid"},"csi":{"driver":"csi.juicefs.com","fsType":"juicefs","volumeHandle":"pvc-volume","volumeAttributes":{"subPath":"pvc-volume","capacity":"1073741824","juicefs/controller-quota-set":"true","storage.kubernetes.io/csiProvisionerIdentity":"1791548452146-883-csi.juicefs.com"},"nodePublishSecretRef":{"name":"juicefs-secret","namespace":"kube-system"}}}})
}

#[test]
fn volume_preflight_rejects_replaced_driver_async_upload_and_destructive_reclaim() {
    let s = storage_binding();
    let original = storage_prerequisites();
    assert!(crate::volume::verify_storage(&s, &original[3], &original[4]).is_ok());
    for (path, value) in [
        ("/metadata/uid", json!("replacement")),
        ("/reclaimPolicy", json!("Delete")),
        ("/volumeBindingMode", json!("WaitForFirstConsumer")),
        ("/mountOptions", json!(["writeback"])),
    ] {
        let mut sc = original[3].clone();
        *sc.pointer_mut(path).unwrap() = value;
        assert_eq!(
            crate::volume::verify_storage(&s, &sc, &original[4]),
            Err(Error::PreconditionFailed)
        );
    }
    let mut driver = original[4].clone();
    driver["metadata"]["uid"] = json!("replacement");
    assert_eq!(
        crate::volume::verify_storage(&s, &original[3], &driver),
        Err(Error::PreconditionFailed)
    );
}

#[test]
fn pending_claim_identity_is_recordable_but_bound_volume_requires_reciprocal_uid() {
    let plan = volume_plan();
    let pending =
        crate::volume::verify_claim(&plan, "namespace-uid", &pvc_fixture(&plan, false), None)
            .unwrap();
    assert_eq!(pending.uid(), "pvc-uid");
    assert!(pending.volume_name().is_none());
    assert_eq!(
        crate::volume::verify_claim(
            &plan,
            "namespace-uid",
            &pvc_fixture(&plan, true),
            Some("old-uid")
        )
        .unwrap_err(),
        Error::IdentityMismatch
    );
    let claim = crate::volume::verify_claim(
        &plan,
        "namespace-uid",
        &pvc_fixture(&plan, true),
        Some("pvc-uid"),
    )
    .unwrap();
    let pv = pv_fixture(&plan);
    assert_eq!(
        crate::volume::verify_volume(&plan, &claim, &pv, None)
            .unwrap()
            .uid(),
        "pv-uid"
    );
    for (path, value) in [
        ("/spec/claimRef/uid", json!("other-claim")),
        ("/spec/csi/driver", json!("other.csi")),
        ("/spec/csi/volumeAttributes/subPath", json!("/")),
        ("/spec/persistentVolumeReclaimPolicy", json!("Delete")),
        ("/spec/csi/nodePublishSecretRef/name", json!("other-secret")),
    ] {
        let mut changed = pv.clone();
        *changed.pointer_mut(path).unwrap() = value;
        assert_eq!(
            crate::volume::verify_volume(&plan, &claim, &changed, None).unwrap_err(),
            Error::IdentityMismatch,
            "{path}"
        );
    }
    assert_eq!(
        crate::volume::verify_volume(&plan, &claim, &pv, Some("old-pv-uid")).unwrap_err(),
        Error::IdentityMismatch
    );
    let mut changed_plan = plan.clone();
    changed_plan.pvc["metadata"]["annotations"]["agent-computer.io/volume-binding"] =
        json!("another-admitted-step");
    assert_eq!(
        crate::volume::verify_volume(&changed_plan, &claim, &pv, None).unwrap_err(),
        Error::IdentityMismatch
    );
    for key in ["subdir", "mountOptions", "pathPattern", "secretFinalizer"] {
        let mut changed = pv.clone();
        changed["spec"]["csi"]["volumeAttributes"][key] = json!("unexpected");
        assert_eq!(
            crate::volume::verify_volume(&plan, &claim, &changed, None).unwrap_err(),
            Error::IdentityMismatch
        );
    }
}

#[test]
fn claim_data_sources_and_capacity_mismatch_cannot_be_adopted() {
    let plan = volume_plan();
    let mut pvc = pvc_fixture(&plan, true);
    pvc["spec"]["dataSource"] = json!({"name":"private-snapshot"});
    assert_eq!(
        crate::volume::verify_claim(&plan, "namespace-uid", &pvc, None).unwrap_err(),
        Error::IdentityMismatch
    );
    let mut pvc = pvc_fixture(&plan, true);
    pvc["status"]["capacity"]["storage"] = json!("512Mi");
    assert_eq!(
        crate::volume::verify_claim(&plan, "namespace-uid", &pvc, None).unwrap_err(),
        Error::IdentityMismatch
    );
    assert_eq!(
        crate::volume::quantity(&json!("18446744073709551615Ei")),
        None
    );
    assert_eq!(crate::volume::quantity(&json!("1.5Gi")), None);
    assert_eq!(crate::volume::quantity(&json!("1024Mi")), Some(1 << 30));
}

#[tokio::test]
async fn volume_creation_ack_loss_is_observed_without_second_post() {
    let plan = volume_plan();
    let mut replies: Vec<_> = storage_prerequisites()
        .into_iter()
        .map(|v| Reply::Json(200, v))
        .collect();
    replies.push(Reply::Drop);
    replies.extend(
        storage_prerequisites()
            .into_iter()
            .map(|v| Reply::Json(200, v)),
    );
    replies.push(Reply::Json(200, pvc_fixture(&plan, true)));
    replies.push(Reply::Json(200, pv_fixture(&plan)));
    let (client, requests, handle) = fixture(replies);
    assert_eq!(
        client.create_volume(&plan).await.unwrap_err(),
        Error::MutationUnconfirmed
    );
    let claim = client
        .observe_volume_claim(&plan, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        client
            .observe_bound_volume(&plan, &claim, None)
            .await
            .unwrap()
            .unwrap()
            .uid(),
        "pv-uid"
    );
    handle.join().unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(r, _)| r.starts_with("POST "))
            .count(),
        1
    );
}

#[test]
fn volume_claim_metadata_cannot_override_csi_mount_configuration() {
    let plan = volume_plan();
    let mut pvc = pvc_fixture(&plan, true);
    for (key, value) in [
        ("pv.kubernetes.io/bind-completed", "yes"),
        ("pv.kubernetes.io/bound-by-controller", "yes"),
        (
            "volume.beta.kubernetes.io/storage-provisioner",
            "csi.juicefs.com",
        ),
        (
            "volume.kubernetes.io/storage-provisioner",
            "csi.juicefs.com",
        ),
    ] {
        pvc["metadata"]["annotations"][key] = json!(value);
    }
    assert!(crate::volume::verify_claim(&plan, "namespace-uid", &pvc, Some("pvc-uid")).is_ok());
    for (section, key, value) in [
        ("annotations", "juicefs/mount-memory-limit", "0"),
        ("annotations", "juicefs/mount-cpu-limit", "0"),
        (
            "annotations",
            "volume.kubernetes.io/storage-provisioner",
            "another.csi",
        ),
        ("labels", "custom-image", "true"),
    ] {
        let mut changed = pvc.clone();
        changed["metadata"][section][key] = json!(value);
        assert_eq!(
            crate::volume::verify_claim(&plan, "namespace-uid", &changed, None).unwrap_err(),
            Error::IdentityMismatch,
            "{section}/{key}"
        );
    }
    pvc["spec"]["resources"]["limits"] = json!({"storage":"2Gi"});
    assert_eq!(
        crate::volume::verify_claim(&plan, "namespace-uid", &pvc, None).unwrap_err(),
        Error::IdentityMismatch
    );
}
