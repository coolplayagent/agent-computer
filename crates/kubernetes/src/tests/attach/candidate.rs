use super::*;
use agent_computer_storage::{Manifest, PrepareRequest, Prepared};

fn prepared() -> (PrepareRequest, Prepared) {
    let request = PrepareRequest {
        organization: "org_a".into(),
        volume_uid: "pvc-uid".into(),
        workspace: "workspace_a".into(),
        candidate: "candidate_a".into(),
        computer: "computer_a".into(),
        generation: 1,
        quota_bytes: 1 << 30,
        manifest_digest: Manifest::default().digest().unwrap(),
        manifest: Manifest::default(),
    };
    let prepared = Prepared {
        version: 1,
        request_digest: request.binding_digest("pvc-volume", 1000, 1000).unwrap(),
        filesystem_uuid: "filesystem-uuid".into(),
        volume_uid: request.volume_uid.clone(),
        path_ref: request.path_ref(),
        data_inode: 123,
        manifest_digest: request.manifest_digest.clone(),
        quota_bytes: request.quota_bytes,
    };
    (request, prepared)
}
fn mount(request: &PrepareRequest, prepared: &Prepared) -> Result<CandidateMount> {
    CandidateMount::new(
        volume_plan(),
        "namespace-uid",
        "pv-uid",
        "pvc-volume",
        request,
        prepared,
    )
}
fn candidate_plan() -> StartupSandboxPlan {
    let (request, prepared) = prepared();
    StartupSandboxPlan::with_candidate(
        &definition(true),
        "sandbox",
        identity(),
        "ac-test",
        &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
        startup_plan().bootstrap().clone(),
        mount(&request, &prepared).unwrap(),
    )
    .unwrap()
}
fn storage(pv: Value) -> Vec<Reply> {
    let mut r: Vec<_> = storage_prerequisites()
        .into_iter()
        .map(|v| Reply::Json(200, v))
        .collect();
    r.push(Reply::Json(200, pvc_fixture(&volume_plan(), true)));
    r.push(Reply::Json(200, pv));
    r
}
fn checked_running(plan: &StartupSandboxPlan, pv: Value) -> Vec<Reply> {
    let mut r = observation(running(plan));
    r.extend(storage(pv));
    r
}
#[test]
fn candidate_receipt_and_fixed_generation_must_match_preparation() {
    let (request, receipt) = prepared();
    mount(&request, &receipt).unwrap();
    for field in [
        "version",
        "request_digest",
        "volume_uid",
        "path_ref",
        "data_inode",
        "manifest_digest",
        "quota_bytes",
        "filesystem_uuid",
    ] {
        let mut changed = serde_json::to_value(&receipt).unwrap();
        changed[field] = if ["version", "data_inode", "quota_bytes"].contains(&field) {
            json!(0)
        } else {
            json!("")
        };
        assert!(
            mount(&request, &serde_json::from_value(changed).unwrap()).is_err(),
            "{field}"
        );
    }
    for field in ["organization", "workspace", "candidate", "computer"] {
        let mut changed = serde_json::to_value(&request).unwrap();
        changed[field] = json!("another");
        assert!(
            mount(&serde_json::from_value(changed).unwrap(), &receipt).is_err(),
            "{field}"
        );
    }
    let mut changed = request.clone();
    changed.generation += 1;
    assert!(mount(&changed, &receipt).is_err());
    let mut owner = identity();
    owner.computer = "foreign".into();
    assert_eq!(
        StartupSandboxPlan::with_candidate(
            &definition(true),
            "sandbox",
            owner,
            "ac-test",
            &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            startup_plan().bootstrap().clone(),
            mount(&request, &receipt).unwrap()
        )
        .unwrap_err(),
        Error::IdentityMismatch
    );
}
#[test]
fn candidate_only_mounts_the_declared_workspace_data_leaf() {
    let p = candidate_plan();
    let m = p.pod_plan().manifest();
    assert!(m["spec"]["securityContext"]["fsGroup"].is_null());
    assert_eq!(
        m["spec"]["volumes"][2],
        json!({"name":"workspace","persistentVolumeClaim":{"claimName":volume_plan().name(),"readOnly":false}})
    );
    assert_eq!(
        m["spec"]["containers"][0]["volumeMounts"][2]["subPath"],
        prepared().1.path_ref
    );
    let (r, p) = prepared();
    let base = serde_json::to_value(definition(true).document()).unwrap();
    for (field, value) in [
        ("workspaceRef", json!("id:other")),
        ("path", json!("/")),
        ("readOnly", json!(true)),
    ] {
        let mut doc = base.clone();
        doc["spec"]["sandboxes"][0]["mounts"][0][field] = value;
        let Ok(d) = validate_bytes(&serde_json::to_vec(&doc).unwrap(), Format::Json) else {
            continue;
        };
        assert_eq!(
            StartupSandboxPlan::with_candidate(
                &d,
                "sandbox",
                identity(),
                "ac-test",
                &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
                startup_plan().bootstrap().clone(),
                mount(&r, &p).unwrap()
            )
            .unwrap_err(),
            Error::UnsupportedSandbox
        );
    }
    assert!(
        StartupSandboxPlan::with_candidate(
            &definition(false),
            "sandbox",
            identity(),
            "ac-test",
            &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            startup_plan().bootstrap().clone(),
            mount(&r, &p).unwrap()
        )
        .is_err()
    );
}
#[test]
fn candidate_admission_cannot_change_subpath_or_reintroduce_volume_ownership() {
    let plan = candidate_plan();
    for (pointer, value) in [
        ("/spec/containers/0/volumeMounts/2/subPath", json!(".")),
        ("/spec/containers/0/volumeMounts/2/readOnly", json!(true)),
        (
            "/spec/volumes/2/persistentVolumeClaim/claimName",
            json!("another"),
        ),
    ] {
        let mut p = running(&plan);
        *p.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            verify::pod(plan.pod_plan(), &p, None).unwrap_err(),
            Error::IdentityMismatch
        );
    }
    for field in ["fsGroup", "supplementalGroups"] {
        let mut p = running(&plan);
        p["spec"]["securityContext"][field] = json!(1000);
        assert_eq!(
            verify::pod(plan.pod_plan(), &p, None).unwrap_err(),
            Error::IdentityMismatch
        );
    }
    let mut p = running(&plan);
    p["spec"]["containers"][0]["volumeMounts"][2]
        .as_object_mut()
        .unwrap()
        .remove("readOnly");
    p["spec"]["volumes"][2]["persistentVolumeClaim"]
        .as_object_mut()
        .unwrap()
        .remove("readOnly");
    verify::pod(plan.pod_plan(), &p, None).unwrap();
}
#[test]
fn fenced_candidate_pins_node_and_rejects_direct_mount_substitution() {
    let (request, prepared) = prepared();
    let node = crate::NodeIdentity {
        name: "node-a".into(),
        uid: "node-uid".into(),
        boot_id: "00000000-0000-0000-0000-000000000001".into(),
    };
    let reference = agent_computer_fence::MountReference {
        version: 1,
        instance: "a".repeat(64),
        path: "/var/lib/agent-computer-csi/mounts/execution-a".into(),
        device: 42,
        inode: 1,
        boot_id: node.boot_id.clone(),
        mount_namespace: 12,
        prepared: prepared.clone(),
    };
    let plan = StartupSandboxPlan::with_candidate(
        &definition(true),
        "sandbox",
        identity(),
        "ac-test",
        &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
        startup_plan().bootstrap().clone(),
        mount(&request, &prepared)
            .unwrap()
            .with_fence(reference.clone(), node.clone())
            .unwrap(),
    )
    .unwrap();
    let pod = running(&plan);
    assert_eq!(
        pod["spec"]["volumes"][2]["csi"]["driver"],
        "csi.agent-computer.io"
    );
    assert!(
        pod["spec"]["containers"][0]["volumeMounts"][2]
            .get("subPath")
            .is_none()
    );
    verify::pod(plan.pod_plan(), &pod, None).unwrap();
    for (pointer, value) in [
        ("/spec/nodeName", json!("node-b")),
        (
            "/spec/volumes/2",
            json!({"name":"workspace","hostPath":{"path":"/mnt/backing"}}),
        ),
        (
            "/spec/volumes/2",
            json!({"name":"workspace","persistentVolumeClaim":{"claimName":"pvc-volume"}}),
        ),
        (
            "/spec/volumes/2/csi/volumeAttributes/agent-computer.io~1mount-instance",
            json!("b".repeat(64)),
        ),
        (
            "/spec/containers/0/volumeMounts/2",
            json!({"name":"workspace","mountPath":"/workspace","subPath":"other","readOnly":false}),
        ),
    ] {
        let mut changed = pod.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(
            verify::pod(plan.pod_plan(), &changed, None).is_err(),
            "{pointer}"
        );
    }
    let mut wrong = reference.clone();
    wrong.prepared.data_inode += 1;
    assert!(
        mount(&request, &prepared)
            .unwrap()
            .with_fence(wrong, node.clone())
            .is_err()
    );
    let mut wrong = reference;
    wrong.boot_id = "00000000-0000-0000-0000-000000000002".into();
    assert!(
        mount(&request, &prepared)
            .unwrap()
            .with_fence(wrong, node)
            .is_err()
    );
}
#[tokio::test]
async fn candidate_create_rechecks_claim_and_pv_before_one_post() {
    for changed in [false, true] {
        let plan = candidate_plan();
        let mut pv = pv_fixture(&volume_plan());
        if changed {
            pv["metadata"]["uid"] = json!("replacement");
        }
        let mut replies = storage(pv);
        if !changed {
            replies.push(Reply::Json(201, running(&plan)));
        }
        let (client, captured, server) = fixture(replies);
        let result = client.create(plan.pod_plan()).await;
        if changed {
            assert_eq!(result.unwrap_err(), Error::IdentityMismatch);
        } else {
            result.unwrap();
        }
        server.join().unwrap();
        let requests = captured.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(r, _)| r.starts_with("POST "))
                .count(),
            usize::from(!changed)
        );
    }
}
#[tokio::test]
async fn replaced_volume_at_every_startup_check_prevents_grant() {
    for stage in 0..3 {
        let plan = candidate_plan();
        let pv = pv_fixture(&volume_plan());
        let mut replaced = pv.clone();
        replaced["spec"]["csi"]["volumeHandle"] = json!("other");
        let mut replies = checked_running(
            &plan,
            if stage == 0 {
                replaced.clone()
            } else {
                pv.clone()
            },
        );
        if stage > 0 {
            let p = plan.clone();
            replies.push(Reply::Upgrade(Box::new(move |stream, headers| {
                let mut s = handshake(stream, headers, "v5.channel.k8s.io", None);
                expect_hello(&mut s, &p);
                send_challenge(&mut s, &p);
                assert_no_grant(&mut s);
            })));
            replies.extend(checked_running(
                &plan,
                if stage == 1 { replaced.clone() } else { pv },
            ));
            if stage == 2 {
                replies.extend(checked_running(&plan, replaced));
            }
        }
        let (client, _, server) = fixture(replies);
        let result = client.attach_startup(&plan, &observed(&plan)).await;
        let error = if stage == 2 {
            result.unwrap().run(&grant(&plan)).await.unwrap_err()
        } else {
            result.unwrap_err()
        };
        assert_eq!(error, Error::IdentityMismatch);
        server.join().unwrap();
    }
}
#[tokio::test]
async fn candidate_attach_preserves_single_grant_after_storage_checks() {
    let plan = candidate_plan();
    let pv = pv_fixture(&volume_plan());
    let mut replies = checked_running(&plan, pv.clone());
    let p = plan.clone();
    replies.push(Reply::Upgrade(Box::new(move |stream, headers| {
        let mut s = handshake(stream, headers, "v5.channel.k8s.io", None);
        expect_hello(&mut s, &p);
        send_challenge(&mut s, &p);
        expect_grant(&mut s, &p);
        send_report(&mut s, &report(&p));
        send(&mut s, 3, br#"{"status":"Success"}"#);
    })));
    replies.extend(checked_running(&plan, pv.clone()));
    replies.extend(checked_running(&plan, pv));
    let (client, captured, server) = fixture(replies);
    client
        .attach_startup(&plan, &observed(&plan))
        .await
        .unwrap()
        .run(&grant(&plan))
        .await
        .unwrap();
    server.join().unwrap();
    assert_eq!(captured.lock().unwrap().len(), 34);
}
#[tokio::test]
async fn candidate_conditional_delete_does_not_depend_on_storage_availability() {
    let plan = candidate_plan();
    let (client, captured, server) = fixture(vec![Reply::Json(200, json!({}))]);
    assert_eq!(
        client
            .delete(plan.pod_plan(), &observed(&plan))
            .await
            .unwrap(),
        DeleteOutcome::Requested
    );
    server.join().unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["preconditions"]["uid"], "pod-uid");
}
