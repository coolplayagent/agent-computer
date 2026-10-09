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

fn plan() -> EphemeralSandboxPlan {
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

fn pod(plan: &EphemeralSandboxPlan) -> Value {
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
